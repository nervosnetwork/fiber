use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use fiber_store::backend::StorageBackend;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use tempfile::tempdir;

use crate::fiber::network::{
    HostedTenantActivity, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
};
use crate::fiber::trampoline::TrampolineForwardingRequest;
use crate::fiber::types::FiberMessage;
use crate::fiber_types::{
    AddTlc, Hash256, HashAlgorithm, PaymentStatus, PrevTlcInfo, Privkey, Pubkey,
};
use crate::invoice::{Currency, InvoiceBuilder};
use crate::lsp::{
    HostedTenantRecord, HostedTenantRuntime, LspConfig, LspDeliveryDecision, LspInvoiceRegistry,
    LspPaymentDeliveryLimits, LspPaymentDeliveryManager, LspPaymentDeliveryStatus, LspService,
    LspServiceArgs, LspServiceMessage, TenantId, TenantRegistry, TenantRuntimeFactory,
    TenantSupervisor, DEFAULT_LSP_BUFFER_DURATION_MS, LSP_DELIVERY_SAFETY_MARGIN_MS,
    MAX_LSP_BUFFER_DURATION_MS,
};
use crate::store::open_store;

use super::dispatcher::{HostedTenantEndpoint, HostedTenantEndpointArgs, TenantMessageDispatcher};

#[cfg(not(target_arch = "wasm32"))]
mod integration;

fn lsp_config(base_dir: PathBuf) -> LspConfig {
    LspConfig {
        base_dir: Some(base_dir),
        tenants: Vec::new(),
        max_active_tenants: 64,
        max_buffer_duration_ms: MAX_LSP_BUFFER_DURATION_MS,
        max_pending_deliveries: 1_024,
        max_pending_deliveries_per_tenant: 64,
    }
}

#[test]
fn lsp_service_uses_an_independent_store() {
    let root = tempdir().expect("temporary directory");
    let public_store_path = root.path().join("fiber/store");
    let config = lsp_config(root.path().join("lsp"));

    config
        .validate_store_separation(&public_store_path)
        .expect("separate store paths");

    std::fs::create_dir_all(&public_store_path).expect("create public store path");
    let public_store = open_store(&public_store_path).expect("open public store");
    let lsp_store = open_store(config.store_path()).expect("open LSP store");
    public_store.put(b"same-key", b"public-value");
    lsp_store.put(b"same-key", b"lsp-value");

    assert_eq!(
        public_store.get(b"same-key"),
        Some(b"public-value".to_vec())
    );
    assert_eq!(lsp_store.get(b"same-key"), Some(b"lsp-value".to_vec()));
    assert_eq!(
        config.tenant_store_root(),
        PathBuf::from(root.path()).join("lsp/tenants")
    );
}

#[test]
fn lsp_service_rejects_public_store_reuse() {
    let public_store_path = PathBuf::from("shared/store");
    let config = lsp_config(PathBuf::from("shared"));

    assert_eq!(
        config.validate_store_separation(&public_store_path),
        Err("LSP service store must be separate from the public Fiber store".to_string())
    );
}

#[test]
fn tenant_registry_is_persistent_and_idempotent() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let registry = TenantRegistry::new(store.clone());
    let record = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: Privkey::from(&[1; 32]).pubkey(),
        private_channel_id: None,
        created_at: 42,
    };

    assert_eq!(registry.register(record.clone()).unwrap(), record);
    assert_eq!(registry.register(record.clone()).unwrap(), record);
    let duplicate_key = HostedTenantRecord {
        tenant_id: TenantId::new("u2").unwrap(),
        invoice_pubkey: record.invoice_pubkey,
        private_channel_id: None,
        created_at: 43,
    };
    assert_eq!(
        registry.register(duplicate_key).unwrap_err(),
        "invoice key is already registered to tenant u1"
    );

    let reopened = TenantRegistry::new(store);
    assert_eq!(reopened.get(&record.tenant_id).unwrap(), Some(record));
    assert_eq!(reopened.list().unwrap().len(), 1);
}

struct NoopNetworkActor;

#[async_trait]
impl Actor for NoopNetworkActor {
    type Msg = NetworkActorMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let NetworkActorMessage::Command(NetworkActorCommand::GetHostedTenantActivity(reply)) =
            message
        {
            let _ = reply.send(Default::default());
        }
        Ok(())
    }
}

struct RecordingNetworkActor;

#[async_trait]
impl Actor for RecordingNetworkActor {
    type Msg = NetworkActorMessage;
    type State = Arc<Mutex<Vec<(Pubkey, Hash256)>>>;
    type Arguments = Arc<Mutex<Vec<(Pubkey, Hash256)>>>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let NetworkActorMessage::Event(NetworkActorEvent::FiberMessage(
            source,
            FiberMessage::ChannelNormalOperation(message),
            _,
        )) = message
        {
            state
                .lock()
                .unwrap()
                .push((source, message.get_channel_id()));
        }
        Ok(())
    }
}

fn test_add_tlc(channel_id: Hash256) -> FiberMessage {
    FiberMessage::add_tlc(AddTlc {
        channel_id,
        tlc_id: 0,
        amount: 1_000,
        payment_hash: Hash256::from([9; 32]),
        expiry: crate::now_timestamp_as_millis_u64() + 60_000,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
    })
}

#[tokio::test]
async fn tenant_dispatcher_isolates_same_channel_id_between_tenants() {
    let dispatcher = TenantMessageDispatcher::default();
    let public_key = Privkey::from(&[7; 32]).pubkey();
    let tenant_u1_key = Privkey::from(&[1; 32]).pubkey();
    let tenant_u2_key = Privkey::from(&[2; 32]).pubkey();
    let tenant_u1_id = TenantId::new("u1").unwrap();
    let tenant_u2_id = TenantId::new("u2").unwrap();
    let channel_id = Hash256::from([3; 32]);
    let public_messages = Arc::new(Mutex::new(Vec::new()));
    let tenant_u1_messages = Arc::new(Mutex::new(Vec::new()));
    let tenant_u2_messages = Arc::new(Mutex::new(Vec::new()));
    let public_actor = Actor::spawn(None, RecordingNetworkActor, public_messages.clone())
        .await
        .unwrap()
        .0;
    let tenant_u1_actor = Actor::spawn(None, RecordingNetworkActor, tenant_u1_messages.clone())
        .await
        .unwrap()
        .0;
    let tenant_u2_actor = Actor::spawn(None, RecordingNetworkActor, tenant_u2_messages.clone())
        .await
        .unwrap()
        .0;
    dispatcher
        .register_runtime(tenant_u1_id.clone(), tenant_u1_key, tenant_u1_actor)
        .unwrap();
    dispatcher
        .register_runtime(tenant_u2_id.clone(), tenant_u2_key, tenant_u2_actor)
        .unwrap();

    let endpoint_u1 = Actor::spawn(
        None,
        HostedTenantEndpoint,
        HostedTenantEndpointArgs {
            tenant_id: tenant_u1_id.clone(),
            invoice_pubkey: tenant_u1_key,
            public_node_id: public_key,
            public_network_actor: public_actor.clone(),
            dispatcher: dispatcher.clone(),
        },
    )
    .await
    .unwrap()
    .0;
    let endpoint_u2 = Actor::spawn(
        None,
        HostedTenantEndpoint,
        HostedTenantEndpointArgs {
            tenant_id: tenant_u2_id.clone(),
            invoice_pubkey: tenant_u2_key,
            public_node_id: public_key,
            public_network_actor: public_actor,
            dispatcher: dispatcher.clone(),
        },
    )
    .await
    .unwrap()
    .0;

    endpoint_u1
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::FiberMessage(public_key, test_add_tlc(channel_id), None),
        ))
        .unwrap();
    endpoint_u2
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::FiberMessage(public_key, test_add_tlc(channel_id), None),
        ))
        .unwrap();
    endpoint_u1
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::FiberMessage(tenant_u1_key, test_add_tlc(channel_id), None),
        ))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tenant_u1_messages.lock().unwrap().len() == 1
                && tenant_u2_messages.lock().unwrap().len() == 1
                && public_messages.lock().unwrap().len() == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all messages dispatched");

    assert_eq!(
        tenant_u1_messages.lock().unwrap().as_slice(),
        &[(public_key, channel_id)]
    );
    assert_eq!(
        tenant_u2_messages.lock().unwrap().as_slice(),
        &[(public_key, channel_id)]
    );
    assert_eq!(
        public_messages.lock().unwrap().as_slice(),
        &[(tenant_u1_key, channel_id)]
    );
    assert!(dispatcher.owns_channel(&tenant_u1_id, &channel_id));
    assert!(dispatcher.owns_channel(&tenant_u2_id, &channel_id));
}

struct FakeRuntimeFactory {
    starts: Arc<AtomicUsize>,
}

struct BusyNetworkActor;

#[async_trait]
impl Actor for BusyNetworkActor {
    type Msg = NetworkActorMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let NetworkActorMessage::Command(NetworkActorCommand::GetHostedTenantActivity(reply)) =
            message
        {
            let _ = reply.send(HostedTenantActivity {
                inflight_payments: 1,
                active_tlcs: 2,
                pending_channel_operations: 3,
            });
        }
        Ok(())
    }
}

struct BusyRuntimeFactory;

#[async_trait]
impl TenantRuntimeFactory for BusyRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            invoice_pubkey: Privkey::from(&[4; 32]).pubkey(),
            private_channel_id: None,
            created_at: 42,
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        let actor = Actor::spawn(None, BusyNetworkActor, ())
            .await
            .map_err(|error| error.to_string())?
            .0;
        Ok(HostedTenantRuntime {
            invoice_pubkey: record.invoice_pubkey,
            network_actor: actor,
            public_network_actor: None,
            transport: None,
        })
    }
}

struct RestartableRuntimeFactory {
    starts: Arc<AtomicUsize>,
    actors: Arc<Mutex<Vec<ActorRef<NetworkActorMessage>>>>,
}

#[async_trait]
impl TenantRuntimeFactory for RestartableRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            invoice_pubkey: Privkey::from(&[5; 32]).pubkey(),
            private_channel_id: None,
            created_at: 42,
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        let actor = Actor::spawn(None, NoopNetworkActor, ())
            .await
            .map_err(|error| error.to_string())?
            .0;
        self.actors.lock().unwrap().push(actor.clone());
        Ok(HostedTenantRuntime {
            invoice_pubkey: record.invoice_pubkey,
            network_actor: actor,
            public_network_actor: None,
            transport: None,
        })
    }
}

#[async_trait]
impl TenantRuntimeFactory for FakeRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        let secret = if tenant_id.as_str() == "u1" { 1 } else { 2 };
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            invoice_pubkey: Privkey::from(&[secret; 32]).pubkey(),
            private_channel_id: None,
            created_at: 42,
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        let actor = Actor::spawn(None, NoopNetworkActor, ())
            .await
            .map_err(|error| error.to_string())?
            .0;
        Ok(HostedTenantRuntime {
            invoice_pubkey: record.invoice_pubkey,
            network_actor: actor,
            public_network_actor: None,
            transport: None,
        })
    }
}

#[tokio::test]
async fn tenant_supervisor_hydrates_evicts_and_enforces_capacity() {
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory {
        starts: starts.clone(),
    });
    let mut supervisor = TenantSupervisor::new(factory.clone(), 1);
    let u1 = factory.provision(&TenantId::new("u1").unwrap()).unwrap();
    let u2 = factory.provision(&TenantId::new("u2").unwrap()).unwrap();

    supervisor.ensure(&u1).await.unwrap();
    supervisor.ensure(&u1).await.unwrap();
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    assert_eq!(supervisor.active_count(), 1);
    assert_eq!(
        supervisor.ensure(&u2).await,
        Err("active tenant limit 1 reached".to_string())
    );

    assert!(supervisor.evict(&u1.tenant_id).await.unwrap());
    assert!(!supervisor.is_active(&u1.tenant_id));
    supervisor.ensure(&u2).await.unwrap();
    assert!(supervisor.is_active(&u2.tenant_id));
    assert_eq!(starts.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn tenant_supervisor_rejects_eviction_while_runtime_is_busy() {
    let factory = Arc::new(BusyRuntimeFactory);
    let mut supervisor = TenantSupervisor::new(factory.clone(), 1);
    let tenant = factory.provision(&TenantId::new("u1").unwrap()).unwrap();
    supervisor.ensure(&tenant).await.unwrap();

    assert_eq!(
        supervisor.evict(&tenant.tenant_id).await.unwrap_err(),
        "hosted tenant runtime is busy: 1 in-flight payments, 2 active TLCs, 3 pending channel operations"
    );
    assert!(supervisor.is_active(&tenant.tenant_id));
}

#[tokio::test]
async fn tenant_supervisor_rehydrates_a_stopped_runtime() {
    let starts = Arc::new(AtomicUsize::new(0));
    let actors = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(RestartableRuntimeFactory {
        starts: starts.clone(),
        actors: actors.clone(),
    });
    let mut supervisor = TenantSupervisor::new(factory.clone(), 1);
    let tenant = factory.provision(&TenantId::new("u1").unwrap()).unwrap();
    supervisor.ensure(&tenant).await.unwrap();
    let first_actor = actors.lock().unwrap()[0].clone();
    first_actor.stop(Some("simulate tenant runtime crash".to_string()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while first_actor.get_status() < ractor::ActorStatus::Stopping {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("tenant runtime stopped");

    assert!(!supervisor.is_active(&tenant.tenant_id));
    supervisor.ensure(&tenant).await.unwrap();
    assert!(supervisor.is_active(&tenant.tenant_id));
    assert_eq!(supervisor.active_count(), 1);
    assert_eq!(starts.load(Ordering::Relaxed), 2);
}

#[test]
fn hosted_tenant_config_is_private_and_isolated() {
    let root = tempdir().expect("temporary directory");
    let public_config = crate::tests::get_fiber_config(root.path().join("public"), Some("T"));
    let u1 = public_config.hosted_tenant_config(root.path().join("lsp/tenants/u1"));
    let u2 = public_config.hosted_tenant_config(root.path().join("lsp/tenants/u2"));

    assert!(!u1.sync_network_graph());
    assert!(!u1.auto_announce_node());
    assert!(u1.in_process_transport_only());
    assert_eq!(u1.listening_addr(), "/ip4/127.0.0.1/tcp/0");
    assert_ne!(u1.store_path(), u2.store_path());
    assert_ne!(u1.public_key().inner_ref(), u2.public_key().inner_ref());

    let u1_store = open_store(u1.store_path()).expect("open U1 store");
    let u2_store = open_store(u2.store_path()).expect("open U2 store");
    u1_store.put(b"same-payment-hash", b"u1-session");
    u2_store.put(b"same-payment-hash", b"u2-session");
    assert_eq!(
        u1_store.get(b"same-payment-hash"),
        Some(b"u1-session".to_vec())
    );
    assert_eq!(
        u2_store.get(b"same-payment-hash"),
        Some(b"u2-session".to_vec())
    );
}

fn signed_invoice(signing_key: &Privkey, payment_hash: Hash256) -> crate::invoice::CkbInvoice {
    InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1_000))
        .payment_hash(payment_hash)
        .expiry_time(Duration::from_secs(60 * 60))
        .build_with_sign(|message| {
            secp256k1::SECP256K1.sign_ecdsa_recoverable(message, &signing_key.0)
        })
        .expect("build signed invoice")
}

#[test]
fn hosted_invoice_registration_is_signed_and_persistent() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };
    let payment_hash = Hash256::from([7; 32]);
    let invoice = signed_invoice(&tenant_key, payment_hash);

    let registration = invoices
        .register(&tenant, invoice, None, lsp_key.pubkey(), &lsp_key)
        .expect("register invoice");
    registration
        .hint
        .verify_for_invoice(&registration.invoice, crate::now_timestamp_as_millis_u64())
        .expect("valid hint");
    assert_eq!(
        registration.hint.payload.buffer_duration_ms,
        DEFAULT_LSP_BUFFER_DURATION_MS
    );
    assert_eq!(registration.hint.trampoline_hops(), [lsp_key.pubkey()]);

    let reopened = LspInvoiceRegistry::new(store);
    assert_eq!(reopened.get(&payment_hash).unwrap(), Some(registration));
}

#[test]
fn hosted_invoice_hint_detects_tampering_and_expiry() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };
    let mut hint = invoices
        .register(
            &tenant,
            signed_invoice(&tenant_key, Hash256::from([8; 32])),
            Some(30_000),
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap()
        .hint;

    hint.payload.payment_hash = Hash256::from([1; 32]);
    assert_eq!(
        hint.verify(crate::now_timestamp_as_millis_u64()),
        Err("invalid LSP invoice hint signature".to_string())
    );
    hint.payload.expires_at = crate::now_timestamp_as_millis_u64();
    assert_eq!(
        hint.verify(crate::now_timestamp_as_millis_u64()),
        Err("LSP invoice hint has expired".to_string())
    );
}

#[test]
fn hosted_invoice_buffer_duration_is_capped_at_seven_days() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };

    let result = invoices.register(
        &tenant,
        signed_invoice(&tenant_key, Hash256::from([15; 32])),
        Some(MAX_LSP_BUFFER_DURATION_MS + 1),
        lsp_key.pubkey(),
        &lsp_key,
    );

    assert_eq!(
        result.unwrap_err(),
        format!(
            "buffer duration exceeds maximum {}ms",
            MAX_LSP_BUFFER_DURATION_MS
        )
    );
}

#[test]
fn hosted_invoice_buffer_duration_is_shortened_by_operator_policy() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let policy_cap = 12 * 60 * 60 * 1_000;
    let invoices = LspInvoiceRegistry::with_max_buffer_duration(store, policy_cap);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };

    let registration = invoices
        .register(
            &tenant,
            signed_invoice(&tenant_key, Hash256::from([16; 32])),
            Some(DEFAULT_LSP_BUFFER_DURATION_MS),
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap();

    assert_eq!(registration.hint.payload.buffer_duration_ms, policy_cap);
}

#[test]
fn hosted_invoice_must_be_signed_by_registered_tenant() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let other_key = Privkey::from(&[4; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };

    let result = invoices.register(
        &tenant,
        signed_invoice(&other_key, Hash256::from([6; 32])),
        None,
        lsp_key.pubkey(),
        &lsp_key,
    );
    assert_eq!(
        result.unwrap_err(),
        "hosted invoice payee does not match tenant u1"
    );
}

fn hosted_forwarding_request(
    tenant: &HostedTenantRecord,
    payment_hash: Hash256,
    now: u64,
) -> TrampolineForwardingRequest {
    let downstream_expiry = 60_000;
    TrampolineForwardingRequest {
        payment_hash,
        next_node_id: tenant.invoice_pubkey,
        amount_to_forward: 1_000,
        hash_algorithm: HashAlgorithm::Sha256,
        build_max_fee_amount: 10,
        tlc_expiry_delta: downstream_expiry,
        tlc_expiry_limit: 60 * 60 * 1_000,
        max_parts: None,
        udt_type_script: None,
        remaining_trampoline_onion: vec![1, 2, 3],
        previous_tlc: PrevTlcInfo::new_with_shared_secret(Hash256::from([5; 32]), 1, 10, [6; 32]),
        max_outgoing_tlc_expiry: now + downstream_expiry + LSP_DELIVERY_SAFETY_MARGIN_MS + 120_000,
    }
}

#[test]
fn payment_delivery_deadline_preserves_downstream_expiry_budget() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(Hash256::from([21; 32])),
        created_at: 42,
    };
    let payment_hash = Hash256::from([11; 32]);
    let registration = invoices
        .register(
            &tenant,
            signed_invoice(&tenant_key, payment_hash),
            None,
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap();
    let now = crate::now_timestamp_as_millis_u64();

    let delivery = deliveries
        .accept(
            &registration,
            &tenant,
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    assert_eq!(delivery.buffer_deadline, now + 120_000);

    let reopened = LspPaymentDeliveryManager::new(store);
    assert_eq!(reopened.list_pending().unwrap(), vec![delivery]);
}

#[test]
fn in_flight_delivery_is_not_reverted_by_buffer_deadline() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(Hash256::from([22; 32])),
        created_at: 42,
    };
    let payment_hash = Hash256::from([12; 32]);
    let registration = invoices
        .register(
            &tenant,
            signed_invoice(&tenant_key, payment_hash),
            None,
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap();
    let now = crate::now_timestamp_as_millis_u64();
    deliveries
        .accept(
            &registration,
            &tenant,
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    let in_flight = deliveries
        .transition(
            &payment_hash,
            LspPaymentDeliveryStatus::Dispatching,
            now + 1,
        )
        .and_then(|_| {
            deliveries.transition(&payment_hash, LspPaymentDeliveryStatus::InFlight, now + 2)
        })
        .unwrap();

    assert_eq!(in_flight.status, LspPaymentDeliveryStatus::InFlight);
    assert_eq!(deliveries.list_pending().unwrap(), vec![in_flight]);
}

#[test]
fn payment_delivery_rejects_invalid_state_transition_and_mpp() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(Hash256::from([25; 32])),
        created_at: 42,
    };
    let payment_hash = Hash256::from([17; 32]);
    let registration = invoices
        .register(
            &tenant,
            signed_invoice(&tenant_key, payment_hash),
            None,
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap();
    let now = crate::now_timestamp_as_millis_u64();
    let mut request = hosted_forwarding_request(&tenant, payment_hash, now);
    request.max_parts = Some(2);
    assert_eq!(
        deliveries
            .accept(&registration, &tenant, request.clone(), now)
            .unwrap_err(),
        "buffered hosted delivery does not support MPP"
    );
    request.max_parts = None;
    deliveries
        .accept(&registration, &tenant, request, now)
        .unwrap();
    assert!(deliveries
        .transition(&payment_hash, LspPaymentDeliveryStatus::Succeeded, now + 1,)
        .unwrap_err()
        .contains("invalid hosted payment delivery transition"));
}

#[test]
fn payment_delivery_enforces_per_tenant_pending_limit() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_store(config.store_path()).expect("open LSP store");
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::with_limits(
        store,
        LspPaymentDeliveryLimits {
            max_pending_deliveries: 2,
            max_pending_deliveries_per_tenant: 1,
        },
    );
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(Hash256::from([26; 32])),
        created_at: 42,
    };
    let now = crate::now_timestamp_as_millis_u64();
    for (index, payment_hash) in [Hash256::from([18; 32]), Hash256::from([19; 32])]
        .into_iter()
        .enumerate()
    {
        let registration = invoices
            .register(
                &tenant,
                signed_invoice(&tenant_key, payment_hash),
                None,
                lsp_key.pubkey(),
                &lsp_key,
            )
            .unwrap();
        let result = deliveries.accept(
            &registration,
            &tenant,
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        );
        if index == 0 {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err(),
                "pending hosted delivery limit reached for tenant u1"
            );
        }
    }
}

struct MockPublicNetworkActor;

struct MockPublicNetworkState {
    dispatches: Arc<AtomicUsize>,
    dispatch_failures_remaining: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
    upstream_pending: Arc<AtomicBool>,
}

#[async_trait]
impl Actor for MockPublicNetworkActor {
    type Msg = NetworkActorMessage;
    type State = MockPublicNetworkState;
    type Arguments = MockPublicNetworkState;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let NetworkActorMessage::Command(command) = message else {
            return Ok(());
        };
        match command {
            NetworkActorCommand::GetPayment(_, reply) => {
                let _ = reply.send(Err("payment not started".to_string()));
            }
            NetworkActorCommand::InspectBufferedTrampolineUpstream { reply, .. } => {
                let status = if state.upstream_pending.load(Ordering::Relaxed) {
                    crate::fiber::network::BufferedTrampolineUpstreamStatus::Pending
                } else {
                    crate::fiber::network::BufferedTrampolineUpstreamStatus::Removed
                };
                let _ = reply.send(status);
            }
            NetworkActorCommand::DispatchBufferedTrampoline { reply, .. } => {
                state.dispatches.fetch_add(1, Ordering::Relaxed);
                let should_fail = state
                    .dispatch_failures_remaining
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok();
                let _ = if should_fail {
                    reply.send(Err("temporary dispatch failure".to_string()))
                } else {
                    reply.send(Ok(()))
                };
            }
            NetworkActorCommand::ReconcileBufferedTrampolineSettlement { reply, .. } => {
                let _ = reply.send(Ok(()));
            }
            NetworkActorCommand::FailBufferedTrampoline { reply, .. } => {
                state.failures.fetch_add(1, Ordering::Relaxed);
                let _ = reply.send(Ok(true));
            }
            _ => {}
        }
        Ok(())
    }
}

async fn start_test_lsp_service(
    store: crate::store::Store,
    config: LspConfig,
    factory: Arc<dyn TenantRuntimeFactory>,
    lsp_key: Privkey,
    dispatches: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
) -> ActorRef<LspServiceMessage> {
    start_test_lsp_service_with_dispatch_failures(
        store,
        config,
        factory,
        lsp_key,
        dispatches,
        Arc::new(AtomicUsize::new(0)),
        failures,
    )
    .await
}

async fn start_test_lsp_service_with_dispatch_failures(
    store: crate::store::Store,
    config: LspConfig,
    factory: Arc<dyn TenantRuntimeFactory>,
    lsp_key: Privkey,
    dispatches: Arc<AtomicUsize>,
    dispatch_failures_remaining: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
) -> ActorRef<LspServiceMessage> {
    start_test_lsp_service_with_upstream(
        store,
        config,
        factory,
        lsp_key,
        dispatches,
        dispatch_failures_remaining,
        failures,
        Arc::new(AtomicBool::new(true)),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_test_lsp_service_with_upstream(
    store: crate::store::Store,
    config: LspConfig,
    factory: Arc<dyn TenantRuntimeFactory>,
    lsp_key: Privkey,
    dispatches: Arc<AtomicUsize>,
    dispatch_failures_remaining: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
    upstream_pending: Arc<AtomicBool>,
) -> ActorRef<LspServiceMessage> {
    let public_network_actor = Actor::spawn(
        None,
        MockPublicNetworkActor,
        MockPublicNetworkState {
            dispatches,
            dispatch_failures_remaining,
            failures,
            upstream_pending,
        },
    )
    .await
    .unwrap()
    .0;
    Actor::spawn(
        None,
        LspService,
        LspServiceArgs {
            config,
            public_node_id: lsp_key.pubkey(),
            public_network_actor,
            store,
            runtime_factory: factory,
            signing_key: lsp_key,
        },
    )
    .await
    .unwrap()
    .0
}

async fn register_test_invoice(
    service: &ActorRef<LspServiceMessage>,
    tenant_key: &Privkey,
    payment_hash: Hash256,
) {
    register_test_invoice_with_buffer(service, tenant_key, payment_hash, None).await;
}

async fn register_test_invoice_with_buffer(
    service: &ActorRef<LspServiceMessage>,
    tenant_key: &Privkey,
    payment_hash: Hash256,
    buffer_duration_ms: Option<u64>,
) {
    register_test_invoice_for_tenant(
        service,
        TenantId::new("u1").unwrap(),
        tenant_key,
        payment_hash,
        buffer_duration_ms,
    )
    .await;
}

async fn register_test_invoice_for_tenant(
    service: &ActorRef<LspServiceMessage>,
    tenant_id: TenantId,
    tenant_key: &Privkey,
    payment_hash: Hash256,
    buffer_duration_ms: Option<u64>,
) {
    ractor::call!(service, |reply| LspServiceMessage::RegisterInvoice {
        tenant_id,
        invoice: signed_invoice(tenant_key, payment_hash),
        buffer_duration_ms,
        reply,
    })
    .unwrap()
    .unwrap();
}

async fn wait_for_delivery_status(
    manager: &LspPaymentDeliveryManager<crate::store::Store>,
    payment_hash: Hash256,
    expected: LspPaymentDeliveryStatus,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager
                .get(&payment_hash)
                .unwrap()
                .is_some_and(|delivery| delivery.status == expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("delivery reached expected status");
}

#[tokio::test]
async fn cold_tenant_delivery_dispatches_only_after_channel_online() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory { starts });
    let dispatches = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        lsp_key,
        dispatches.clone(),
        failures.clone(),
    )
    .await;
    let payment_hash = Hash256::from([13; 32]);
    register_test_invoice(&service, &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([23; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };
    let now = crate::now_timestamp_as_millis_u64();

    assert_eq!(
        ractor::call!(service, |reply| {
            LspServiceMessage::AcceptTrampolineDelivery(
                hosted_forwarding_request(&tenant, payment_hash, now),
                reply,
            )
        })
        .unwrap()
        .unwrap(),
        crate::lsp::LspDeliveryDecision::Buffered
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(dispatches.load(Ordering::Relaxed), 0);

    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant.invoice_pubkey,
            private_channel_id,
        ))
        .unwrap();
    wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::InFlight).await;
    assert_eq!(dispatches.load(Ordering::Relaxed), 1);

    service
        .send_message(LspServiceMessage::ExpireDelivery(payment_hash))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn offline_tenant_does_not_block_an_online_tenant() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string(), "u2".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory {
        starts: starts.clone(),
    });
    let dispatches = Arc::new(AtomicUsize::new(0));
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let u1_key = Privkey::from(&[1; 32]);
    let u2_key = Privkey::from(&[2; 32]);
    let u1_payment_hash = Hash256::from([31; 32]);
    let u2_payment_hash = Hash256::from([32; 32]);
    register_test_invoice_for_tenant(
        &service,
        TenantId::new("u1").unwrap(),
        &u1_key,
        u1_payment_hash,
        None,
    )
    .await;
    register_test_invoice_for_tenant(
        &service,
        TenantId::new("u2").unwrap(),
        &u2_key,
        u2_payment_hash,
        None,
    )
    .await;
    let u1_channel = Hash256::from([33; 32]);
    let u2_channel = Hash256::from([34; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            u1_key.pubkey(),
            u1_channel,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            u1_key.pubkey(),
            u1_channel,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            u2_key.pubkey(),
            u2_channel,
        ))
        .unwrap();
    let u1 = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: u1_key.pubkey(),
        private_channel_id: Some(u1_channel),
        created_at: 42,
    };
    let u2 = HostedTenantRecord {
        tenant_id: TenantId::new("u2").unwrap(),
        invoice_pubkey: u2_key.pubkey(),
        private_channel_id: Some(u2_channel),
        created_at: 42,
    };
    let now = crate::now_timestamp_as_millis_u64();
    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&u1, u1_payment_hash, now),
            reply,
        )
    })
    .unwrap()
    .unwrap();
    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&u2, u2_payment_hash, now),
            reply,
        )
    })
    .unwrap()
    .unwrap();

    wait_for_delivery_status(
        &manager,
        u2_payment_hash,
        LspPaymentDeliveryStatus::InFlight,
    )
    .await;
    assert_eq!(
        manager.get(&u1_payment_hash).unwrap().unwrap().status,
        LspPaymentDeliveryStatus::Deferred
    );
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    assert_eq!(dispatches.load(Ordering::Relaxed), 1);

    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            u1_key.pubkey(),
            u1_channel,
        ))
        .unwrap();
    wait_for_delivery_status(
        &manager,
        u1_payment_hash,
        LspPaymentDeliveryStatus::InFlight,
    )
    .await;
    assert_eq!(starts.load(Ordering::Relaxed), 2);
    assert_eq!(dispatches.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn cold_tenant_delivery_fails_at_buffer_deadline() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory {
        starts: Arc::new(AtomicUsize::new(0)),
    });
    let dispatches = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        failures.clone(),
    )
    .await;
    let payment_hash = Hash256::from([14; 32]);
    register_test_invoice(&service, &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([24; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };
    let now = crate::now_timestamp_as_millis_u64();
    let mut request = hosted_forwarding_request(&tenant, payment_hash, now);
    request.max_outgoing_tlc_expiry =
        now + request.tlc_expiry_delta + LSP_DELIVERY_SAFETY_MARGIN_MS + 200;

    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(request, reply)
    })
    .unwrap()
    .unwrap();
    wait_for_delivery_status(
        &manager,
        payment_hash,
        LspPaymentDeliveryStatus::Failed {
            reason: "hosted tenant was unavailable before the buffer deadline".to_string(),
        },
    )
    .await;
    assert_eq!(dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(failures.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn restart_cancels_deferred_delivery_after_upstream_tlc_removal() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory: Arc<dyn TenantRuntimeFactory> = Arc::new(FakeRuntimeFactory {
        starts: Arc::new(AtomicUsize::new(0)),
    });
    let dispatches = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let upstream_pending = Arc::new(AtomicBool::new(true));
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service_with_upstream(
        store.clone(),
        config.clone(),
        factory.clone(),
        lsp_key.clone(),
        dispatches.clone(),
        Arc::new(AtomicUsize::new(0)),
        failures.clone(),
        upstream_pending.clone(),
    )
    .await;
    let payment_hash = Hash256::from([41; 32]);
    register_test_invoice(&service, &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([42; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };
    let now = crate::now_timestamp_as_millis_u64();
    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&tenant, payment_hash, now),
            reply,
        )
    })
    .unwrap()
    .unwrap();
    wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::Deferred).await;

    service.stop(None);
    tokio::time::sleep(Duration::from_millis(50)).await;
    upstream_pending.store(false, Ordering::Relaxed);

    let reopened = start_test_lsp_service_with_upstream(
        store,
        config,
        factory,
        lsp_key,
        dispatches.clone(),
        Arc::new(AtomicUsize::new(0)),
        failures.clone(),
        upstream_pending,
    )
    .await;
    wait_for_delivery_status(
        &manager,
        payment_hash,
        LspPaymentDeliveryStatus::Cancelled {
            reason: "upstream TLC was removed before hosted delivery dispatch".to_string(),
        },
    )
    .await;
    assert_eq!(dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(failures.load(Ordering::Relaxed), 0);
    assert!(!manager.has_pending_for_tenant(&tenant.tenant_id).unwrap());
    reopened.stop(None);
}

#[tokio::test]
async fn zero_buffer_hint_keeps_immediate_trampoline_behavior() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory {
        starts: starts.clone(),
    });
    let dispatches = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        failures.clone(),
    )
    .await;
    let payment_hash = Hash256::from([20; 32]);
    register_test_invoice_with_buffer(&service, &tenant_key, payment_hash, Some(0)).await;
    let private_channel_id = Hash256::from([27; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };

    assert_eq!(
        ractor::call!(service, |reply| {
            LspServiceMessage::AcceptTrampolineDelivery(
                hosted_forwarding_request(
                    &tenant,
                    payment_hash,
                    crate::now_timestamp_as_millis_u64(),
                ),
                reply,
            )
        })
        .unwrap()
        .unwrap(),
        LspDeliveryDecision::NotHosted
    );
    assert_eq!(manager.get(&payment_hash).unwrap(), None);
    assert_eq!(starts.load(Ordering::Relaxed), 0);
    assert_eq!(dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn tenant_with_pending_delivery_cannot_be_evicted() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let factory = Arc::new(FakeRuntimeFactory {
        starts: Arc::new(AtomicUsize::new(0)),
    });
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let payment_hash = Hash256::from([21; 32]);
    register_test_invoice(&service, &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([28; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };
    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64()),
            reply,
        )
    })
    .unwrap()
    .unwrap();

    assert_eq!(
        ractor::call!(service, |reply| LspServiceMessage::EvictTenant(
            TenantId::new("u1").unwrap(),
            reply,
        ))
        .unwrap()
        .unwrap_err(),
        "tenant u1 has unfinished hosted payment deliveries"
    );
}

#[tokio::test]
async fn transient_dispatch_failure_returns_to_deferred_and_retries() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory {
        starts: Arc::new(AtomicUsize::new(0)),
    });
    let dispatches = Arc::new(AtomicUsize::new(0));
    let upstream_failures = Arc::new(AtomicUsize::new(0));
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service_with_dispatch_failures(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        Arc::new(AtomicUsize::new(1)),
        upstream_failures.clone(),
    )
    .await;
    let payment_hash = Hash256::from([22; 32]);
    register_test_invoice(&service, &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([29; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };

    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64()),
            reply,
        )
    })
    .unwrap()
    .unwrap();

    wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::InFlight).await;
    assert_eq!(dispatches.load(Ordering::Relaxed), 2);
    assert_eq!(upstream_failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn downstream_outcome_is_persisted_before_upstream_settlement() {
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.tenants = vec!["u1".to_string()];
    let store = open_store(config.store_path()).expect("open LSP store");
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory {
        starts: Arc::new(AtomicUsize::new(0)),
    });
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store.clone(),
        config,
        factory,
        Privkey::from(&[9; 32]),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let payment_hash = Hash256::from([23; 32]);
    register_test_invoice(&service, &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([30; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        invoice_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };
    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64()),
            reply,
        )
    })
    .unwrap()
    .unwrap();
    manager
        .transition(
            &payment_hash,
            LspPaymentDeliveryStatus::Dispatching,
            crate::now_timestamp_as_millis_u64(),
        )
        .and_then(|_| {
            manager.transition(
                &payment_hash,
                LspPaymentDeliveryStatus::InFlight,
                crate::now_timestamp_as_millis_u64(),
            )
        })
        .unwrap();

    for _ in 0..2 {
        ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady {
            payment_hash,
            payment_status: PaymentStatus::Success,
            failure: None,
            reply,
        })
        .unwrap()
        .unwrap();
    }
    let settling = manager.get(&payment_hash).unwrap().unwrap();
    assert_eq!(
        settling.status,
        LspPaymentDeliveryStatus::SettlingUpstream {
            payment_status: PaymentStatus::Success,
            failure: None,
        }
    );
    let reopened = LspPaymentDeliveryManager::new(store);
    assert_eq!(reopened.get(&payment_hash).unwrap(), Some(settling));

    service
        .send_message(LspServiceMessage::PaymentOutcomeSettled {
            payment_hash,
            payment_status: PaymentStatus::Success,
            failure: None,
        })
        .unwrap();
    wait_for_delivery_status(&reopened, payment_hash, LspPaymentDeliveryStatus::Succeeded).await;
}
