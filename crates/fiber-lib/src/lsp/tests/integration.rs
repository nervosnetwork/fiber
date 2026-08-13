use std::{
    collections::HashMap,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use biscuit_auth::{macros::biscuit, KeyPair};
use ckb_types::packed::Script;
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
use ractor::{Actor, ActorRef};
use secp256k1::SECP256K1;
use tempfile::{tempdir, TempDir};

use super::{lsp_config, NoopNetworkActor};
use crate::{
    ckb::{client::CkbRpcClient, config::CkbConfig},
    fiber::{
        network::{
            FiberActorCommand, FiberActorMessage, FiberActorRef, NetworkActorMessage,
            PublicNetworkCommand,
        },
        payment::SendPaymentCommand,
    },
    fiber_types::Privkey,
    gen_rand_sha256_hash,
    invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder},
    lsp::{
        FiberTenantRuntimeFactory, HostedTenantRecord, HostedTenantRpcContext, HostedTenantRuntime,
        LspInvoiceStore, LspService, LspServiceArgs, LspServiceMessage, TenantId,
        TenantRuntimeFactory,
    },
    rpc::{
        lsp::{
            GetLspTenantRegistryNonceParams, GetLspTenantRegistryNonceResult, ListLspTenantsResult,
            LspInvoiceRegistration, LspPaymentDelivery, LspPaymentDeliveryStatus,
            LspPaymentHashParams, LspServiceStatus, LspTenantParams, LspTenantRuntimeStatus,
            NewLspInvoiceParams, RegisterLspTenantParams, RegisterLspTenantResult,
            SendLspPaymentParams,
        },
        payment::{GetPaymentCommandResult, SendPaymentCommandParams},
        server::start_rpc,
    },
    store::NodeNamespace,
    tests::{
        create_n_nodes_network_with_visibility, establish_channel_between_nodes, gen_rpc_config,
        get_test_root_actor, init_tracing, wait_until_async_timeout,
        wait_until_node_supports_trampoline_routing, ChannelParameters, NetworkNode,
        HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
    },
    NetworkServiceEvent,
};

const TENANT_ID: &str = "u1";
const BUFFER_DURATION_MS: u64 = 120_000;

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
    root_signer_key: &crate::fiber_types::Privkey,
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

async fn register_legacy_test_tenant(actor: &ActorRef<LspServiceMessage>, tenant_id: TenantId) {
    ractor::call!(actor, LspServiceMessage::RegisterTenant, tenant_id)
        .expect("register legacy test tenant")
        .expect("legacy test tenant registration");
}

struct ExistingRuntime {
    record: HostedTenantRecord,
    network_actor: ActorRef<NetworkActorMessage>,
    rpc_context: Option<HostedTenantRpcContext>,
}

struct ExistingRuntimeFactory {
    runtimes: HashMap<TenantId, ExistingRuntime>,
    starts: Arc<AtomicUsize>,
}

impl ExistingRuntimeFactory {
    fn single(
        record: HostedTenantRecord,
        network_actor: ActorRef<NetworkActorMessage>,
        rpc_context: Option<HostedTenantRpcContext>,
        starts: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            runtimes: HashMap::from([(
                record.tenant_id.clone(),
                ExistingRuntime {
                    record,
                    network_actor,
                    rpc_context,
                },
            )]),
            starts,
        }
    }
}

#[async_trait]
impl TenantRuntimeFactory for ExistingRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        self.runtimes
            .get(tenant_id)
            .map(|runtime| runtime.record.clone())
            .ok_or_else(|| format!("unknown integration-test tenant {tenant_id}"))
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        let existing = self
            .runtimes
            .get(&record.tenant_id)
            .ok_or_else(|| format!("unknown integration-test tenant {}", record.tenant_id))?;
        if record.tenant_pubkey != existing.record.tenant_pubkey {
            return Err("tenant record changed after registration".to_string());
        }
        self.starts.fetch_add(1, Ordering::Relaxed);
        let runtime = HostedTenantRuntime::network_backed(
            record.tenant_pubkey,
            existing.network_actor.clone(),
        );
        Ok(match &existing.rpc_context {
            Some(rpc_context) => runtime.with_rpc_context(rpc_context.clone()),
            None => runtime,
        })
    }
}

async fn register_in_process_endpoint(
    local: &NetworkNode,
    remote_pubkey: crate::fiber_types::Pubkey,
    endpoint: FiberActorRef,
) {
    ractor::call_t!(
        local.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::RegisterInProcessPeer {
            pubkey: remote_pubkey,
            actor: endpoint,
            features: crate::fiber_types::FeatureVector::default(),
            reply,
        },),
        5_000
    )
    .expect("register in-process peer reply")
    .expect("register in-process peer");
}

async fn activate_in_process_peer(local: &NetworkNode, remote: &NetworkNode) {
    ractor::call_t!(
        local.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::ActivateInProcessPeer(
            remote.pubkey,
            reply,
        )),
        5_000
    )
    .expect("activate in-process peer reply")
    .expect("activate in-process peer");
}

async fn connect_in_process(left: &NetworkNode, right: &NetworkNode) {
    register_in_process_endpoint(
        left,
        right.pubkey,
        FiberActorRef::from_network(&right.network_actor),
    )
    .await;
    register_in_process_endpoint(
        right,
        left.pubkey,
        FiberActorRef::from_network(&left.network_actor),
    )
    .await;
    activate_in_process_peer(left, right).await;
    activate_in_process_peer(right, left).await;
}

fn disconnect_in_process(left: &NetworkNode, right: &NetworkNode) {
    left.network_actor
        .send_message(NetworkActorMessage::new_command(
            FiberActorCommand::UnregisterInProcessPeer(right.pubkey),
        ))
        .expect("unregister right in-process peer");
    right
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            FiberActorCommand::UnregisterInProcessPeer(left.pubkey),
        ))
        .expect("unregister left in-process peer");
}

async fn wait_for_tenant_channel(client: &HttpClient, tenant_id: &TenantId, expected_online: bool) {
    wait_until_async_timeout(|| async {
        let result: ListLspTenantsResult = client
            .request("lsp_list_tenants", rpc_params![])
            .await
            .expect("list hosted tenants");
        result.tenants.iter().any(|tenant| {
            tenant.tenant_id == tenant_id.as_str() && tenant.channel_online == expected_online
        })
    })
    .await;
}

async fn wait_for_delivery_status(
    client: &HttpClient,
    payment_hash: crate::fiber_types::Hash256,
    expected: LspPaymentDeliveryStatus,
) -> LspPaymentDelivery {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result: Result<LspPaymentDelivery, _> = client
                .request(
                    "lsp_get_payment_delivery",
                    rpc_params![LspPaymentHashParams {
                        payment_hash: payment_hash.into(),
                    }],
                )
                .await;
            if let Ok(delivery) = result {
                if std::mem::discriminant(&delivery.status) == std::mem::discriminant(&expected) {
                    return delivery;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("delivery reached expected status")
}

struct HostedTenantTestNode {
    tenant_id: TenantId,
    lsp_node_index: usize,
    node: NetworkNode,
    private_channel_id: crate::fiber_types::Hash256,
}

struct LspTestService {
    node_index: usize,
    client: HttpClient,
    rpc_handle: ServerHandle,
    actor: ActorRef<LspServiceMessage>,
}

struct LspTestNetwork {
    nodes: Vec<NetworkNode>,
    channel_ids: Vec<crate::fiber_types::Hash256>,
    tenants: Vec<HostedTenantTestNode>,
    lsp_services: Vec<LspTestService>,
    _root: TempDir,
}

impl LspTestNetwork {
    fn lsp(&self, node_index: usize) -> &LspTestService {
        self.lsp_services
            .iter()
            .find(|lsp| lsp.node_index == node_index)
            .unwrap_or_else(|| panic!("network node {node_index} is not an LSP"))
    }

    fn tenant(&self, lsp_node_index: usize, tenant_id: &str) -> &HostedTenantTestNode {
        self.tenants
            .iter()
            .find(|tenant| {
                tenant.lsp_node_index == lsp_node_index && tenant.tenant_id.as_str() == tenant_id
            })
            .unwrap_or_else(|| {
                panic!("unknown hosted tenant {tenant_id} on LSP node {lsp_node_index}")
            })
    }

    fn tenant_lsp(&self, lsp_node_index: usize, tenant_id: &str) -> &LspTestService {
        self.lsp(self.tenant(lsp_node_index, tenant_id).lsp_node_index)
    }

    async fn send_payment(
        &self,
        lsp_node_index: usize,
        tenant_id: &str,
        command: SendPaymentCommand,
    ) -> Result<GetPaymentCommandResult, jsonrpsee::core::ClientError> {
        let payment = SendPaymentCommandParams {
            target_pubkey: command.target_pubkey.map(Into::into),
            amount: command.amount,
            payment_hash: command.payment_hash.map(Into::into),
            final_tlc_expiry_delta: command.final_tlc_expiry_delta,
            tlc_expiry_limit: command.tlc_expiry_limit,
            invoice: command.invoice,
            timeout: command.timeout,
            max_fee_amount: command.max_fee_amount,
            max_fee_rate: command.max_fee_rate,
            max_parts: command.max_parts,
            trampoline_hops: command
                .trampoline_hops
                .map(|hops| hops.into_iter().map(Into::into).collect()),
            keysend: command.keysend,
            udt_type_script: command.udt_type_script.map(Into::into),
            allow_self_payment: command.allow_self_payment.then_some(true),
            custom_records: command.custom_records.map(Into::into),
            hop_hints: command.hop_hints.map(|hints| {
                hints
                    .into_iter()
                    .map(|hint| fiber_json_types::HopHint {
                        pubkey: hint.pubkey.into(),
                        channel_outpoint: hint.channel_outpoint,
                        fee_rate: hint.fee_rate,
                        tlc_expiry_delta: hint.tlc_expiry_delta,
                    })
                    .collect()
            }),
            dry_run: command.dry_run.then_some(true),
        };
        self.tenant_lsp(lsp_node_index, tenant_id)
            .client
            .request(
                "lsp_send_payment",
                rpc_params![SendLspPaymentParams {
                    tenant_id: tenant_id.to_string(),
                    payment,
                }],
            )
            .await
    }

    async fn stop(self) {
        for lsp in self.lsp_services {
            lsp.rpc_handle.stop().expect("stop LSP RPC server");
            lsp.rpc_handle.stopped().await;
            lsp.actor
                .stop(Some("integration test complete".to_string()));
        }
    }
}

/// Builds an ordinary Fiber network, starts an LSP service on every network
/// node referenced by `tenant_channels`, and connects each hosted tenant to
/// its selected LSP with a private channel.
///
/// `network_channels` uses the same `(node indexes, funding amounts, visibility)`
/// shape as `create_n_nodes_network_with_visibility`. Each tenant entry is
/// `((lsp_node_index, tenant_id), (lsp_funding, tenant_funding))`.
#[allow(clippy::type_complexity)]
async fn create_lsp_test_network(
    network_channels: &[((usize, usize), (u128, u128), bool)],
    node_count: usize,
    tenant_channels: &[((usize, &str), (u128, u128))],
) -> LspTestNetwork {
    assert!(node_count >= 2);
    assert!(!tenant_channels.is_empty());

    let (mut nodes, channel_ids) =
        create_n_nodes_network_with_visibility(network_channels, node_count).await;
    let mut tenants = Vec::<(TenantId, usize, NetworkNode)>::with_capacity(tenant_channels.len());
    let mut runtimes_by_lsp = HashMap::<usize, HashMap<TenantId, ExistingRuntime>>::new();

    for ((lsp_node_index, tenant_name), _) in tenant_channels {
        assert!(
            *lsp_node_index < nodes.len(),
            "LSP node index {lsp_node_index} is out of bounds"
        );
        let tenant_id = TenantId::new(*tenant_name).expect("valid test tenant id");
        assert!(
            !tenants.iter().any(|(registered_id, registered_lsp, _)| {
                registered_lsp == lsp_node_index && registered_id == &tenant_id
            }),
            "duplicate test tenant id {tenant_id} on LSP node {lsp_node_index}"
        );
        let tenant = NetworkNode::new_with_node_name(&format!("lsp-tenant-{tenant_name}")).await;
        connect_in_process(&nodes[*lsp_node_index], &tenant).await;
        let record = HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            root_signer_pubkey: None,
            tenant_pubkey: tenant.pubkey,
            private_channel_id: None,
            created_at: crate::now_timestamp_as_millis_u64(),
        };
        runtimes_by_lsp.entry(*lsp_node_index).or_default().insert(
            tenant_id.clone(),
            ExistingRuntime {
                record,
                network_actor: tenant.network_actor.clone(),
                rpc_context: Some(HostedTenantRpcContext {
                    tenant_id: tenant_id.clone(),
                    config: tenant.fiber_config.clone(),
                    fiber_actor: FiberActorRef::from_network(&tenant.network_actor),
                    public_node_id: nodes[*lsp_node_index].pubkey,
                    store: tenant.store.clone(),
                }),
            },
        );
        tenants.push((tenant_id, *lsp_node_index, tenant));
    }

    let root = tempdir().expect("temporary LSP directory");
    let root_actor = get_test_root_actor().await;
    let mut lsp_node_indexes = runtimes_by_lsp.keys().copied().collect::<Vec<_>>();
    lsp_node_indexes.sort_unstable();
    let mut lsp_services = Vec::with_capacity(lsp_node_indexes.len());
    for lsp_node_index in lsp_node_indexes {
        let config = lsp_config(root.path().join(format!("lsp-{lsp_node_index}")));
        let runtime_factory = Arc::new(ExistingRuntimeFactory {
            runtimes: runtimes_by_lsp
                .remove(&lsp_node_index)
                .expect("runtime group for LSP"),
            starts: Arc::new(AtomicUsize::new(0)),
        });
        let lsp_actor = Actor::spawn_linked(
            None,
            LspService,
            LspServiceArgs {
                config,
                public_node_id: nodes[lsp_node_index].pubkey,
                public_network_actor: nodes[lsp_node_index].network_actor.clone(),
                store: nodes[lsp_node_index]
                    .store
                    .namespaced(NodeNamespace::lsp_metadata()),
                runtime_factory,
                signing_key: nodes[lsp_node_index].private_key.clone(),
            },
            root_actor.get_cell(),
        )
        .await
        .expect("start LSP service")
        .0;
        nodes[lsp_node_index]
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                PublicNetworkCommand::SetLspService(lsp_actor.clone()),
            ))
            .expect("attach LSP service to public node");

        let mut rpc_config = gen_rpc_config();
        rpc_config.enabled_modules = vec!["lsp".to_string()];
        let (rpc_handle, rpc_addr) = start_rpc(
            rpc_config,
            None,
            None,
            None,
            None,
            Some(lsp_actor.clone()),
            nodes[lsp_node_index].store.clone(),
            None,
            None,
            root_actor.get_cell(),
            None,
            #[cfg(debug_assertions)]
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .await
        .expect("start LSP RPC server");
        let client = HttpClientBuilder::default()
            .build(format!("http://{rpc_addr}"))
            .expect("build LSP RPC client");
        lsp_services.push(LspTestService {
            node_index: lsp_node_index,
            client,
            rpc_handle,
            actor: lsp_actor,
        });
    }

    for (tenant_id, lsp_node_index, _) in &tenants {
        let service = lsp_services
            .iter()
            .find(|lsp| lsp.node_index == *lsp_node_index)
            .expect("LSP service for tenant");
        register_legacy_test_tenant(&service.actor, tenant_id.clone()).await;
    }

    let mut hosted_tenants = Vec::with_capacity(tenants.len());
    for ((tenant_id, lsp_node_index, mut tenant), (_, funding)) in
        tenants.into_iter().zip(tenant_channels)
    {
        let private_channel_id = establish_channel_between_nodes(
            &mut nodes[lsp_node_index],
            &mut tenant,
            ChannelParameters {
                public: false,
                node_a_funding_amount: funding.0,
                node_b_funding_amount: funding.1,
                ..Default::default()
            },
        )
        .await
        .0;
        let client = &lsp_services
            .iter()
            .find(|lsp| lsp.node_index == lsp_node_index)
            .expect("LSP service for tenant")
            .client;
        wait_for_tenant_channel(client, &tenant_id, true).await;
        hosted_tenants.push(HostedTenantTestNode {
            tenant_id,
            lsp_node_index,
            node: tenant,
            private_channel_id,
        });
    }

    LspTestNetwork {
        nodes,
        channel_ids,
        tenants: hosted_tenants,
        lsp_services,
        _root: root,
    }
}

#[tokio::test]
async fn production_factory_activates_one_tenant_runtime_via_rpc() {
    init_tracing();

    let public_t = NetworkNode::new_with_node_name("lsp-production-public-t").await;
    let root = tempdir().expect("temporary LSP directory");
    let config = lsp_config(root.path().join("lsp"));
    let ckb_config = CkbConfig {
        base_dir: Some(root.path().join("ckb")),
        rpc_url: "http://127.0.0.1:8114".to_string(),
        udt_whitelist: None,
        tx_tracing_polling_interval_ms: 4_000,
        funding_tx_shell_builder: None,
    };
    let root_actor = get_test_root_actor().await;
    let runtime_factory = Arc::new(FiberTenantRuntimeFactory::new(
        config.clone(),
        public_t.fiber_config.clone(),
        CkbRpcClient::new(&ckb_config),
        public_t.chain_actor.clone(),
        public_t.network_actor.clone(),
        public_t.store.clone(),
        root_actor.get_cell(),
        Script::default(),
    ));
    let tenant_id = TenantId::new(TENANT_ID).unwrap();
    let expected_tenant = runtime_factory
        .provision(&tenant_id)
        .expect("provision hosted tenant identity");
    assert_ne!(expected_tenant.tenant_pubkey, public_t.pubkey);

    let public_network_actor_id = public_t.network_actor.get_id();
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
        },
        root_actor.get_cell(),
    )
    .await
    .expect("start LSP service with production runtime factory")
    .0;

    let mut rpc_config = gen_rpc_config();
    rpc_config.enabled_modules = vec!["lsp".to_string()];
    let (rpc_handle, rpc_addr) = start_rpc(
        rpc_config,
        None,
        None,
        None,
        None,
        Some(lsp_actor.clone()),
        public_t.store.clone(),
        None,
        None,
        root_actor.get_cell(),
        None,
        #[cfg(debug_assertions)]
        None,
        #[cfg(debug_assertions)]
        None,
    )
    .await
    .expect("start LSP RPC server");
    let client = HttpClientBuilder::default()
        .build(format!("http://{rpc_addr}"))
        .expect("build LSP RPC client");

    let registered = ractor::call!(
        lsp_actor,
        LspServiceMessage::RegisterTenant,
        tenant_id.clone()
    )
    .expect("register tenant actor reply")
    .expect("register tenant");
    assert_eq!(
        registered.status.record.tenant_pubkey,
        expected_tenant.tenant_pubkey
    );
    assert!(matches!(
        registered.status.runtime_status,
        crate::lsp::TenantRuntimeStatus::Cold
    ));

    let ensured: crate::rpc::lsp::LspTenantStatus = client
        .request(
            "lsp_ensure_tenant",
            rpc_params![LspTenantParams {
                tenant_id: TENANT_ID.to_string(),
            }],
        )
        .await
        .expect("activate hosted tenant");
    assert!(matches!(
        ensured.runtime_status,
        LspTenantRuntimeStatus::Active
    ));

    let status: LspServiceStatus = client
        .request("lsp_get_status", rpc_params![])
        .await
        .expect("get active LSP status");
    assert_eq!(status.registered_tenants, 1);
    assert_eq!(status.active_tenants, 1);
    assert_eq!(public_t.network_actor.get_id(), public_network_actor_id);
    let tenant_rpc_context = ractor::call_t!(
        lsp_actor,
        |reply| LspServiceMessage::GetTenantRpcContext(tenant_id.clone(), reply),
        5_000
    )
    .expect("get hosted tenant RPC context")
    .expect("hosted tenant RPC context");
    let tenant_actor_name = tenant_rpc_context
        .fiber_actor
        .get_name()
        .expect("hosted tenant actor has a name");
    assert!(tenant_actor_name.starts_with("HostedTenant "));
    assert!(!tenant_actor_name.starts_with("Network "));
    let activity = ractor::call_t!(
        tenant_rpc_context.fiber_actor.clone(),
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetHostedTenantActivity(reply)),
        5_000
    )
    .expect("hosted tenant accepts Fiber core commands");
    assert!(activity.is_idle());

    // Funding and closing transaction tracers are linked children which stop normally once their
    // result is delivered. Their termination must not cascade into the hosted tenant runtime.
    let (completed_child, completed_child_handle) = Actor::spawn_linked(
        None,
        NoopNetworkActor,
        (),
        tenant_rpc_context.fiber_actor.get_cell(),
    )
    .await
    .expect("start hosted tenant child");
    completed_child.stop(Some("test child completed".to_string()));
    completed_child_handle
        .await
        .expect("join completed hosted tenant child");
    let activity = ractor::call_t!(
        tenant_rpc_context.fiber_actor.clone(),
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetHostedTenantActivity(reply)),
        5_000
    )
    .expect("hosted tenant survives linked child termination");
    assert!(activity.is_idle());

    let impostor = Actor::spawn(None, NoopNetworkActor, ())
        .await
        .expect("start impostor endpoint")
        .0;
    let duplicate = ractor::call_t!(
        public_t.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::RegisterInProcessPeer {
            pubkey: expected_tenant.tenant_pubkey,
            actor: crate::fiber::FiberActorRef::from_network(&impostor),
            features: crate::fiber_types::FeatureVector::default(),
            reply,
        },),
        5_000
    )
    .expect("duplicate in-process peer reply");
    assert_eq!(
        duplicate.unwrap_err(),
        format!(
            "in-process peer {:?} is already owned by another actor",
            expected_tenant.tenant_pubkey
        )
    );

    let ensured_again: crate::rpc::lsp::LspTenantStatus = client
        .request(
            "lsp_ensure_tenant",
            rpc_params![LspTenantParams {
                tenant_id: TENANT_ID.to_string(),
            }],
        )
        .await
        .expect("ensure active tenant again");
    assert!(matches!(
        ensured_again.runtime_status,
        LspTenantRuntimeStatus::Active
    ));
    let status: LspServiceStatus = client
        .request("lsp_get_status", rpc_params![])
        .await
        .expect("get idempotent LSP status");
    assert_eq!(status.active_tenants, 1);

    let evicted: crate::rpc::lsp::LspTenantStatus = client
        .request(
            "lsp_evict_tenant",
            rpc_params![LspTenantParams {
                tenant_id: TENANT_ID.to_string(),
            }],
        )
        .await
        .expect("evict hosted tenant");
    assert!(matches!(
        evicted.runtime_status,
        LspTenantRuntimeStatus::Cold
    ));
    let status: LspServiceStatus = client
        .request("lsp_get_status", rpc_params![])
        .await
        .expect("get cold LSP status");
    assert_eq!(status.active_tenants, 0);

    rpc_handle.stop().expect("stop LSP RPC server");
    rpc_handle.stopped().await;
    lsp_actor.stop(Some("integration test complete".to_string()));
}

#[tokio::test]
async fn biscuit_tenant_context_routes_standard_rpc_to_hosted_runtime() {
    init_tracing();

    let public_t = NetworkNode::new_with_node_name("lsp-auth-public-t").await;
    let root = tempdir().expect("temporary LSP directory");
    let config = lsp_config(root.path().join("lsp"));
    let ckb_config = CkbConfig {
        base_dir: Some(root.path().join("ckb")),
        rpc_url: "http://127.0.0.1:8114".to_string(),
        udt_whitelist: None,
        tx_tracing_polling_interval_ms: 4_000,
        funding_tx_shell_builder: None,
    };
    let root_actor = get_test_root_actor().await;
    let runtime_factory = Arc::new(FiberTenantRuntimeFactory::new(
        config.clone(),
        public_t.fiber_config.clone(),
        CkbRpcClient::new(&ckb_config),
        public_t.chain_actor.clone(),
        public_t.network_actor.clone(),
        public_t.store.clone(),
        root_actor.get_cell(),
        Script::default(),
    ));
    let root_signer_key = Privkey::from(&[42; 32]);
    let tenant_id = TenantId::from_root_signer_pubkey(&root_signer_key.pubkey());
    let expected_tenant = runtime_factory
        .provision(&tenant_id)
        .expect("provision hosted tenant identity");
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
        },
        root_actor.get_cell(),
    )
    .await
    .expect("start authenticated LSP service")
    .0;

    let biscuit_root = KeyPair::new();
    let admin_token = biscuit!(
        r#"
            read("lsp");
            write("lsp");
        "#
    )
    .build(&biscuit_root)
    .unwrap()
    .to_base64()
    .unwrap();
    let public_invoice_token = biscuit!(r#"read("invoices");"#)
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
    let public_client = authenticated_client(rpc_addr, &public_invoice_token);

    assert!(public_client
        .request::<GetLspTenantRegistryNonceResult, _>(
            "lsp_get_tenant_registry_nonce",
            rpc_params![GetLspTenantRegistryNonceParams {
                root_signer_pubkey: root_signer_key.pubkey().into(),
            }],
        )
        .await
        .is_err());

    let registered = register_root_signer_tenant(&admin_client, &root_signer_key)
        .await
        .expect("register RootSigner tenant");
    assert_eq!(registered.tenant.tenant_id, tenant_id.as_str());
    assert_eq!(
        registered.tenant.root_signer_pubkey,
        Some(root_signer_key.pubkey().into())
    );
    assert!(matches!(
        registered.tenant.runtime_status,
        LspTenantRuntimeStatus::Cold
    ));
    let tenant_token = registered
        .access_token
        .expect("new tenant registration must issue an access token");
    let duplicate = match register_root_signer_tenant(&admin_client, &root_signer_key).await {
        Ok(_) => panic!("registration proof must be one-time"),
        Err(error) => error,
    };
    assert!(duplicate.to_string().contains("already registered"));
    let tenant_client = authenticated_client(rpc_addr, &tenant_token);

    let channels: fiber_json_types::ListChannelsResult = tenant_client
        .request(
            "list_channels",
            rpc_params![fiber_json_types::ListChannelsParams {
                pubkey: None,
                include_closed: None,
                only_pending: None,
            }],
        )
        .await
        .expect("list tenant channels through the standard RPC");
    assert!(channels.channels.is_empty());
    let payments: fiber_json_types::ListPaymentsResult = tenant_client
        .request(
            "list_payments",
            rpc_params![fiber_json_types::ListPaymentsParams {
                status: None,
                limit: None,
                after: None,
            }],
        )
        .await
        .expect("list tenant payments through the standard RPC");
    assert!(payments.payments.is_empty());

    let active: LspServiceStatus = admin_client
        .request("lsp_get_status", rpc_params![])
        .await
        .expect("get active LSP status");
    assert_eq!(active.active_tenants, 1);

    let public_channel_error = match tenant_client
        .request::<fiber_json_types::OpenChannelResult, _>(
            "open_channel",
            rpc_params![fiber_json_types::OpenChannelParams {
                pubkey: public_t.pubkey.into(),
                funding_amount: 1_000,
                public: Some(true),
                one_way: None,
                shutdown_script: None,
                commitment_delay_epoch: None,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
            }],
        )
        .await
    {
        Ok(_) => panic!("tenant must not open a public channel"),
        Err(error) => error,
    };
    assert!(public_channel_error
        .to_string()
        .contains("hosted tenant channels must be private"));

    let wrong_peer_error = match tenant_client
        .request::<fiber_json_types::OpenChannelResult, _>(
            "open_channel",
            rpc_params![fiber_json_types::OpenChannelParams {
                pubkey: expected_tenant.tenant_pubkey.into(),
                funding_amount: 1_000,
                public: Some(false),
                one_way: None,
                shutdown_script: None,
                commitment_delay_epoch: None,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
            }],
        )
        .await
    {
        Ok(_) => panic!("tenant must not open a channel to another peer"),
        Err(error) => error,
    };
    assert!(wrong_peer_error
        .to_string()
        .contains("hosted tenants may only open a channel to the public LSP node"));

    let opened: fiber_json_types::OpenChannelResult = tenant_client
        .request(
            "open_channel",
            rpc_params![fiber_json_types::OpenChannelParams {
                pubkey: public_t.pubkey.into(),
                funding_amount: MIN_RESERVED_CKB,
                public: Some(false),
                one_way: None,
                shutdown_script: None,
                commitment_delay_epoch: None,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
            }],
        )
        .await
        .expect("open tenant private channel through the standard RPC");
    let temporary_channel_id = opened.temporary_channel_id.into();
    wait_until_async_timeout(|| {
        let public_network_actor = public_t.network_actor.clone();
        async move {
            ractor::call_t!(
                public_network_actor,
                |reply| NetworkActorMessage::new_command(
                    FiberActorCommand::GetPendingAcceptChannels(reply)
                ),
                5_000
            )
            .ok()
            .and_then(Result::ok)
            .is_some_and(|pending| {
                pending.iter().any(|channel| {
                    channel.channel_id == temporary_channel_id
                        && channel.pubkey == expected_tenant.tenant_pubkey
                })
            })
        }
    })
    .await;

    let accept = ractor::call!(public_t.network_actor, |reply| {
        NetworkActorMessage::new_command(FiberActorCommand::AcceptChannel(
            crate::fiber::network::AcceptChannelCommand {
                temp_channel_id: temporary_channel_id,
                funding_amount: MIN_RESERVED_CKB,
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
    .expect("public LSP network actor alive")
    .expect("accept tenant channel");
    let channel_id = accept.new_channel_id;
    wait_until_async_timeout(|| {
        let tenant_store = public_t
            .store
            .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        async move {
            crate::fiber::channel::ChannelActorStateStore::get_channel_actor_state(
                &tenant_store,
                &channel_id,
            )
            .is_some()
        }
    })
    .await;

    let signing_status: fiber_json_types::GetChannelSigningStatusResult = tenant_client
        .request(
            "get_channel_signing_status",
            rpc_params![fiber_json_types::GetChannelSigningStatusParams {
                channel_id: channel_id.into(),
            }],
        )
        .await
        .expect("read signing status from the tenant channel namespace");
    assert!(matches!(
        signing_status.status,
        fiber_json_types::ChannelSigningStatus::Internal
    ));

    let submit_error = tenant_client
        .request::<fiber_json_types::SubmitChannelSignatureResult, _>(
            "submit_channel_signature",
            rpc_params![fiber_json_types::SubmitChannelSignatureParams {
                channel_id: channel_id.into(),
                request_id: crate::fiber_types::Hash256::from([9; 32]).into(),
                partial_signature: [1; 32],
                next_material: None,
            }],
        )
        .await
        .expect_err("an internal tenant channel must reject external signatures");
    assert!(submit_error
        .to_string()
        .contains("channel does not use an external signer"));

    let invoice: fiber_json_types::InvoiceResult = tenant_client
        .request(
            "new_invoice",
            rpc_params![fiber_json_types::NewInvoiceParams {
                amount: 1_000,
                description: Some("tenant-scoped RPC invoice".to_string()),
                currency: fiber_json_types::Currency::Fibd,
                payment_preimage: None,
                payment_hash: None,
                expiry: Some(60 * 60),
                fallback_address: None,
                final_expiry_delta: None,
                udt_type_script: None,
                hash_algorithm: None,
                allow_mpp: None,
                allow_trampoline_routing: Some(true),
            }],
        )
        .await
        .expect("create invoice through the standard tenant RPC");
    let decoded = crate::invoice::CkbInvoice::from_str(&invoice.invoice_address)
        .expect("decode tenant invoice");
    let expected_tenant_pubkey: secp256k1::PublicKey = expected_tenant.tenant_pubkey.into();
    assert_eq!(decoded.payee_pub_key(), Some(&expected_tenant_pubkey));
    assert_eq!(
        decoded.trampoline_route_hint(),
        Some(&secp256k1::PublicKey::from(public_t.pubkey))
    );
    let payment_hash = invoice.invoice.data.payment_hash.into();
    let registration = public_t
        .store
        .namespaced(NodeNamespace::lsp_metadata())
        .get_lsp_invoice(&payment_hash)
        .expect("read hosted invoice registration")
        .expect("tenant new_invoice should register the hosted invoice");
    assert_eq!(registration.tenant_id, tenant_id);
    assert_eq!(registration.invoice.to_string(), invoice.invoice_address);
    assert_eq!(registration.hint.payload.lsp_node_id, public_t.pubkey);
    assert_eq!(registration.hint.payload.payment_hash, payment_hash);
    assert_eq!(
        registration.hint.payload.buffer_duration_ms,
        crate::lsp::DEFAULT_LSP_BUFFER_DURATION_MS
    );

    let tenant_invoice: fiber_json_types::GetInvoiceResult = tenant_client
        .request(
            "get_invoice",
            rpc_params![fiber_json_types::InvoiceParams {
                payment_hash: invoice.invoice.data.payment_hash,
            }],
        )
        .await
        .expect("read tenant invoice from its namespace");
    assert_eq!(tenant_invoice.invoice_address, invoice.invoice_address);
    if public_client
        .request::<fiber_json_types::GetInvoiceResult, _>(
            "get_invoice",
            rpc_params![fiber_json_types::InvoiceParams {
                payment_hash: invoice.invoice.data.payment_hash,
            }],
        )
        .await
        .is_ok()
    {
        panic!("public node must not see a tenant invoice");
    }

    rpc_handle
        .stop()
        .expect("stop authenticated LSP RPC server");
    rpc_handle.stopped().await;
    lsp_actor.stop(Some("integration test complete".to_string()));
}

#[tokio::test]
async fn hosted_payment_buffers_offline_private_channel_and_resumes_via_rpc() {
    init_tracing();

    // Keep the payer connected only to Public T. U is reachable solely through
    // the private T-U channel, matching the hosted LSP topology.
    let mut payer = NetworkNode::new_with_node_name("lsp-payer").await;
    let mut public_t = NetworkNode::new_with_node_name("lsp-public-t").await;
    let mut tenant = NetworkNode::new_with_node_name("lsp-tenant-u1").await;
    payer.connect_to(&mut public_t).await;
    let tenant_id = TenantId::new(TENANT_ID).unwrap();
    connect_in_process(&public_t, &tenant).await;
    let replacement = ractor::call_t!(
        public_t.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::RegisterInProcessPeer {
            pubkey: tenant.pubkey,
            actor: crate::fiber::FiberActorRef::from_network(&payer.network_actor),
            features: crate::fiber_types::FeatureVector::default(),
            reply,
        },),
        5_000
    )
    .expect("in-process replacement reply");
    assert_eq!(
        replacement.unwrap_err(),
        format!(
            "in-process peer {:?} is already owned by another actor",
            tenant.pubkey
        )
    );

    let root = tempdir().expect("temporary LSP directory");
    let config = lsp_config(root.path().join("lsp"));
    let lsp_store = public_t.store.namespaced(NodeNamespace::lsp_metadata());
    let starts = Arc::new(AtomicUsize::new(0));
    let tenant_record = HostedTenantRecord {
        tenant_id: TenantId::new(TENANT_ID).unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant.pubkey,
        private_channel_id: None,
        created_at: crate::now_timestamp_as_millis_u64(),
    };
    let runtime_factory = Arc::new(ExistingRuntimeFactory::single(
        tenant_record,
        tenant.network_actor.clone(),
        None,
        starts.clone(),
    ));
    let root_actor = get_test_root_actor().await;
    let lsp_actor = Actor::spawn_linked(
        None,
        LspService,
        LspServiceArgs {
            config,
            public_node_id: public_t.pubkey,
            public_network_actor: public_t.network_actor.clone(),
            store: lsp_store,
            runtime_factory,
            signing_key: public_t.private_key.clone(),
        },
        root_actor.get_cell(),
    )
    .await
    .expect("start LSP service")
    .0;
    public_t
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            PublicNetworkCommand::SetLspService(lsp_actor.clone()),
        ))
        .expect("attach LSP service to Public T");

    let mut rpc_config = gen_rpc_config();
    rpc_config.enabled_modules = vec!["lsp".to_string()];
    let (rpc_handle, rpc_addr) = start_rpc(
        rpc_config,
        None,
        None,
        None,
        None,
        Some(lsp_actor.clone()),
        public_t.store.clone(),
        None,
        None,
        root_actor.get_cell(),
        None,
        #[cfg(debug_assertions)]
        None,
        #[cfg(debug_assertions)]
        None,
    )
    .await
    .expect("start LSP RPC server");
    let client = HttpClientBuilder::default()
        .build(format!("http://{rpc_addr}"))
        .expect("build LSP RPC client");

    let status: LspServiceStatus = client
        .request("lsp_get_status", rpc_params![])
        .await
        .expect("get LSP status");
    assert_eq!(status.public_node_id, public_t.pubkey.into());
    assert_eq!(status.registered_tenants, 0);
    assert_eq!(status.active_tenants, 0);

    register_legacy_test_tenant(&lsp_actor, tenant_id.clone()).await;
    let registered: ListLspTenantsResult = client
        .request("lsp_list_tenants", rpc_params![])
        .await
        .expect("list registered hosted tenant");
    let registered = registered.tenants.first().expect("registered tenant");
    assert_eq!(registered.invoice_pubkey, tenant.pubkey.into());
    assert_eq!(registered.private_channel_id, None);
    assert!(matches!(
        registered.runtime_status,
        LspTenantRuntimeStatus::Cold
    ));
    assert!(!registered.channel_online);

    establish_channel_between_nodes(
        &mut payer,
        &mut public_t,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    let (private_channel_id, _) = establish_channel_between_nodes(
        &mut public_t,
        &mut tenant,
        ChannelParameters {
            public: false,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_until_node_supports_trampoline_routing(&payer, &public_t).await;
    wait_for_tenant_channel(&client, &tenant_id, true).await;
    let tenants: ListLspTenantsResult = client
        .request("lsp_list_tenants", rpc_params![])
        .await
        .expect("list hosted tenant after channel binding");
    assert_eq!(
        tenants.tenants[0].private_channel_id,
        Some(private_channel_id.into())
    );

    let amount = 1_000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(tenant.pubkey.into())
        .expiry_time(Duration::from_secs(60 * 60))
        .trampoline_route_hint(public_t.pubkey.into())
        .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &tenant.private_key.0))
        .expect("build hosted invoice");
    tenant.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    let registration = ractor::call!(lsp_actor, |reply| LspServiceMessage::RegisterInvoice {
        tenant_id: TenantId::new(TENANT_ID).unwrap(),
        invoice: invoice.clone(),
        buffer_duration_ms: Some(BUFFER_DURATION_MS),
        reply,
    })
    .expect("register hosted invoice reply")
    .expect("register hosted invoice");
    assert_eq!(registration.tenant_id, TenantId::new(TENANT_ID).unwrap());
    assert_eq!(registration.hint.payload.lsp_node_id, public_t.pubkey);
    assert_eq!(registration.hint.payload.payment_hash, payment_hash);
    assert_eq!(
        registration.hint.payload.buffer_duration_ms,
        BUFFER_DURATION_MS
    );

    let stored_registration = public_t
        .store
        .namespaced(NodeNamespace::lsp_metadata())
        .get_lsp_invoice(&payment_hash)
        .expect("read hosted invoice registration")
        .expect("hosted invoice should be persisted");
    assert_eq!(stored_registration, registration);

    disconnect_in_process(&public_t, &tenant);
    public_t
        .expect_event(|event| {
            matches!(event, NetworkServiceEvent::ChannelOffline(pubkey, channel_id, _) if pubkey == &tenant.pubkey && channel_id == &private_channel_id)
        })
        .await;
    tenant
        .expect_event(|event| {
            matches!(event, NetworkServiceEvent::ChannelOffline(pubkey, channel_id, _) if pubkey == &public_t.pubkey && channel_id == &private_channel_id)
        })
        .await;
    wait_for_tenant_channel(&client, &tenant_id, false).await;

    let response = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(500),
            ..Default::default()
        })
        .await
        .expect("start hosted payment");
    assert_eq!(response.payment_hash, payment_hash);
    let deferred =
        wait_for_delivery_status(&client, payment_hash, LspPaymentDeliveryStatus::Deferred).await;
    assert_eq!(deferred.tenant_id, TENANT_ID);
    assert_eq!(deferred.private_channel_id, private_channel_id.into());
    assert!(deferred.buffer_deadline > crate::now_timestamp_as_millis_u64());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(starts.load(Ordering::Relaxed), 0);

    let ensured: crate::rpc::lsp::LspTenantStatus = client
        .request(
            "lsp_ensure_tenant",
            rpc_params![LspTenantParams {
                tenant_id: TENANT_ID.to_string(),
            }],
        )
        .await
        .expect("activate hosted tenant after signer reconnect");
    assert!(matches!(
        ensured.runtime_status,
        LspTenantRuntimeStatus::Active
    ));
    wait_until_async_timeout(|| async { starts.load(Ordering::Relaxed) == 1 }).await;

    let tenants: ListLspTenantsResult = client
        .request("lsp_list_tenants", rpc_params![])
        .await
        .expect("list active hosted tenant");
    assert!(matches!(
        tenants.tenants[0].runtime_status,
        LspTenantRuntimeStatus::Active
    ));
    assert!(!tenants.tenants[0].channel_online);

    connect_in_process(&public_t, &tenant).await;
    public_t
        .expect_event(|event| {
            matches!(event, NetworkServiceEvent::ChannelOnline(pubkey, channel_id, _) if pubkey == &tenant.pubkey && channel_id == &private_channel_id)
        })
        .await;
    tenant
        .expect_event(|event| {
            matches!(event, NetworkServiceEvent::ChannelOnline(pubkey, channel_id, _) if pubkey == &public_t.pubkey && channel_id == &private_channel_id)
        })
        .await;
    wait_for_tenant_channel(&client, &tenant_id, true).await;

    payer.wait_until_success(payment_hash).await;
    wait_for_delivery_status(&client, payment_hash, LspPaymentDeliveryStatus::Succeeded).await;
    wait_until_async_timeout(|| async {
        tenant.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;

    rpc_handle.stop().expect("stop LSP RPC server");
    rpc_handle.stopped().await;
    lsp_actor.stop(Some("integration test complete".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_tenant_pays_ordinary_node_through_lsp_with_mpp() {
    init_tracing();

    // Hosted U reaches the network only through its private channel to Public T.
    // Neither T -> receiver channel can carry the full payment, so Public T must
    // split the downstream trampoline payment across both channels.
    let network = create_lsp_test_network(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + 1_000_000_000, MIN_RESERVED_CKB),
                false,
            ),
            (
                (0, 1),
                (MIN_RESERVED_CKB + 1_000_000_000, MIN_RESERVED_CKB),
                false,
            ),
        ],
        2,
        &[
            ((0, TENANT_ID), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, "u2"), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
    )
    .await;
    assert_eq!(network.tenants.len(), 2);
    assert_ne!(
        network.tenant(0, TENANT_ID).private_channel_id,
        network.tenant(0, "u2").private_channel_id
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    let public_t = &network.nodes[0];
    let tenant = network.tenant(0, TENANT_ID);
    let receiver = &network.nodes[1];
    let amount = 1_500_000_000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .allow_trampoline_routing(true)
        .allow_mpp(true)
        .payee_pub_key(receiver.pubkey.into())
        .description("hosted tenant trampoline MPP".to_string())
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|message| {
            SECP256K1.sign_ecdsa_recoverable(message, &receiver.private_key.0)
        })
        .expect("build receiver invoice");
    receiver.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    let response = network
        .send_payment(
            0,
            TENANT_ID,
            SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                max_fee_amount: Some(2_000_000),
                trampoline_hops: Some(vec![public_t.pubkey]),
                ..Default::default()
            },
        )
        .await
        .expect("send hosted tenant payment through LSP RPC");
    assert_eq!(response.payment_hash, payment_hash.into());

    tenant.node.wait_until_success(payment_hash).await;
    wait_until_async_timeout(|| async {
        receiver.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;

    let public_t_payment = public_t.get_payment_result(payment_hash).await;
    assert_eq!(public_t_payment.routers.len(), 2);
    let used_channels = public_t
        .routers_used_channels(&public_t_payment.routers, &network.channel_ids)
        .await;
    assert_eq!(used_channels.len(), 2);
    assert_eq!(
        public_t_payment
            .routers
            .iter()
            .map(|route| route.receiver_amount())
            .sum::<u128>(),
        amount
    );
    assert!(public_t
        .store
        .namespaced(NodeNamespace::lsp_metadata())
        .get_lsp_invoice(&payment_hash)
        .expect("read LSP invoice registrations")
        .is_none());

    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_tenant_pays_hosted_tenant_across_two_lsps() {
    init_tracing();

    // U1 -> LSP1 -> N2 -> N3 -> LSP2 -> U2
    let network = create_lsp_test_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
        ],
        4,
        &[
            ((0, "u1"), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, "u2"), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
    )
    .await;
    assert_eq!(network.lsp_services.len(), 2);
    assert_eq!(network.tenant(0, "u1").lsp_node_index, 0);
    assert_eq!(network.tenant(3, "u2").lsp_node_index, 3);

    let invoice: LspInvoiceRegistration = network
        .tenant_lsp(3, "u2")
        .client
        .request(
            "lsp_new_invoice",
            rpc_params![NewLspInvoiceParams {
                tenant_id: "u2".to_string(),
                invoice: fiber_json_types::NewInvoiceParams {
                    amount: 1_000_000,
                    description: Some("cross LSP hosted payment".to_string()),
                    currency: fiber_json_types::Currency::Fibd,
                    payment_preimage: None,
                    payment_hash: None,
                    expiry: Some(60 * 60),
                    fallback_address: None,
                    final_expiry_delta: None,
                    udt_type_script: None,
                    hash_algorithm: None,
                    allow_mpp: None,
                    allow_trampoline_routing: Some(true),
                },
                buffer_duration_ms: None,
            }],
        )
        .await
        .expect("create U2 hosted invoice through LSP2");
    assert_eq!(invoice.tenant_id, "u2");
    assert_eq!(invoice.hint.lsp_node_id, network.nodes[3].pubkey.into());
    let payment_hash: crate::fiber_types::Hash256 = invoice.hint.payment_hash.into();

    let response = network
        .send_payment(
            0,
            "u1",
            SendPaymentCommand {
                invoice: Some(invoice.invoice),
                max_fee_amount: Some(100_000),
                max_fee_rate: Some(1_000),
                trampoline_hops: Some(vec![network.nodes[0].pubkey, network.nodes[3].pubkey]),
                ..Default::default()
            },
        )
        .await
        .expect("send U1 payment through LSP1 and LSP2");
    assert_eq!(response.payment_hash, payment_hash.into());

    network
        .tenant(0, "u1")
        .node
        .wait_until_success(payment_hash)
        .await;
    wait_until_async_timeout(|| async {
        network
            .tenant(3, "u2")
            .node
            .get_invoice_status(&payment_hash)
            == Some(CkbInvoiceStatus::Paid)
    })
    .await;
    let delivery = wait_for_delivery_status(
        &network.tenant_lsp(3, "u2").client,
        payment_hash,
        LspPaymentDeliveryStatus::Succeeded,
    )
    .await;
    assert_eq!(delivery.tenant_id, "u2");
    assert_eq!(
        delivery.private_channel_id,
        network.tenant(3, "u2").private_channel_id.into()
    );

    let u1_payment = network
        .tenant(0, "u1")
        .node
        .get_payment_result(payment_hash)
        .await;
    assert_eq!(u1_payment.routers.len(), 1);
    let u1_used_channels = network
        .tenant(0, "u1")
        .node
        .routers_used_channels(
            &u1_payment.routers,
            &[network.tenant(0, "u1").private_channel_id],
        )
        .await;
    assert_eq!(u1_used_channels.len(), 1);

    let lsp1_payment = network.nodes[0].get_payment_result(payment_hash).await;
    assert_eq!(lsp1_payment.routers.len(), 1);
    let route_pubkeys = lsp1_payment.routers[0]
        .nodes
        .iter()
        .map(|hop| hop.pubkey)
        .collect::<Vec<_>>();
    assert!(route_pubkeys.contains(&network.nodes[1].pubkey));
    assert!(route_pubkeys.contains(&network.nodes[2].pubkey));
    assert!(route_pubkeys.contains(&network.nodes[3].pubkey));

    network.stop().await;
}
