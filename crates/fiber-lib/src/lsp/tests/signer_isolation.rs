use std::{sync::Arc, time::Duration};

use biscuit_auth::{macros::biscuit, KeyPair};
use ckb_types::{
    packed::{Script, Transaction},
    prelude::*,
};
use fiber_lsp_sdk::{
    ChannelSignature, ChannelSigner, ChannelSigningContent, MemoryStore, RootSigner,
};
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
    fiber::{network::NetworkActorMessage, payment::SendPaymentCommand},
    fiber_types::{ChannelState, Hash256, Privkey},
    lsp::{
        BiscuitTokenIssuer, FiberTenantRuntimeFactory, LspService, LspServiceArgs,
        LspServiceMessage, TenantId,
    },
    rpc::{
        channel::{
            to_rpc_channel_open_signer_material, GetChannelSigningStatusParams,
            GetChannelSigningStatusResult, OpenChannelWithExternalFundingParams,
            OpenChannelWithExternalFundingResult, SubmitChannelSignatureParams,
            SubmitChannelSignatureResult, SubmitSignedFundingTxParams, SubmitSignedFundingTxResult,
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
        gen_rpc_config, get_test_root_actor, init_tracing, wait_until_async_timeout,
        wait_until_node_supports_trampoline_routing, NetworkNode, HUGE_CKB_AMOUNT,
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
        let fiber_json_types::ChannelSigningStatus::SignatureRequired {
            request_id,
            content,
            ..
        } = status
        else {
            panic!("channel must have a pending signing request");
        };
        let content = fiber_lsp_sdk::json::musig2_from_rpc(content)
            .expect("RPC signing content must round-trip into fiber-signer plaintext");
        let slot = content.slot;
        let prepared = self
            .signer
            .prepare(ChannelSigningContent::Musig2(content))
            .await
            .expect("independent signer prepares the RPC plaintext");
        let signature = self
            .signer
            .sign(prepared)
            .await
            .expect("independent signer signs the RPC plaintext");
        let ChannelSignature::Musig2(signature) = signature else {
            panic!("channel MuSig2 request must produce a MuSig2 signature");
        };
        let next_material = self
            .signer
            .next_material(slot)
            .await
            .expect("next public signer material");
        SubmitChannelSignatureParams {
            channel_id: channel_id.into(),
            request_id,
            partial_signature: signature.partial_signature.serialize(),
            next_material: Some(fiber_lsp_sdk::json::next_material_to_rpc(&next_material)),
        }
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
    fixture: &TestLspFixture,
    tenant_client: &HttpClient,
) -> (Hash256, ExternalSignerClient) {
    let created = RootSigner::in_memory()
        .await
        .expect("create in-memory root signer");
    let channel_signer = created
        .root_signer
        .create_channel()
        .await
        .expect("create local channel signer");
    let material = channel_signer
        .channel_open_material(false)
        .await
        .expect("channel open material");

    let open: OpenChannelWithExternalFundingResult = tenant_client
        .request(
            "open_channel_with_external_funding",
            rpc_params![OpenChannelWithExternalFundingParams {
                pubkey: fixture.public_t.pubkey.into(),
                funding_amount: 100_000_000_000,
                public: Some(false),
                funding_udt_type_script: None,
                shutdown_script: Script::default().into(),
                funding_lock_script: Script::default().into(),
                funding_lock_script_cell_deps: None,
                commitment_delay_epoch: None,
                commitment_fee_rate: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
                external_channel_signer: Some(to_rpc_channel_open_signer_material(&material)),
            }],
        )
        .await
        .expect("open hosted channel with external funding");
    let channel_id: Hash256 = open.channel_id.into();
    let unsigned_tx: Transaction = open.unsigned_funding_tx.into();
    let expected_inputs: Vec<_> = unsigned_tx
        .raw()
        .inputs()
        .into_iter()
        .map(|input| input.previous_output())
        .collect();
    channel_signer
        .bind_from_approved_funding(&unsigned_tx, 0, Script::default(), &expected_inputs)
        .await
        .expect("bind approved funding");
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
    use fiber_types::{
        OnchainKeyPurpose, OnchainSigningContent, WatchtowerExternalSignerState,
        WatchtowerExternalState, WatchtowerSignerState,
    };

    let fixture = TestLspFixture::new("isolation-watchtower").await;
    let (_, _, _, other) = fixture.register_tenant(1).await;
    let (_, owner_pubkey, _, owner) = fixture.register_tenant(2).await;
    let channel_id = Hash256::from([0x77; 32]);
    let key = Privkey::from([10; 32]);
    let mut params = CreateWatchChannelParams {
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

    // This test checks authenticated routing only; the watchtower actor tests
    // cover generating and applying requests from real settlement transactions.
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: Transaction::default(),
    };
    let request_id = Hash256::from([0x88; 32]);
    fixture.public_t.store.put_watchtower_signer(
        &crate::lsp::tenant_watchtower_node_id(&owner_pubkey),
        &channel_id,
        WatchtowerSignerState::External(WatchtowerExternalSignerState {
            state: WatchtowerExternalState::AwaitingSignature {
                request_id,
                content: content.clone(),
            },
            last_applied: None,
        }),
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
}

#[tokio::test]
async fn pending_signature_survives_cold_queries_and_resumes_after_eviction() {
    let fixture = TestLspFixture::new("hosted-external-evict").await;
    let (tenant_id, _, _, client) = fixture.register_tenant(1).await;
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
async fn hosted_external_signer_inbound_payment_requires_signing_during_commitment() {
    let mut fixture = TestLspFixture::new("hosted-inbound-external-pay").await;
    let (_tenant_id, _u1_pubkey, _u1_token, u1_client) = fixture.register_tenant(1).await;

    let mut payer = NetworkNode::new_with_node_name("lsp-external-payer").await;
    payer.connect_to(&mut fixture.public_t).await;

    crate::tests::establish_channel_between_nodes(
        &mut fixture.public_t,
        &mut payer,
        crate::tests::ChannelParameters {
            public: true,
            node_a_funding_amount: 100_000_000_000,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;

    crate::tests::test_utils::wait_for_network_graph_update(&payer, 1).await;
    wait_until_node_supports_trampoline_routing(&payer, &fixture.public_t).await;

    let (channel_id, sdk) = open_hosted_external_channel(&fixture, &u1_client).await;

    sdk.finish_opening(&fixture.public_t, channel_id).await;

    // Create hosted invoice for tenant
    let payment_preimage = Hash256::from([0xab; 32]);
    let invoice: fiber_json_types::InvoiceResult = u1_client
        .request(
            "new_invoice",
            rpc_params![fiber_json_types::NewInvoiceParams {
                amount: 1_000,
                description: Some("inbound to external signer hosted channel".to_string()),
                currency: fiber_json_types::Currency::Fibd,
                payment_preimage: Some(payment_preimage.into()),
                payment_hash: None,
                expiry: Some(60 * 60),
                fallback_address: None,
                final_expiry_delta: None,
                udt_type_script: None,
                hash_algorithm: None,
                allow_mpp: None,
                allow_trampoline_routing: Some(true),
                lsp_buffer_duration_ms: None,
            }],
        )
        .await
        .expect("create hosted invoice");
    let payment_hash: Hash256 = invoice.invoice.data.payment_hash.into();

    let payment_response = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            max_fee_amount: Some(500),
            ..Default::default()
        })
        .await
        .expect("send payment to hosted tenant");
    assert_eq!(payment_response.payment_hash, payment_hash);

    // Sign incoming commitment and TLC removal until invoice is marked Paid
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            sdk.try_sign_pending(channel_id).await;
            let res: Result<fiber_json_types::GetInvoiceResult, _> = u1_client
                .request(
                    "get_invoice",
                    rpc_params![fiber_json_types::InvoiceParams {
                        payment_hash: payment_hash.into(),
                    }],
                )
                .await;
            if let Ok(inv) = res {
                if inv.status == fiber_json_types::CkbInvoiceStatus::Paid {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("payer payment should succeed after external signer completes signing");
}
