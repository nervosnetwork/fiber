use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use ractor::{Actor, ActorRef};
use secp256k1::SECP256K1;
use tempfile::tempdir;

use super::lsp_config;
use crate::{
    fiber::{
        network::{NetworkActorCommand, NetworkActorMessage},
        payment::SendPaymentCommand,
    },
    gen_rand_sha256_hash,
    invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder},
    lsp::{
        HostedTenantRecord, HostedTenantRuntime, LspService, LspServiceArgs, TenantId,
        TenantRuntimeFactory,
    },
    rpc::{
        lsp::{
            ListLspTenantsResult, LspInvoiceRegistration, LspPaymentDelivery,
            LspPaymentDeliveryStatus, LspPaymentHashParams, LspServiceStatus, LspTenantParams,
            LspTenantRuntimeStatus, RegisterLspInvoiceParams,
        },
        server::start_rpc,
    },
    store::open_store,
    tests::{
        establish_channel_between_nodes, gen_rpc_config, get_test_root_actor, init_tracing,
        wait_until_async_timeout, wait_until_node_supports_trampoline_routing, ChannelParameters,
        NetworkNode, HUGE_CKB_AMOUNT,
    },
    NetworkServiceEvent,
};

const TENANT_ID: &str = "u1";
const BUFFER_DURATION_MS: u64 = 120_000;

struct ExistingRuntimeFactory {
    record: HostedTenantRecord,
    network_actor: ActorRef<NetworkActorMessage>,
    starts: Arc<AtomicUsize>,
}

#[async_trait]
impl TenantRuntimeFactory for ExistingRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        if tenant_id != &self.record.tenant_id {
            return Err(format!("unknown integration-test tenant {tenant_id}"));
        }
        Ok(self.record.clone())
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        if record.tenant_id != self.record.tenant_id
            || record.invoice_pubkey != self.record.invoice_pubkey
        {
            return Err("tenant record changed after registration".to_string());
        }
        self.starts.fetch_add(1, Ordering::Relaxed);
        Ok(HostedTenantRuntime {
            invoice_pubkey: record.invoice_pubkey,
            network_actor: self.network_actor.clone(),
            public_network_actor: None,
        })
    }
}

async fn register_in_process_peer(local: &NetworkNode, remote: &NetworkNode) {
    ractor::call_t!(
        local.network_actor,
        |reply| NetworkActorMessage::new_command(NetworkActorCommand::RegisterInProcessPeer {
            pubkey: remote.pubkey,
            actor: remote.network_actor.clone(),
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
        |reply| NetworkActorMessage::new_command(NetworkActorCommand::ActivateInProcessPeer(
            remote.pubkey,
            reply,
        )),
        5_000
    )
    .expect("activate in-process peer reply")
    .expect("activate in-process peer");
}

async fn connect_in_process(left: &NetworkNode, right: &NetworkNode) {
    register_in_process_peer(left, right).await;
    register_in_process_peer(right, left).await;
    activate_in_process_peer(left, right).await;
    activate_in_process_peer(right, left).await;
}

fn disconnect_in_process(left: &NetworkNode, right: &NetworkNode) {
    left.network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::UnregisterInProcessPeer(right.pubkey),
        ))
        .expect("unregister right in-process peer");
    right
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::UnregisterInProcessPeer(left.pubkey),
        ))
        .expect("unregister left in-process peer");
}

async fn wait_for_tenant_channel(client: &HttpClient, expected_online: bool) {
    wait_until_async_timeout(|| async {
        let result: ListLspTenantsResult = client
            .request("lsp_list_tenants", rpc_params![])
            .await
            .expect("list hosted tenants");
        result
            .tenants
            .iter()
            .any(|tenant| tenant.tenant_id == TENANT_ID && tenant.channel_online == expected_online)
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

#[tokio::test]
async fn hosted_payment_buffers_offline_private_channel_and_resumes_via_rpc() {
    init_tracing();

    // Keep the payer connected only to Public T. U is reachable solely through
    // the private T-U channel, matching the hosted LSP topology.
    let mut payer = NetworkNode::new_with_node_name("lsp-payer").await;
    let mut public_t = NetworkNode::new_with_node_name("lsp-public-t").await;
    let mut tenant = NetworkNode::new_with_node_name("lsp-tenant-u1").await;
    payer.connect_to(&mut public_t).await;
    connect_in_process(&public_t, &tenant).await;

    let root = tempdir().expect("temporary LSP directory");
    let config = lsp_config(root.path().join("lsp"));
    let lsp_store = open_store(config.store_path()).expect("open LSP store");
    let starts = Arc::new(AtomicUsize::new(0));
    let tenant_record = HostedTenantRecord {
        tenant_id: TenantId::new(TENANT_ID).unwrap(),
        invoice_pubkey: tenant.pubkey,
        private_channel_id: None,
        created_at: crate::now_timestamp_as_millis_u64(),
    };
    let runtime_factory = Arc::new(ExistingRuntimeFactory {
        record: tenant_record,
        network_actor: tenant.network_actor.clone(),
        starts: starts.clone(),
    });
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
            NetworkActorCommand::SetLspService(lsp_actor.clone()),
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

    let registered: crate::rpc::lsp::LspTenantStatus = client
        .request(
            "lsp_register_tenant",
            rpc_params![LspTenantParams {
                tenant_id: TENANT_ID.to_string(),
            }],
        )
        .await
        .expect("register hosted tenant");
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
    wait_for_tenant_channel(&client, true).await;
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
        .allow_trampoline_routing(true)
        .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &tenant.private_key.0))
        .expect("build hosted invoice");
    tenant.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    let registration: LspInvoiceRegistration = client
        .request(
            "lsp_register_invoice",
            rpc_params![RegisterLspInvoiceParams {
                tenant_id: TENANT_ID.to_string(),
                invoice: invoice.to_string(),
                buffer_duration_ms: Some(BUFFER_DURATION_MS),
            }],
        )
        .await
        .expect("register hosted invoice");
    assert_eq!(registration.tenant_id, TENANT_ID);
    assert_eq!(registration.hint.lsp_node_id, public_t.pubkey.into());
    assert_eq!(registration.hint.payment_hash, payment_hash.into());
    assert_eq!(registration.hint.buffer_duration_ms, BUFFER_DURATION_MS);

    let stored_registration: LspInvoiceRegistration = client
        .request(
            "lsp_get_invoice_registration",
            rpc_params![LspPaymentHashParams {
                payment_hash: payment_hash.into(),
            }],
        )
        .await
        .expect("read hosted invoice registration");
    assert_eq!(stored_registration.invoice, invoice.to_string());
    assert_eq!(
        stored_registration.hint.signature,
        registration.hint.signature
    );

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
    wait_for_tenant_channel(&client, false).await;

    let response = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(500),
            trampoline_hops: Some(vec![public_t.pubkey]),
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
    wait_for_tenant_channel(&client, true).await;

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
