mod payment_preimage;

use std::{sync::Arc, time::Duration};

use biscuit_auth::{macros::biscuit, KeyPair};
use ckb_types::{
    packed::{Script, Transaction},
    prelude::*,
};
use fiber_json_types::{
    ChannelSigningStatus, ChannelSigningTransition, InvoiceResult, OpenChannelResult,
};
use fiber_lsp_sdk::{ChannelSigner, MemoryStore, RootSigner};
use hyper::{
    header::{HeaderValue, AUTHORIZATION},
    HeaderMap,
};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
    server::ServerHandle,
};
use ractor::Actor;
use secp256k1::SECP256K1;
use tempfile::{tempdir, TempDir};

use super::lsp_config;
use crate::{
    fiber::{
        channel::{ChannelActorStateStore, ChannelOpenRecordStore, PEER_CHANNEL_RESPONSE_TIMEOUT},
        network::{
            AcceptChannelCommand, FiberActorCommand, FiberActorMessage, NetworkActorMessage,
            OpenChannelCommand, OpenChannelWithExternalFundingCommand,
        },
        payment::SendPaymentCommand,
    },
    fiber_types::{ChannelOpeningStatus, ChannelState, CloseFlags, Hash256, Privkey},
    lsp::{
        BiscuitTokenIssuer, FiberTenantRuntimeFactory, LspService, LspServiceArgs,
        LspServiceMessage, TenantId,
    },
    rpc::{
        channel::{
            to_rpc_channel_open_signer_material, GetChannelSigningStatusParams,
            GetChannelSigningStatusResult, OpenChannelWithExternalFundingParams,
            SubmitChannelSignatureParams, SubmitChannelSignatureResult,
            SubmitSignedFundingTxParams, SubmitSignedFundingTxResult,
        },
        lsp::{
            GetLspTenantRegistryNonceParams, GetLspTenantRegistryNonceResult, LspServiceStatus,
            LspTenantParams, LspTenantRuntimeStatus, RegisterLspTenantParams,
            RegisterLspTenantResult,
        },
        server::start_rpc,
        watchtower::{
            CreateWatchChannelParams, GetWatchtowerSigningStatusParams,
            GetWatchtowerSigningStatusResult, SubmitWatchtowerSignatureParams,
            SubmitWatchtowerSignatureResult,
        },
    },
    store::NodeNamespace,
    tests::{
        establish_channel_between_nodes, gen_rpc_config, get_test_root_actor, init_tracing,
        wait_until_async_timeout, wait_until_node_supports_trampoline_routing, ChannelParameters,
        NetworkNode, HUGE_CKB_AMOUNT,
    },
};

fn authenticated_client(rpc_addr: std::net::SocketAddr, token: &str) -> HttpClient {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("valid authorization header"),
    );
    HttpClientBuilder::default()
        .set_headers(headers)
        .build(format!("http://{rpc_addr}"))
        .expect("build authenticated LSP RPC client")
}

async fn register_root_signer_tenant(
    client: &HttpClient,
    root_signer_key: &Privkey,
) -> Result<RegisterLspTenantResult, jsonrpsee::core::ClientError> {
    let root_signer_pubkey = root_signer_key.pubkey();
    let challenge: GetLspTenantRegistryNonceResult = client
        .request(
            "lsp_get_tenant_registry_nonce",
            rpc_params![GetLspTenantRegistryNonceParams {
                root_signer_pubkey: root_signer_pubkey.into(),
            }],
        )
        .await
        .expect("issue tenant registration nonce");
    let lsp_node_id = crate::fiber_types::Pubkey::from_slice(&challenge.lsp_node_id.0)
        .expect("valid LSP node id");
    let payload = crate::fiber_types::TenantRegistryPayload::new(
        lsp_node_id,
        root_signer_pubkey,
        challenge.nonce.0,
    );
    let signature = SECP256K1.sign_ecdsa(
        &secp256k1::Message::from_digest(payload.digest()),
        &root_signer_key.0,
    );
    client
        .request(
            "lsp_register_tenant",
            rpc_params![RegisterLspTenantParams {
                root_signer_pubkey: root_signer_pubkey.into(),
                nonce: challenge.nonce,
                signature: hex::encode(signature.serialize_compact()),
            }],
        )
        .await
}

fn mock_sign_external_funding_tx(unsigned_tx: &Transaction) -> Transaction {
    unsigned_tx
        .as_advanced_builder()
        .set_witnesses(vec![ckb_types::packed::Bytes::default()])
        .build()
        .data()
}

struct TestLspFixture {
    public_t: NetworkNode,
    rpc_addr: std::net::SocketAddr,
    admin_client: HttpClient,
    _rpc_handle: ServerHandle,
    _lsp_actor: ractor::ActorRef<LspServiceMessage>,
    _root: TempDir,
}

impl TestLspFixture {
    async fn new(name: &str) -> Self {
        init_tracing();
        let public_t = NetworkNode::new_n_interconnected_nodes_with_config(1, |index| {
            crate::tests::NetworkNodeConfigBuilder::new()
                .node_name(Some(format!("{name}-{index}")))
                .base_dir_prefix(&format!("{name}-{index}-"))
                .fiber_config_updater(|config| {
                    config.auto_accept_channel_ckb_funding_amount = Some(100_000_000_000);
                })
                .build()
        })
        .await
        .pop()
        .expect("one node");
        let root = tempdir().expect("temporary LSP directory");
        let config = lsp_config(root.path().join("lsp"));
        let root_actor = get_test_root_actor().await;
        let runtime_factory = Arc::new(FiberTenantRuntimeFactory::new(
            config.clone(),
            public_t.fiber_config.clone(),
            public_t.chain_client.clone(),
            public_t.chain_actor.clone(),
            public_t.network_actor.clone(),
            public_t.store.clone(),
            root_actor.get_cell(),
            Script::default(),
        ));
        let biscuit_root = KeyPair::new();
        let token_issuer = BiscuitTokenIssuer::from_private_key(
            &biscuit_root.private().to_prefixed_string(),
            &biscuit_root.public().to_string(),
        )
        .expect("configure test Biscuit issuer");
        let lsp_actor = Actor::spawn_linked(
            None,
            LspService,
            LspServiceArgs {
                config,
                public_node_id: public_t.pubkey,
                public_network_actor: public_t.network_actor.clone(),
                store: public_t.store.namespaced(NodeNamespace::lsp_metadata()),
                runtime_factory,
                signing_key: public_t.private_key.clone(),
                token_issuer,
                watchtower_store: public_t.store.clone(),
            },
            root_actor.get_cell(),
        )
        .await
        .expect("start authenticated LSP service")
        .0;
        public_t
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                crate::fiber::network::PublicNetworkCommand::SetLspService(lsp_actor.clone()),
            ))
            .expect("attach LSP service to public node");

        let admin_token = biscuit!(
            r#"
                read("lsp");
                write("lsp");
                read("channels");
                write("channels");
                read("invoices");
                write("invoices");
                read("payments");
                write("payments");
                read("watchtower");
                write("watchtower");
            "#
        )
        .build(&biscuit_root)
        .unwrap()
        .to_base64()
        .unwrap();

        let mut rpc_config = gen_rpc_config();
        rpc_config.biscuit_public_key = Some(biscuit_root.public().to_string());
        let biscuit_private_key_path = root.path().join("biscuit-private-key");
        std::fs::write(
            &biscuit_private_key_path,
            biscuit_root.private().to_prefixed_string(),
        )
        .expect("write Biscuit private key");
        rpc_config.biscuit_private_key_path = Some(biscuit_private_key_path);
        rpc_config.enabled_modules = vec![
            "channel".to_string(),
            "invoice".to_string(),
            "lsp".to_string(),
            "payment".to_string(),
            "watchtower".to_string(),
        ];
        let (rpc_handle, rpc_addr) = start_rpc(
            rpc_config,
            None,
            Some(public_t.fiber_config.clone()),
            Some(public_t.network_actor.clone()),
            None,
            Some(lsp_actor.clone()),
            public_t.store.clone(),
            None,
            Some(public_t.network_graph.clone()),
            root_actor.get_cell(),
            None,
            #[cfg(debug_assertions)]
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .await
        .expect("start authenticated LSP RPC server");
        let admin_client = authenticated_client(rpc_addr, &admin_token);

        Self {
            public_t,
            rpc_addr,
            admin_client,
            _rpc_handle: rpc_handle,
            _lsp_actor: lsp_actor,
            _root: root,
        }
    }

    async fn register_tenant(
        &self,
        secret_byte: u8,
    ) -> (TenantId, crate::fiber_types::Pubkey, String, HttpClient) {
        let key = Privkey::from(&[secret_byte; 32]);
        let tenant_id = TenantId::from_root_signer_pubkey(&key.pubkey());
        let reg = register_root_signer_tenant(&self.admin_client, &key)
            .await
            .expect("register tenant");
        let client = authenticated_client(self.rpc_addr, &reg.access_token);
        let tenant_pubkey = crate::fiber_types::Pubkey::from_slice(&reg.tenant.invoice_pubkey.0)
            .expect("valid tenant pubkey");
        (tenant_id, tenant_pubkey, reg.access_token, client)
    }
}

struct ExternalSignerClient {
    client: HttpClient,
    signer: ChannelSigner<MemoryStore>,
}

impl ExternalSignerClient {
    async fn get_signing_status(
        &self,
        channel_id: Hash256,
    ) -> Result<GetChannelSigningStatusResult, jsonrpsee::core::ClientError> {
        self.client
            .request(
                "get_channel_signing_status",
                rpc_params![GetChannelSigningStatusParams {
                    channel_id: channel_id.into(),
                }],
            )
            .await
    }

    async fn pending_status(&self, channel_id: Hash256) -> fiber_json_types::ChannelSigningStatus {
        wait_until_async_timeout(|| async {
            matches!(
                self.get_signing_status(channel_id).await.map(|r| r.status),
                Ok(fiber_json_types::ChannelSigningStatus::SignatureRequired { .. })
            )
        })
        .await;
        self.get_signing_status(channel_id)
            .await
            .expect("pending signing status")
            .status
    }

    async fn finish_opening(&self, host: &NetworkNode, channel_id: Hash256) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !host
                .get_channel_actor_state_unchecked(channel_id)
                .is_some_and(|state| matches!(state.state, ChannelState::ChannelReady))
            {
                self.try_sign_pending(channel_id).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("channel reaches ChannelReady after signing");
    }

    async fn prepare_submission(
        &self,
        channel_id: Hash256,
        status: fiber_json_types::ChannelSigningStatus,
    ) -> SubmitChannelSignatureParams {
        crate::fiber::tests::external_signer_restart::prepare_hosted_signature(
            &self.signer,
            channel_id,
            status,
        )
        .await
    }

    async fn submit(
        &self,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult, jsonrpsee::core::ClientError> {
        self.client
            .request("submit_channel_signature", rpc_params![params])
            .await
    }

    async fn try_sign_pending(&self, channel_id: Hash256) {
        if let Ok(result) = self.get_signing_status(channel_id).await {
            if matches!(
                result.status,
                fiber_json_types::ChannelSigningStatus::SignatureRequired { .. }
            ) {
                let submission = self.prepare_submission(channel_id, result.status).await;
                let applied = self.submit(submission).await.expect("submit signature");
                assert_eq!(applied, SubmitChannelSignatureResult::Applied);
            }
        }
    }
}

async fn open_hosted_external_channel(
    _fixture: &TestLspFixture,
    tenant_client: &HttpClient,
) -> (Hash256, ExternalSignerClient) {
    let created = RootSigner::in_memory()
        .await
        .expect("create in-memory root signer");
    let mut session = fiber_lsp_sdk::HostedSession::new(created.root_signer).with_state(
        fiber_lsp_sdk::HostedSessionState {
            tenant_token: Some("client already carries tenant token".into()),
            ..Default::default()
        },
    );
    let material = session.allocate_pending_channel().await.unwrap();
    let channel_signer = session
        .open_channel(session.pending_channel_key_id().unwrap())
        .await
        .unwrap();

    let request = OpenChannelWithExternalFundingParams {
        pubkey: _fixture.public_t.pubkey.into(),
        public: Some(false),
        funding_amount: 100_000_000_000,
        funding_udt_type_script: None,
        shutdown_script: Script::default().into(),
        funding_lock_script: Script::default().into(),
        funding_lock_script_cell_deps: None,
        commitment_delay_epoch: Some(
            ckb_types::core::EpochNumberWithFraction::new(1, 0, 1)
                .full_value()
                .into(),
        ),
        commitment_fee_rate: Some(1000),
        funding_fee_rate: None,
        tlc_expiry_delta: None,
        tlc_min_value: None,
        tlc_fee_proportional_millionths: None,
        max_tlc_value_in_flight: None,
        max_tlc_number_in_flight: None,
        external_channel_signer: Some(to_rpc_channel_open_signer_material(&material)),
    };
    let open = crate::fiber::tests::external_signer_restart::open_rpc_channel(
        &mut session,
        tenant_client.clone(),
        request,
        Default::default(),
    )
    .await;
    let channel_id: Hash256 = open.channel_id.into();
    let unsigned_tx: Transaction = open.unsigned_funding_tx.clone().into();
    let signed_tx = mock_sign_external_funding_tx(&unsigned_tx);
    let _: SubmitSignedFundingTxResult = tenant_client
        .request(
            "submit_signed_funding_tx",
            rpc_params![SubmitSignedFundingTxParams {
                channel_id: channel_id.into(),
                signed_funding_tx: signed_tx.into(),
            }],
        )
        .await
        .expect("submit_signed_funding_tx over tenant RPC");

    let sdk = ExternalSignerClient {
        client: tenant_client.clone(),
        signer: channel_signer,
    };
    (channel_id, sdk)
}

#[tokio::test]
async fn tenant_cannot_query_or_sign_another_tenants_channel() {
    let fixture = TestLspFixture::new("isolation-channel").await;
    let (_, _, _, other) = fixture.register_tenant(1).await;
    let (_, _, _, owner) = fixture.register_tenant(2).await;
    let (channel_id, sdk) = open_hosted_external_channel(&fixture, &owner).await;
    let before = sdk.pending_status(channel_id).await;
    let submission = sdk.prepare_submission(channel_id, before.clone()).await;
    let err = other
        .request::<GetChannelSigningStatusResult, _>(
            "get_channel_signing_status",
            rpc_params![GetChannelSigningStatusParams {
                channel_id: channel_id.into()
            }],
        )
        .await
        .expect_err("other tenant cannot read channel");
    assert!(err.to_string().contains("not found"), "{err}");
    let err = other
        .request::<SubmitChannelSignatureResult, _>(
            "submit_channel_signature",
            rpc_params![submission.clone()],
        )
        .await
        .expect_err("other tenant cannot sign channel");
    assert!(err.to_string().contains("not found"), "{err}");
    assert_eq!(
        serde_json::to_value(sdk.get_signing_status(channel_id).await.unwrap().status).unwrap(),
        serde_json::to_value(before).unwrap(),
        "cross-tenant attempts leave the request unchanged"
    );
    assert_eq!(
        sdk.submit(submission).await.expect("owner can sign"),
        SubmitChannelSignatureResult::Applied
    );
}

#[tokio::test]
async fn tenant_watchtower_rpc_rejects_secrets_and_cross_tenant_signatures() {
    use crate::watchtower::WatchtowerStore;
    use fiber_json_types::{SubscriptionEvent, SubscriptionOptions};
    use fiber_types::{OnchainKeyPurpose, OnchainSigningContent, WatchtowerExternalSignerState};
    use jsonrpsee::core::client::SubscriptionClientT;

    let fixture = TestLspFixture::new("isolation-watchtower").await;
    let (_, _, other_token, other) = fixture.register_tenant(1).await;
    let (_, owner_pubkey, owner_token, owner) = fixture.register_tenant(2).await;
    let channel_id = Hash256::from([0x77; 32]);
    let key = Privkey::from([10; 32]);
    let mut params = CreateWatchChannelParams {
        commitment_contract_features: fiber_types::CommitmentContractFeatures::LEGACY.into(),
        channel_id: channel_id.into(),
        funding_udt_type_script: None,
        local_settlement_key: Some(key.clone().into()),
        local_settlement_key_pubkey: Some(key.pubkey().into()),
        remote_settlement_key: key.pubkey().into(),
        local_funding_pubkey: key.pubkey().into(),
        remote_funding_pubkey: key.pubkey().into(),
        settlement_data: fiber_json_types::SettlementData {
            local_amount: 1000,
            remote_amount: 1000,
            tlcs: vec![],
        },
    };
    let err = owner
        .request::<(), _>("create_watch_channel", rpc_params![params.clone()])
        .await
        .expect_err("tenant cannot upload a settlement secret");
    assert!(
        err.to_string()
            .contains("tenant token cannot register a local settlement key"),
        "{err}"
    );
    params.local_settlement_key = None;
    owner
        .request::<(), _>("create_watch_channel", rpc_params![params])
        .await
        .expect("public-key registration");

    let owner_ws = authenticated_subscription_client(fixture.rpc_addr, &owner_token).await;
    let other_ws = authenticated_subscription_client(fixture.rpc_addr, &other_token).await;
    let options = SubscriptionOptions::default();
    let mut owner_events = owner_ws
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["watchtower_signing", &options],
            "unsubscribe",
        )
        .await
        .unwrap();
    let mut other_events = other_ws
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["watchtower_signing", &options],
            "unsubscribe",
        )
        .await
        .unwrap();

    // This test checks authenticated routing only; the watchtower actor tests
    // cover generating and applying requests from real settlement transactions.
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: Transaction::default(),
    };
    let request_id = Hash256::from([0x88; 32]);
    let mut external = WatchtowerExternalSignerState::default();
    external
        .pending_requests
        .insert(request_id, content.clone());
    fixture.public_t.store.put_watchtower_signer(
        &crate::lsp::tenant_watchtower_node_id(&owner_pubkey),
        &channel_id,
        fiber_types::WatchtowerSignerState::External(external),
    );
    let live = tokio::time::timeout(Duration::from_secs(5), owner_events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(live, SubscriptionEvent::WatchtowerSigning(result)
        if matches!(result.status, fiber_json_types::WatchtowerSigningStatus::SignatureRequired { request_id: id, .. } if id == request_id.into())));
    let mut recovered = owner_ws
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["watchtower_signing", &options],
            "unsubscribe",
        )
        .await
        .unwrap();
    assert!(matches!(
        recovered.next().await.unwrap().unwrap(),
        SubscriptionEvent::WatchtowerSigning(_)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), recovered.next())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), other_events.next())
            .await
            .is_err()
    );
    let query = GetWatchtowerSigningStatusParams {
        channel_id: channel_id.into(),
    };
    let err = other
        .request::<GetWatchtowerSigningStatusResult, _>(
            "get_watchtower_signing_status",
            rpc_params![query.clone()],
        )
        .await
        .expect_err("other tenant cannot read watch channel");
    assert!(
        err.to_string().contains("watched channel not found"),
        "{err}"
    );
    let submission = SubmitWatchtowerSignatureParams {
        channel_id: channel_id.into(),
        request_id: request_id.into(),
        signature: crate::watchtower::sign_onchain_request(&key, &content)
            .unwrap()
            .to_vec(),
    };
    let err = other
        .request::<SubmitWatchtowerSignatureResult, _>(
            "submit_watchtower_signature",
            rpc_params![submission.clone()],
        )
        .await
        .expect_err("other tenant cannot submit even a valid signature");
    assert!(
        err.to_string().contains("watched channel not found"),
        "{err}"
    );
    let status: GetWatchtowerSigningStatusResult = owner
        .request("get_watchtower_signing_status", rpc_params![query])
        .await
        .expect("owner query");
    assert!(
        matches!(status.status, fiber_json_types::WatchtowerSigningStatus::SignatureRequired { request_id: id, .. } if id == request_id.into())
    );
    assert_eq!(
        owner
            .request::<SubmitWatchtowerSignatureResult, _>(
                "submit_watchtower_signature",
                rpc_params![submission]
            )
            .await
            .expect("owner submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let cleared = tokio::time::timeout(Duration::from_secs(5), owner_events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(cleared, SubscriptionEvent::WatchtowerSigning(result)
        if matches!(result.status, fiber_json_types::WatchtowerSigningStatus::NoSignatureRequired))
    );
    owner_events.unsubscribe().await.unwrap();
    recovered.unsubscribe().await.unwrap();
    other_events.unsubscribe().await.unwrap();
}

#[tokio::test]
async fn tenant_rpc_allowlist_rejects_control_methods_but_reaches_payment_validation() {
    let fixture = TestLspFixture::new("isolation-allowlist").await;
    let (_, _, _, client) = fixture.register_tenant(1).await;
    // Authorization must reject these methods before parameter validation.
    for method in [
        "open_channel",
        "accept_channel",
        "abandon_channel",
        "connect_peer",
        "disconnect_peer",
        "node_info",
        "lsp_get_status",
        "lsp_list_tenants",
        "lsp_register_tenant",
        "lsp_ensure_tenant",
        "lsp_evict_tenant",
    ] {
        let err = client
            .request::<serde_json::Value, _>(method, rpc_params![serde_json::json!({})])
            .await
            .expect_err("tenant method must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("Unauthorized")
                || message.contains("tenant token is not allowed to call"),
            "{method}: {message}"
        );
    }
    let err = client
        .request::<serde_json::Value, _>(
            "send_payment",
            rpc_params![serde_json::json!({
                "target_pubkey": fixture.public_t.pubkey, "amount": "0x0", "keysend": true,
            })],
        )
        .await
        .expect_err("zero-amount payment fails validation");
    assert!(
        err.to_string().contains("amount must be greater than 0"),
        "{err}"
    );

    // Hosted payment validation must use the tenant's configured network too.
    let invoice = crate::invoice::InvoiceBuilder::new(crate::invoice::Currency::Fibb)
        .amount(Some(1_000))
        .payment_preimage(Hash256::from([0x42; 32]))
        .payee_pub_key(fixture.public_t.pubkey.into())
        .build_with_sign(|hash| {
            SECP256K1.sign_ecdsa_recoverable(hash, &fixture.public_t.private_key.0)
        })
        .unwrap();
    let err = client
        .request::<serde_json::Value, _>(
            "send_payment",
            rpc_params![serde_json::json!({ "invoice": invoice.to_string() })],
        )
        .await
        .expect_err("hosted tenant rejects an invoice for a different network");
    assert!(
        err.to_string()
            .contains("invoice currency fibb does not match node currency fibd"),
        "{err}"
    );
}

#[tokio::test]
async fn hosted_tenant_core_rejects_local_channel_creation() {
    let fixture = TestLspFixture::new("tenant-reject-local-channel").await;
    let (tenant_id, _, _, _) = fixture.register_tenant(1).await;
    let context = ractor::call!(fixture._lsp_actor, |reply| {
        crate::lsp::LspServiceMessage::GetTenantRpcContext(tenant_id.clone(), reply)
    })
    .unwrap()
    .unwrap();
    let tenant_store = fixture
        .public_t
        .store
        .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));

    let rejected = ractor::call!(context.fiber_actor, |reply| {
        FiberActorMessage::new_command(FiberActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: fixture.public_t.pubkey,
                funding_amount: 100_000_000_000,
                public: false,
                one_way: false,
                shutdown_script: None,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
            },
            reply,
        ))
    })
    .unwrap();
    assert!(rejected.unwrap_err().contains("external signer material"));

    let rejected = ractor::call!(context.fiber_actor, |reply| {
        FiberActorMessage::new_command(FiberActorCommand::OpenChannelWithExternalFunding(
            OpenChannelWithExternalFundingCommand {
                pubkey: fixture.public_t.pubkey,
                funding_amount: 100_000_000_000,
                public: false,
                shutdown_script: Default::default(),
                funding_lock_script: Default::default(),
                funding_lock_script_cell_deps: vec![],
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
                external_channel_signer: None,
            },
            reply,
        ))
    })
    .unwrap();
    assert!(rejected.unwrap_err().contains("external signer material"));

    // Internal AcceptChannel calls must fail even without an incoming offer.
    let rejected = ractor::call!(context.fiber_actor, |reply| {
        FiberActorMessage::new_command(FiberActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: Hash256::from([0xff; 32]),
                funding_amount: 100_000_000_000,
                shutdown_script: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
                min_tlc_value: None,
                tlc_fee_proportional_millionths: None,
                tlc_expiry_delta: None,
            },
            reply,
        ))
    })
    .unwrap();
    assert!(rejected
        .unwrap_err()
        .contains("do not accept inbound channels"));
    assert!(tenant_store.get_all_channel_states().is_empty());
    assert!(tenant_store.get_channel_open_records().is_empty());
}

#[tokio::test]
async fn hosted_tenant_rejects_inbound_before_and_after_its_own_channel() {
    let fixture = TestLspFixture::new("tenant-reject-inbound").await;
    let (tenant_id, tenant_pubkey, _, client) = fixture.register_tenant(1).await;
    let context = ractor::call!(fixture._lsp_actor, |reply| {
        crate::lsp::LspServiceMessage::GetTenantRpcContext(tenant_id.clone(), reply)
    })
    .unwrap()
    .unwrap();
    let tenant_store = fixture
        .public_t
        .store
        .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
    let mut existing_channel = None;

    for expected_count in [0, 1] {
        if expected_count == 1 {
            let (id, signer) = open_hosted_external_channel(&fixture, &client).await;
            signer.finish_opening(&fixture.public_t, id).await;
            wait_until_async_timeout(|| async {
                tenant_store
                    .get_channel_actor_state(&id)
                    .is_some_and(|channel| channel.state == ChannelState::ChannelReady)
            })
            .await;
            existing_channel = Some(id);
        }
        let records_before = tenant_store.get_channel_open_records().len();
        let opened: OpenChannelResult = fixture
            .admin_client
            .request(
                "open_channel",
                rpc_params![serde_json::json!({
                    "pubkey": tenant_pubkey,
                    "funding_amount": "0x174876e800",
                    "public": false,
                })],
            )
            .await
            .unwrap();
        let rejected_id: Hash256 = opened.temporary_channel_id.into();
        wait_until_async_timeout(|| async {
            fixture
                .public_t
                .store
                .get_channel_open_record(&rejected_id)
                .is_some_and(|record| record.status == ChannelOpeningStatus::Failed)
                && fixture
                    .public_t
                    .get_channel_actor_state_unchecked(rejected_id)
                    .is_none()
        })
        .await;
        let failed = fixture
            .public_t
            .store
            .get_channel_open_record(&rejected_id)
            .unwrap();
        assert!(failed
            .failure_detail
            .unwrap()
            .contains("hosted tenants do not accept inbound channels"));
        assert_eq!(tenant_store.get_all_channel_states().len(), expected_count);
        assert_eq!(
            tenant_store.get_channel_open_records().len(),
            records_before
        );
        let pending = ractor::call!(context.fiber_actor, |reply| {
            FiberActorMessage::new_command(FiberActorCommand::GetPendingAcceptChannels(reply))
        })
        .unwrap()
        .unwrap();
        assert!(pending.is_empty());
        if let Some(id) = existing_channel {
            let channel = tenant_store.get_channel_actor_state(&id).unwrap();
            assert_eq!(channel.state, ChannelState::ChannelReady);
            assert_eq!(channel.get_remote_pubkey(), fixture.public_t.pubkey);
            assert!(channel.signer.is_none());
            assert!(!channel.channel_signer.is_local());
        }
    }
}

#[tokio::test]
async fn pending_signature_survives_cold_queries_and_resumes_after_eviction() {
    let fixture = TestLspFixture::new("hosted-external-evict").await;
    let (tenant_id, tenant_pubkey, _, client) = fixture.register_tenant(1).await;
    let (channel_id, sdk) = open_hosted_external_channel(&fixture, &client).await;
    let before = sdk.pending_status(channel_id).await;
    assert!(!fixture
        .public_t
        .get_channel_actor_state_unchecked(channel_id)
        .is_some_and(|state| matches!(state.state, ChannelState::ChannelReady)));
    let evicted: crate::rpc::lsp::LspTenantStatus = fixture
        .admin_client
        .request(
            "lsp_evict_tenant",
            rpc_params![LspTenantParams {
                tenant_id: tenant_id.as_str().to_string()
            }],
        )
        .await
        .expect("evict");
    assert!(matches!(
        evicted.runtime_status,
        LspTenantRuntimeStatus::Cold
    ));
    // Internal-channel ownership survives eviction and is scoped to Public T.
    assert!(fixture
        .public_t
        .store
        .is_internal_channel(&tenant_pubkey)
        .unwrap());
    assert!(!fixture
        .public_t
        .store
        .is_internal_channel(&fixture.public_t.pubkey)
        .unwrap());
    assert!(!fixture
        .public_t
        .store
        .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()))
        .is_internal_channel(&tenant_pubkey)
        .unwrap());
    let status: LspServiceStatus = fixture
        .admin_client
        .request("lsp_get_status", rpc_params![])
        .await
        .unwrap();
    assert_eq!(status.active_tenants, 0);
    let after = sdk
        .get_signing_status(channel_id)
        .await
        .expect("cold signing status")
        .status;
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    let status: LspServiceStatus = fixture
        .admin_client
        .request("lsp_get_status", rpc_params![])
        .await
        .unwrap();
    assert_eq!(
        status.active_tenants, 0,
        "read-only query must not hydrate the tenant"
    );
    let submission = sdk.prepare_submission(channel_id, after).await;
    assert_eq!(
        sdk.submit(submission)
            .await
            .expect("submit rehydrates tenant"),
        SubmitChannelSignatureResult::Applied
    );
    sdk.finish_opening(&fixture.public_t, channel_id).await;
}

#[tokio::test]
async fn channel_signature_submission_rejects_invalid_input_and_is_idempotent() {
    let fixture = TestLspFixture::new("hosted-external-rejections").await;
    let (_u1_id, _u1_pubkey, _u1_token, u1_client) = fixture.register_tenant(1).await;

    let (channel_id, sdk) = open_hosted_external_channel(&fixture, &u1_client).await;

    let status = sdk.pending_status(channel_id).await;
    let valid_submission = sdk.prepare_submission(channel_id, status).await;

    // 1. Wrong request ID
    let mut wrong_req = valid_submission.clone();
    wrong_req.request_id = Hash256::from([0xee; 32]).into();
    let err = sdk
        .submit(wrong_req)
        .await
        .expect_err("wrong request id must fail");
    assert!(
        err.to_string().contains("request id does not match"),
        "error must state request id does not match, got {err}"
    );

    // 2. Invalid partial signature
    let mut invalid_sig = valid_submission.clone();
    invalid_sig.partial_signature = [1u8; 32];
    let err = sdk
        .submit(invalid_sig)
        .await
        .expect_err("invalid partial signature must fail");
    assert!(
        err.to_string().contains("signature is invalid"),
        "error must state signature is invalid, got {err}"
    );

    // 3. Conflicting next material
    let mut conflicting_mat = valid_submission.clone();
    conflicting_mat
        .next_material
        .as_mut()
        .expect("submission has next material")
        .next_commitment_point = Some(fixture.public_t.pubkey.into());
    let err = sdk
        .submit(conflicting_mat)
        .await
        .expect_err("conflicting next material must fail");
    assert!(
        err.to_string()
            .contains("conflicts with persisted material"),
        "error must state conflicts with persisted material, got {err}"
    );

    // 3a. Missing next material when required by commitment progression
    let mut missing_mat = valid_submission.clone();
    missing_mat.next_material = None;
    let err = sdk
        .submit(missing_mat)
        .await
        .expect_err("missing next material must fail");
    assert!(
        err.to_string().contains("must provide next_material"),
        "error must state must provide next_material, got {err}"
    );

    // 3b. Partial next material: missing commitment point
    let mut missing_point = valid_submission.clone();
    missing_point
        .next_material
        .as_mut()
        .unwrap()
        .next_commitment_point = None;
    let err = sdk
        .submit(missing_point)
        .await
        .expect_err("missing commitment point must fail");
    assert!(
        err.to_string().contains("missing next_commitment_point"),
        "error must state missing next_commitment_point, got {err}"
    );

    // 3c. Partial next material: missing commitment nonce
    let mut missing_c_nonce = valid_submission.clone();
    missing_c_nonce
        .next_material
        .as_mut()
        .unwrap()
        .next_commitment_nonce = None;
    let err = sdk
        .submit(missing_c_nonce)
        .await
        .expect_err("missing commitment nonce must fail");
    assert!(
        err.to_string().contains("missing next_commitment_nonce"),
        "error must state missing next_commitment_nonce, got {err}"
    );

    // 3d. Partial next material: missing revocation nonce
    let mut missing_r_nonce = valid_submission.clone();
    missing_r_nonce
        .next_material
        .as_mut()
        .unwrap()
        .next_revocation_nonce = None;
    let err = sdk
        .submit(missing_r_nonce)
        .await
        .expect_err("missing revocation nonce must fail");
    assert!(
        err.to_string().contains("missing next_revocation_nonce"),
        "error must state missing next_revocation_nonce, got {err}"
    );

    // 3e. Nonce reuse: identical commitment and revocation nonce in same submission
    let mut reused_in_submission = valid_submission.clone();
    let c_nonce = reused_in_submission
        .next_material
        .as_ref()
        .unwrap()
        .next_commitment_nonce
        .clone();
    reused_in_submission
        .next_material
        .as_mut()
        .unwrap()
        .next_revocation_nonce = c_nonce;
    let err = sdk
        .submit(reused_in_submission)
        .await
        .expect_err("identical commitment and revocation nonces must fail");
    assert!(
        err.to_string().contains("must be distinct"),
        "error must state nonces must be distinct, got {err}"
    );

    // 4. Valid submission succeeds
    let applied = sdk
        .submit(valid_submission.clone())
        .await
        .expect("valid submission must succeed");
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);
    assert_eq!(
        sdk.submit(valid_submission)
            .await
            .expect("retry submission"),
        SubmitChannelSignatureResult::AlreadyApplied
    );
}

#[tokio::test]
async fn host_force_closes_non_hosted_peer_missing_ack() {
    let mut fixture = TestLspFixture::new("host-normal-peer-ack").await;
    let mut peer = NetworkNode::new_with_node_name("host-normal-peer").await;
    peer.connect_to(&mut fixture.public_t).await;
    let (channel_id, _) = establish_channel_between_nodes(
        &mut fixture.public_t,
        &mut peer,
        ChannelParameters {
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    // Hold the sender's CommitmentSigned, so the peer never produces an ACK
    // or advances nonces. Holding only its ACK would reorder later messages.
    fixture
        .public_t
        .hold_next_fiber_messages(
            peer.pubkey,
            channel_id,
            crate::fiber::network::TestFiberMessageKind::CommitmentSigned,
            1,
        )
        .await;
    fixture
        .public_t
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(peer.pubkey),
            amount: Some(1_000),
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    fixture.public_t.wait_for_held_fiber_messages(1).await;
    let started = std::time::Instant::now();
    assert!(fixture
        .public_t
        .get_channel_actor_state(channel_id)
        .is_waiting_tlc_ack());
    tokio::time::timeout(
        Duration::from_millis(PEER_CHANNEL_RESPONSE_TIMEOUT) + Duration::from_secs(10),
        async {
            loop {
                if matches!(
                    fixture.public_t.get_channel_actor_state(channel_id).state,
                    ChannelState::Closed(flags)
                        if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        },
    )
    .await
    .expect("ordinary peers retain the ACK timeout even when Public T runs an LSP");
    assert!(started.elapsed() >= Duration::from_millis(PEER_CHANNEL_RESPONSE_TIMEOUT - 500));
}

#[tokio::test]
async fn hosted_external_signer_inbound_payment_requires_signing_during_commitment() {
    let mut fixture = TestLspFixture::new("hosted-inbound-offline-signer").await;
    let mut payer = NetworkNode::new_with_node_name("hosted-inbound-payer").await;
    payer.connect_to(&mut fixture.public_t).await;
    // T initiates so its auto-accept configuration cannot race manual acceptance.
    establish_channel_between_nodes(
        &mut fixture.public_t,
        &mut payer,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_until_node_supports_trampoline_routing(&payer, &fixture.public_t).await;

    let (tenant_id, _, _, client) = fixture.register_tenant(1).await;
    let (channel_id, signer) = open_hosted_external_channel(&fixture, &client).await;
    signer.finish_opening(&fixture.public_t, channel_id).await;
    let tenant_store = fixture
        .public_t
        .store
        .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
    // Do not process signing requests until after the host ACK timeout has elapsed.

    let payment_preimage = Hash256::from([0xab; 32]);
    let invoice: InvoiceResult = client
        .request(
            "new_invoice",
            rpc_params![serde_json::json!({
                "amount": "0x3e8",
                "currency": "Fibd",
                "payment_preimage": payment_preimage,
                "expiry": "0xe10",
            })],
        )
        .await
        .expect("create tenant invoice");
    let payment_hash: Hash256 = invoice.invoice.data.payment_hash.into();
    crate::fiber::tests::external_signer_restart::approve_fixture_payment(
        &signer.signer,
        payment_hash,
        payment_preimage,
        1_000,
        true,
    )
    .await;
    let payment = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            max_fee_amount: Some(500),
            ..Default::default()
        })
        .await
        .expect("payer starts payment without the external signer");
    assert_eq!(payment.payment_hash, payment_hash);
    wait_until_async_timeout(|| async {
        let status: fiber_json_types::GetChannelSigningStatusResult = client
            .request(
                "get_channel_signing_status",
                rpc_params![serde_json::json!({ "channel_id": channel_id })],
            )
            .await
            .expect("query pending signature without signing");
        matches!(
            status.status,
            ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedCommitment,
                ..
            }
        )
    })
    .await;
    let pending = tenant_store
        .get_channel_actor_state(&channel_id)
        .unwrap()
        .signing_context
        .pending_signature
        .clone()
        .unwrap();
    let before = fixture.public_t.get_channel_actor_state(channel_id);
    assert!(before.is_waiting_tlc_ack());
    // Cross a real ACK timeout without changing actor state.
    tokio::time::sleep(Duration::from_millis(PEER_CHANNEL_RESPONSE_TIMEOUT + 500)).await;
    let after = fixture.public_t.get_channel_actor_state(channel_id);
    assert_eq!(after.state, ChannelState::ChannelReady);
    assert!(after.is_waiting_tlc_ack());
    assert_eq!(after.commitment_numbers, before.commitment_numbers);
    assert_eq!(
        tenant_store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .signing_context
            .pending_signature
            .as_ref()
            .unwrap()
            .request_id,
        pending.request_id,
        "waiting must preserve the immutable signing request"
    );

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            signer.try_sign_pending(channel_id).await;
            if payer.get_payment_status(payment_hash).await
                == crate::fiber_types::PaymentStatus::Success
                && !fixture
                    .public_t
                    .get_channel_actor_state(channel_id)
                    .is_waiting_tlc_ack()
                && !tenant_store
                    .get_channel_actor_state(&channel_id)
                    .unwrap()
                    .signing_context
                    .is_awaiting_signature()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("payment completes when the external signer returns");
    assert_eq!(
        fixture.public_t.get_channel_actor_state(channel_id).state,
        ChannelState::ChannelReady
    );
    let invoice: fiber_json_types::GetInvoiceResult = client
        .request(
            "get_invoice",
            rpc_params![fiber_json_types::InvoiceParams {
                payment_hash: payment_hash.into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(invoice.status, fiber_json_types::CkbInvoiceStatus::Paid);
}

async fn authenticated_subscription_client(
    address: std::net::SocketAddr,
    token: &str,
) -> jsonrpsee::ws_client::WsClient {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    jsonrpsee::ws_client::WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://{address}"))
        .await
        .unwrap()
}

async fn subscribe_signing(
    client: &jsonrpsee::ws_client::WsClient,
) -> jsonrpsee::core::client::Subscription<fiber_json_types::SubscriptionEvent> {
    use jsonrpsee::core::client::SubscriptionClientT;
    client
        .subscribe("subscribe", rpc_params!["channel_signing"], "unsubscribe")
        .await
        .unwrap()
}

async fn next_subscription_signature(
    subscription: &mut jsonrpsee::core::client::Subscription<fiber_json_types::SubscriptionEvent>,
) -> GetChannelSigningStatusResult {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let fiber_json_types::SubscriptionEvent::ChannelSigning(result) =
                subscription.next().await.unwrap().unwrap()
            {
                if matches!(
                    result.status,
                    ChannelSigningStatus::SignatureRequired { .. }
                ) {
                    return *result;
                }
            }
        }
    })
    .await
    .expect("signature arrives on subscription")
}

#[tokio::test]
async fn hosted_signing_subscription_recovers_pending_without_waking_cold_tenants() {
    use fiber_json_types::SubscriptionEvent;
    use jsonrpsee::core::client::SubscriptionClientT;
    let fixture = TestLspFixture::new("signing-subscription").await;
    let (tenant_id, _, token, owner) = fixture.register_tenant(1).await;
    let (_, _, other_token, _) = fixture.register_tenant(2).await;
    let client = authenticated_subscription_client(fixture.rpc_addr, &token).await;
    let other_client = authenticated_subscription_client(fixture.rpc_addr, &other_token).await;
    let mut other = subscribe_signing(&other_client).await;
    let status = ractor::call!(fixture._lsp_actor, LspServiceMessage::GetStatus).unwrap();
    assert_eq!(
        status.active_tenants, 0,
        "subscribing does not activate tenant runtimes"
    );
    let (channel_id, sdk) = open_hosted_external_channel(&fixture, &owner).await;
    let mut first = subscribe_signing(&client).await;
    let pending = next_subscription_signature(&mut first).await;
    assert_eq!(pending.channel_id, channel_id.into());
    first.unsubscribe().await.unwrap();
    fixture
        .admin_client
        .request::<crate::rpc::lsp::LspTenantStatus, _>(
            "lsp_evict_tenant",
            rpc_params![LspTenantParams {
                tenant_id: tenant_id.to_string()
            }],
        )
        .await
        .unwrap();
    let mut replay = subscribe_signing(&client).await;
    let recovered = next_subscription_signature(&mut replay).await;
    assert_eq!(
        serde_json::to_value(&recovered).unwrap(),
        serde_json::to_value(&pending).unwrap()
    );
    let status = ractor::call!(fixture._lsp_actor, LspServiceMessage::GetStatus).unwrap();
    assert_eq!(status.active_tenants, 0, "pending replay stays cold");
    let denied = other_client
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params![
                "channel_signing",
                fiber_json_types::SubscriptionOptions {
                    channel_ids: Some(vec![channel_id.into()])
                }
            ],
            "unsubscribe",
        )
        .await;
    assert!(denied.is_err());
    let submission = sdk.prepare_submission(channel_id, recovered.status).await;
    assert_eq!(
        sdk.submit(submission).await.unwrap(),
        SubmitChannelSignatureResult::Applied
    );
    let next = next_subscription_signature(&mut replay).await;
    assert_ne!(
        serde_json::to_value(&next.status).unwrap(),
        serde_json::to_value(&pending.status).unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), other.next())
            .await
            .is_err()
    );
    let submission = sdk.prepare_submission(channel_id, next.status).await;
    sdk.submit(submission).await.unwrap();
    sdk.finish_opening(&fixture.public_t, channel_id).await;
    replay.unsubscribe().await.unwrap();
    other.unsubscribe().await.unwrap();
}
