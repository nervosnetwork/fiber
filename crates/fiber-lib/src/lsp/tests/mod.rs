use std::{
    collections::HashMap,
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
    FiberActorCommand, FiberActorMessage, HostedTenantActivity, NetworkActorMessage,
};
use crate::fiber_types::{
    Hash256, HashAlgorithm, PaymentStatus, PrevTlcInfo, Privkey, TenantRegistryPayload,
    TenantRegistrySignature, TlcErrorCode,
};
use crate::invoice::{Currency, InvoiceBuilder};
use crate::lsp::TrampolineForwardingRequest;
use crate::lsp::{
    is_permanent_hosted_payment_failure, BiscuitTokenIssuer, HostedTenantRecord,
    HostedTenantRuntime, LspConfig, LspDeliveryDecision, LspInvoiceRegistry,
    LspPaymentDeliveryLimits, LspPaymentDeliveryManager, LspPaymentDeliveryStatus,
    LspPaymentDispatchError, LspPaymentOutcomeDecision, LspService, LspServiceArgs,
    LspServiceMessage, TenantId, TenantRegistry, TenantRuntimeFactory, TenantSupervisor,
    DEFAULT_LSP_BUFFER_DURATION_MS, LSP_DELIVERY_SAFETY_MARGIN_MS, MAX_LSP_BUFFER_DURATION_MS,
};
use crate::store::{open_store, NodeNamespace, Store};

#[cfg(not(target_arch = "wasm32"))]
mod integration;

fn test_token_issuer() -> BiscuitTokenIssuer {
    let root = biscuit_auth::KeyPair::new();
    BiscuitTokenIssuer::from_private_key(
        &root.private().to_prefixed_string(),
        &root.public().to_string(),
    )
    .expect("test biscuit issuer")
}

fn lsp_config(base_dir: PathBuf) -> LspConfig {
    LspConfig {
        base_dir: Some(base_dir),
        max_active_tenants: 64,
        max_buffer_duration_ms: MAX_LSP_BUFFER_DURATION_MS,
        max_pending_deliveries: 1_024,
        max_pending_deliveries_per_tenant: 64,
    }
}

fn open_lsp_store(config: &LspConfig) -> Store {
    std::fs::create_dir_all(config.base_dir()).expect("create test LSP base directory");
    open_store(config.base_dir().join("fiber-store"))
        .expect("open shared Fiber store")
        .namespaced(NodeNamespace::lsp_metadata())
}

#[test]
fn lsp_service_uses_a_namespace_in_the_fiber_store() {
    let root = tempdir().expect("temporary directory");
    let public_store_path = root.path().join("fiber/store");
    let config = lsp_config(root.path().join("lsp"));

    std::fs::create_dir_all(&public_store_path).expect("create public store path");
    let public_store = open_store(&public_store_path).expect("open public store");
    let lsp_store = public_store.namespaced(NodeNamespace::lsp_metadata());
    let tenant_store = public_store.namespaced(NodeNamespace::hosted_tenant("u1"));
    public_store.put(b"same-key", b"public-value");
    lsp_store.put(b"same-key", b"lsp-value");
    tenant_store.put(b"same-key", b"tenant-value");

    assert_eq!(
        public_store.get(b"same-key"),
        Some(b"public-value".to_vec())
    );
    assert_eq!(lsp_store.get(b"same-key"), Some(b"lsp-value".to_vec()));
    assert_eq!(
        tenant_store.get(b"same-key"),
        Some(b"tenant-value".to_vec())
    );
    assert_eq!(
        config.tenant_store_root(),
        PathBuf::from(root.path()).join("lsp/tenants")
    );
}

#[test]
fn tenant_registry_is_persistent_and_idempotent() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let record = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[1; 32]).pubkey(),
        private_channel_id: None,
        created_at: 42,
    };

    assert_eq!(registry.register(record.clone()).unwrap(), record);
    assert_eq!(registry.register(record.clone()).unwrap(), record);
    let duplicate_key = HostedTenantRecord {
        tenant_id: TenantId::new("u2").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: record.tenant_pubkey,
        private_channel_id: None,
        created_at: 43,
    };
    assert_eq!(
        registry.register(duplicate_key).unwrap_err(),
        "protocol key is already registered to tenant u1"
    );

    let reopened = TenantRegistry::new(store);
    assert_eq!(reopened.get(&record.tenant_id).unwrap(), Some(record));
    assert_eq!(reopened.list().unwrap().len(), 1);
}

#[cfg(feature = "watchtower")]
#[test]
fn ensure_hosted_watch_skips_an_existing_row() {
    use crate::fiber_types::SettlementData;
    use crate::watchtower::WatchtowerStore;

    let root = tempdir().expect("temporary directory");
    std::fs::create_dir_all(root.path().join("fiber")).expect("create store dir");
    let store = open_store(root.path().join("fiber/store")).expect("open store");
    let record = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[1; 32]).pubkey(),
        private_channel_id: None,
        created_at: 42,
    };
    let channel_id = Hash256::from([9u8; 32]);
    let node_id = crate::lsp::tenant_watchtower_node_id(&record.tenant_pubkey);
    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        None,
        record.tenant_pubkey,
        Privkey::from(&[2; 32]).pubkey(),
        record.tenant_pubkey,
        Privkey::from(&[3; 32]).pubkey(),
        SettlementData {
            local_amount: 11,
            remote_amount: 22,
            tlcs: Vec::new(),
        },
    );

    super::watch::ensure_hosted_watch_channel(&store, &record, channel_id).unwrap();

    let watched = store
        .get_watch_channel(&node_id, &channel_id)
        .expect("watch row remains");
    assert_eq!(watched.local_settlement_data.local_amount, 11);
    assert_eq!(watched.local_settlement_data.remote_amount, 22);
}

#[test]
fn authenticated_tenant_registry_replaces_consumes_and_persists_nonces() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let root_signer_pubkey = Privkey::from(&[7; 32]).pubkey();
    let tenant_id = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
    let first = registry
        .issue_registration_nonce(&root_signer_pubkey)
        .unwrap();
    let second = registry
        .issue_registration_nonce(&root_signer_pubkey)
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        registry.registration_nonce(&root_signer_pubkey).unwrap(),
        Some(second)
    );
    let record = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: Some(root_signer_pubkey),
        tenant_pubkey: Privkey::from(&[8; 32]).pubkey(),
        private_channel_id: None,
        created_at: 42,
    };
    assert!(registry
        .register_authenticated(record.clone(), first)
        .unwrap_err()
        .contains("missing, replaced, or consumed"));
    assert_eq!(
        registry
            .register_authenticated(record.clone(), second)
            .unwrap(),
        record
    );
    assert_eq!(
        registry.registration_nonce(&root_signer_pubkey).unwrap(),
        None
    );

    let reopened = TenantRegistry::new(store);
    assert_eq!(reopened.get(&tenant_id).unwrap(), Some(record.clone()));
    assert!(reopened
        .register_authenticated(record, second)
        .unwrap_err()
        .contains("already registered"));
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
        if let NetworkActorMessage::Fiber(FiberActorMessage::Command(
            FiberActorCommand::GetHostedTenantActivity(reply),
        )) = message
        {
            let _ = reply.send(Default::default());
        }
        Ok(())
    }
}

struct FakeRuntimeFactory {
    starts: Arc<AtomicUsize>,
    protocol_secrets: HashMap<TenantId, u8>,
    default_protocol_secret: u8,
}

impl FakeRuntimeFactory {
    fn new(starts: Arc<AtomicUsize>) -> Self {
        Self {
            starts,
            protocol_secrets: HashMap::new(),
            default_protocol_secret: 1,
        }
    }

    fn with_protocol_secret(mut self, tenant_id: TenantId, secret: u8) -> Self {
        self.protocol_secrets.insert(tenant_id, secret);
        self
    }

    fn protocol_secret(&self, tenant_id: &TenantId) -> u8 {
        self.protocol_secrets
            .get(tenant_id)
            .copied()
            .unwrap_or(self.default_protocol_secret)
    }
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
        if let NetworkActorMessage::Fiber(FiberActorMessage::Command(
            FiberActorCommand::GetHostedTenantActivity(reply),
        )) = message
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
            root_signer_pubkey: None,
            tenant_pubkey: Privkey::from(&[4; 32]).pubkey(),
            private_channel_id: None,
            created_at: 42,
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        let actor = Actor::spawn(None, BusyNetworkActor, ())
            .await
            .map_err(|error| error.to_string())?
            .0;
        Ok(HostedTenantRuntime::network_backed(
            record.tenant_pubkey,
            actor,
        ))
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
            root_signer_pubkey: None,
            tenant_pubkey: Privkey::from(&[5; 32]).pubkey(),
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
        Ok(HostedTenantRuntime::network_backed(
            record.tenant_pubkey,
            actor,
        ))
    }
}

#[async_trait]
impl TenantRuntimeFactory for FakeRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        let secret = self.protocol_secret(tenant_id);
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            root_signer_pubkey: None,
            tenant_pubkey: Privkey::from(&[secret; 32]).pubkey(),
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
        Ok(HostedTenantRuntime::network_backed(
            record.tenant_pubkey,
            actor,
        ))
    }
}

#[tokio::test]
async fn tenant_supervisor_hydrates_evicts_and_enforces_capacity() {
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory::new(starts.clone()));
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
fn hosted_tenant_config_is_private() {
    let root = tempdir().expect("temporary directory");
    let public_config = crate::tests::get_fiber_config(root.path().join("public"), Some("T"));
    let u1 = public_config.hosted_tenant_config(root.path().join("lsp/tenants/u1"));
    let u2 = public_config.hosted_tenant_config(root.path().join("lsp/tenants/u2"));

    assert!(!u1.sync_network_graph());
    assert!(!u1.auto_announce_node());
    assert_eq!(u1.listening_addr(), "/ip4/127.0.0.1/tcp/0");
    assert_ne!(u1.store_path(), u2.store_path());
    assert_ne!(u1.public_key().inner_ref(), u2.public_key().inner_ref());
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
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
fn hosted_invoice_route_hint_must_match_public_t() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let other_lsp_key = Privkey::from(&[10; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        private_channel_id: None,
        created_at: 42,
    };
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1_000))
        .payment_hash(Hash256::from([14; 32]))
        .expiry_time(Duration::from_secs(60 * 60))
        .trampoline_route_hint(other_lsp_key.pubkey().into())
        .build_with_sign(|message| {
            secp256k1::SECP256K1.sign_ecdsa_recoverable(message, &tenant_key.0)
        })
        .expect("build hosted invoice");

    assert_eq!(
        invoices
            .register(&tenant, invoice, None, lsp_key.pubkey(), &lsp_key)
            .unwrap_err(),
        "hosted invoice trampoline route hint does not match Public T"
    );
}

#[test]
fn hosted_invoice_hint_detects_tampering_and_expiry() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let store = open_lsp_store(&config);
    let policy_cap = 12 * 60 * 60 * 1_000;
    let invoices = LspInvoiceRegistry::with_max_buffer_duration(store, policy_cap);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let other_key = Privkey::from(&[4; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let incoming_tlc_id = u64::from_be_bytes(
        payment_hash.as_ref()[..8]
            .try_into()
            .expect("payment hash prefix"),
    );
    TrampolineForwardingRequest {
        payment_hash,
        next_node_id: tenant.tenant_pubkey,
        amount_to_forward: 1_000,
        hash_algorithm: HashAlgorithm::Sha256,
        build_max_fee_amount: 10,
        tlc_expiry_delta: downstream_expiry,
        tlc_expiry_limit: 60 * 60 * 1_000,
        max_parts: None,
        udt_type_script: None,
        remaining_trampoline_onion: vec![1, 2, 3],
        previous_tlc: PrevTlcInfo::new_with_shared_secret(
            Hash256::from([5; 32]),
            incoming_tlc_id,
            10,
            [6; 32],
        ),
        max_outgoing_tlc_expiry: now + downstream_expiry + LSP_DELIVERY_SAFETY_MARGIN_MS + 120_000,
    }
}

#[test]
fn payment_delivery_deadline_preserves_downstream_expiry_budget() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
fn payment_delivery_uses_incoming_tlc_as_primary_key() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(Hash256::from([22; 32])),
        created_at: 42,
    };
    let payment_hash = Hash256::from([29; 32]);
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
    let first_request = hosted_forwarding_request(&tenant, payment_hash, now);
    let first = deliveries
        .accept(&registration, &tenant, first_request.clone(), now)
        .unwrap();

    assert_eq!(
        deliveries
            .accept(&registration, &tenant, first_request.clone(), now + 1)
            .unwrap(),
        first
    );
    let mut conflicting_request = first_request.clone();
    conflicting_request.remaining_trampoline_onion.push(4);
    assert_eq!(
        deliveries
            .accept(&registration, &tenant, conflicting_request, now + 2)
            .unwrap_err(),
        "hosted payment execution already exists with different data"
    );
    let mut second_request = first_request.clone();
    second_request.previous_tlc.prev_tlc_id += 1;
    assert!(deliveries
        .accept(&registration, &tenant, second_request.clone(), now + 3,)
        .unwrap_err()
        .contains("multiple active incoming TLCs"));

    deliveries
        .transition(
            &first.key(),
            LspPaymentDeliveryStatus::Failed {
                reason: "payer replaced the incoming TLC".to_string(),
            },
            now + 4,
        )
        .unwrap();
    let second = deliveries
        .accept(&registration, &tenant, second_request.clone(), now + 5)
        .unwrap();
    assert_ne!(first.key(), second.key());
    assert_eq!(
        deliveries
            .get_by_payment_hash(&payment_hash)
            .unwrap()
            .unwrap()
            .key(),
        second.key()
    );

    deliveries
        .transition(
            &second.key(),
            LspPaymentDeliveryStatus::Failed {
                reason: "payer replaced the incoming channel".to_string(),
            },
            now + 6,
        )
        .unwrap();
    let mut third_request = first_request;
    third_request.previous_tlc.prev_channel_id = Hash256::from([30; 32]);
    let third = deliveries
        .accept(&registration, &tenant, third_request, now + 7)
        .unwrap();
    assert_ne!(first.key(), third.key());
    assert_ne!(second.key(), third.key());

    deliveries
        .transition(&third.key(), LspPaymentDeliveryStatus::Dispatching, now + 8)
        .and_then(|_| {
            deliveries.transition(&third.key(), LspPaymentDeliveryStatus::InFlight, now + 9)
        })
        .and_then(|_| {
            deliveries.transition(
                &third.key(),
                LspPaymentDeliveryStatus::SettlingUpstream {
                    payment_status: PaymentStatus::Success,
                    failure: None,
                },
                now + 10,
            )
        })
        .and_then(|_| {
            deliveries.transition(&third.key(), LspPaymentDeliveryStatus::Succeeded, now + 11)
        })
        .unwrap();
    let mut replay_after_success = second_request;
    replay_after_success.previous_tlc.prev_tlc_id += 1;
    assert!(deliveries
        .accept(&registration, &tenant, replay_after_success, now + 12,)
        .unwrap_err()
        .contains("already delivered successfully"));

    let reopened = LspPaymentDeliveryManager::new(store);
    let indexed = reopened.list_by_payment_hash(&payment_hash).unwrap();
    let indexed_keys = indexed
        .iter()
        .map(|delivery| delivery.key())
        .collect::<Vec<_>>();
    assert_eq!(indexed.len(), 3);
    assert!(indexed_keys.contains(&first.key()));
    assert!(indexed_keys.contains(&second.key()));
    assert!(indexed_keys.contains(&third.key()));
    assert_eq!(
        reopened.get(&first.key()).unwrap().unwrap().payment_hash,
        payment_hash
    );
    assert_eq!(
        reopened.get(&second.key()).unwrap().unwrap().payment_hash,
        payment_hash
    );
    assert_eq!(
        reopened.get(&third.key()).unwrap().unwrap().payment_hash,
        payment_hash
    );
    assert_eq!(
        reopened
            .get_by_payment_hash(&payment_hash)
            .unwrap()
            .unwrap()
            .key(),
        third.key()
    );
}

#[test]
fn in_flight_delivery_is_not_reverted_by_buffer_deadline() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let delivery = deliveries
        .accept(
            &registration,
            &tenant,
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    let key = delivery.key();
    let in_flight = deliveries
        .transition(&key, LspPaymentDeliveryStatus::Dispatching, now + 1)
        .and_then(|_| deliveries.transition(&key, LspPaymentDeliveryStatus::InFlight, now + 2))
        .unwrap();

    assert_eq!(in_flight.status, LspPaymentDeliveryStatus::InFlight);
    assert_eq!(deliveries.list_pending().unwrap(), vec![in_flight]);
}

#[test]
fn payment_delivery_status_accepts_only_declared_transitions() {
    let statuses = [
        ("Deferred", LspPaymentDeliveryStatus::Deferred),
        ("Dispatching", LspPaymentDeliveryStatus::Dispatching),
        ("InFlight", LspPaymentDeliveryStatus::InFlight),
        ("Succeeded", LspPaymentDeliveryStatus::Succeeded),
        (
            "Failed",
            LspPaymentDeliveryStatus::Failed {
                reason: "failed".to_string(),
            },
        ),
        (
            "SettlingUpstream",
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Success,
                failure: None,
            },
        ),
    ];
    let valid_transitions = [
        ("Deferred", "Dispatching"),
        ("Deferred", "Failed"),
        ("Deferred", "SettlingUpstream"),
        ("Dispatching", "Deferred"),
        ("Dispatching", "InFlight"),
        ("Dispatching", "Failed"),
        ("Dispatching", "SettlingUpstream"),
        ("InFlight", "Deferred"),
        ("InFlight", "SettlingUpstream"),
        ("SettlingUpstream", "InFlight"),
        ("SettlingUpstream", "Succeeded"),
        ("SettlingUpstream", "Failed"),
    ];

    for (current_name, current) in &statuses {
        for (next_name, next) in &statuses {
            assert_eq!(
                current.check_next_valid(next),
                valid_transitions.contains(&(*current_name, *next_name)),
                "unexpected transition decision from {current_name} to {next_name}"
            );
        }
    }
}

#[test]
fn payment_delivery_accepts_downstream_mpp_and_rejects_invalid_state_transition() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let invoices = LspInvoiceRegistry::new(store.clone());
    let deliveries = LspPaymentDeliveryManager::new(store);
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let delivery = deliveries
        .accept(&registration, &tenant, request.clone(), now)
        .unwrap();
    assert_eq!(delivery.request.max_parts, Some(2));
    assert!(request.into_send_payment_data().unwrap().allow_mpp);
    assert!(deliveries
        .transition(
            &delivery.key(),
            LspPaymentDeliveryStatus::Succeeded,
            now + 1,
        )
        .unwrap_err()
        .contains("invalid hosted payment delivery transition"));
}

#[test]
fn hosted_payment_failure_classification_matches_retry_policy() {
    for code in [
        TlcErrorCode::IncorrectOrUnknownPaymentDetails,
        TlcErrorCode::InvoiceExpired,
        TlcErrorCode::InvoiceCancelled,
        TlcErrorCode::FinalIncorrectExpiryDelta,
        TlcErrorCode::FinalIncorrectTlcAmount,
        TlcErrorCode::HoldTlcTimeout,
    ] {
        assert!(is_permanent_hosted_payment_failure(code), "{code:?}");
    }
    for code in [
        TlcErrorCode::TemporaryNodeFailure,
        TlcErrorCode::TemporaryChannelFailure,
        TlcErrorCode::UnknownNextPeer,
        TlcErrorCode::ChannelDisabled,
    ] {
        assert!(!is_permanent_hosted_payment_failure(code), "{code:?}");
    }
}

#[test]
fn payment_delivery_enforces_per_tenant_pending_limit() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
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
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    permanent_dispatch_failures_remaining: Arc<AtomicUsize>,
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
        let NetworkActorMessage::Fiber(FiberActorMessage::Command(command)) = message else {
            return Ok(());
        };
        match command {
            FiberActorCommand::GetPayment(_, reply) => {
                let _ = reply.send(Err("payment not started".to_string()));
            }
            FiberActorCommand::InspectBufferedTrampolineUpstream { reply, .. } => {
                let status = if state.upstream_pending.load(Ordering::Relaxed) {
                    crate::fiber::network::BufferedTrampolineUpstreamStatus::Pending
                } else {
                    crate::fiber::network::BufferedTrampolineUpstreamStatus::Removed
                };
                let _ = reply.send(status);
            }
            FiberActorCommand::DispatchBufferedTrampoline { reply, .. } => {
                state.dispatches.fetch_add(1, Ordering::Relaxed);
                let should_fail_permanently = state
                    .permanent_dispatch_failures_remaining
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok();
                let should_fail = state
                    .dispatch_failures_remaining
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok();
                let _ = if should_fail_permanently {
                    reply.send(Err(LspPaymentDispatchError::Permanent {
                        reason: "invoice is cancelled".to_string(),
                        error_code: TlcErrorCode::InvoiceCancelled,
                    }))
                } else if should_fail {
                    reply.send(Err(LspPaymentDispatchError::Temporary {
                        reason: "temporary dispatch failure".to_string(),
                    }))
                } else {
                    reply.send(Ok(()))
                };
            }
            FiberActorCommand::ReconcileBufferedTrampolineSettlement { reply, .. } => {
                let _ = reply.send(Ok(()));
            }
            FiberActorCommand::FailBufferedTrampoline { reply, .. } => {
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
    start_test_lsp_service_with_all_dispatch_failures(
        store,
        config,
        factory,
        lsp_key,
        dispatches,
        dispatch_failures_remaining,
        Arc::new(AtomicUsize::new(0)),
        failures,
        upstream_pending,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_test_lsp_service_with_all_dispatch_failures(
    store: crate::store::Store,
    config: LspConfig,
    factory: Arc<dyn TenantRuntimeFactory>,
    lsp_key: Privkey,
    dispatches: Arc<AtomicUsize>,
    dispatch_failures_remaining: Arc<AtomicUsize>,
    permanent_dispatch_failures_remaining: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
    upstream_pending: Arc<AtomicBool>,
) -> ActorRef<LspServiceMessage> {
    let public_network_actor = Actor::spawn(
        None,
        MockPublicNetworkActor,
        MockPublicNetworkState {
            dispatches,
            dispatch_failures_remaining,
            permanent_dispatch_failures_remaining,
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
            watchtower_store: store.clone(),
            store,
            runtime_factory: factory,
            signing_key: lsp_key,
            token_issuer: test_token_issuer(),
        },
    )
    .await
    .unwrap()
    .0
}

fn test_root_signer() -> Privkey {
    Privkey::from(&[7; 32])
}

fn test_root_signer_n(n: u8) -> Privkey {
    Privkey::from(&[20 + n; 32])
}

fn test_protocol_key(secret: u8) -> Privkey {
    Privkey::from(&[secret; 32])
}

async fn register_authenticated_test_tenant(
    service: &ActorRef<LspServiceMessage>,
    lsp_key: &Privkey,
    root_signer_key: &Privkey,
) -> TenantId {
    let root_signer_pubkey = root_signer_key.pubkey();
    let nonce = ractor::call!(
        service,
        LspServiceMessage::IssueTenantRegistryNonce,
        root_signer_pubkey
    )
    .unwrap()
    .unwrap();
    let payload = TenantRegistryPayload::new(lsp_key.pubkey(), root_signer_pubkey, nonce);
    let registration = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(root_signer_key, &payload),
            payload,
            reply,
        }
    })
    .unwrap()
    .unwrap();
    registration.status.record.tenant_id
}

fn sign_tenant_registry_payload(
    root_signer_key: &Privkey,
    payload: &TenantRegistryPayload,
) -> TenantRegistrySignature {
    let signature = secp256k1::SECP256K1.sign_ecdsa(
        &secp256k1::Message::from_digest(payload.digest()),
        &root_signer_key.0,
    );
    TenantRegistrySignature(signature.serialize_compact())
}

#[tokio::test]
async fn service_registers_only_valid_root_signer_proofs_and_consumes_nonce() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let lsp_key = Privkey::from(&[9; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let root_signer_key = Privkey::from(&[7; 32]);
    let root_signer_pubkey = root_signer_key.pubkey();
    let replaced_nonce = ractor::call!(
        service,
        LspServiceMessage::IssueTenantRegistryNonce,
        root_signer_pubkey
    )
    .unwrap()
    .unwrap();
    let nonce = ractor::call!(
        service,
        LspServiceMessage::IssueTenantRegistryNonce,
        root_signer_pubkey
    )
    .unwrap()
    .unwrap();

    let old_payload =
        TenantRegistryPayload::new(lsp_key.pubkey(), root_signer_pubkey, replaced_nonce);
    let old_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &old_payload),
            payload: old_payload,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(old_result.contains("missing, replaced, or consumed"));

    let wrong_lsp_payload =
        TenantRegistryPayload::new(Privkey::from(&[6; 32]).pubkey(), root_signer_pubkey, nonce);
    let wrong_lsp_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &wrong_lsp_payload),
            payload: wrong_lsp_payload,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(wrong_lsp_result.contains("another LSP node"));

    let payload = TenantRegistryPayload::new(lsp_key.pubkey(), root_signer_pubkey, nonce);
    let wrong_signature = sign_tenant_registry_payload(&Privkey::from(&[5; 32]), &payload);
    let wrong_signature_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            payload: payload.clone(),
            signature: wrong_signature,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(wrong_signature_result.contains("invalid RootSigner registration proof"));

    let registration = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            payload: payload.clone(),
            reply,
        }
    })
    .unwrap()
    .unwrap();
    let expected_tenant_id = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
    assert_eq!(registration.status.record.tenant_id, expected_tenant_id);
    assert_eq!(
        registration.status.record.root_signer_pubkey,
        Some(root_signer_pubkey)
    );
    assert_eq!(
        registry.registration_nonce(&root_signer_pubkey).unwrap(),
        None
    );
    assert!(!registration.access_token.is_empty());

    let replay_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            payload,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(replay_result.contains("already registered"));
}

#[tokio::test]
async fn authenticated_registration_issues_tenant_token_through_issuer() {
    use crate::lsp::tenant_watchtower_node_id;
    use crate::rpc::biscuit::BiscuitAuth;
    use biscuit_auth::KeyPair;

    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let biscuit_root = KeyPair::new();
    let issuer = BiscuitTokenIssuer::from_private_key(
        &biscuit_root.private().to_prefixed_string(),
        &biscuit_root.public().to_string(),
    )
    .unwrap();
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let lsp_key = Privkey::from(&[9; 32]);
    let public_network_actor = Actor::spawn(None, NoopNetworkActor, ()).await.unwrap().0;
    let service = Actor::spawn(
        None,
        LspService,
        LspServiceArgs {
            config,
            public_node_id: lsp_key.pubkey(),
            public_network_actor,
            watchtower_store: store.clone(),
            store,
            runtime_factory: factory,
            signing_key: lsp_key.clone(),
            token_issuer: issuer,
        },
    )
    .await
    .unwrap()
    .0;

    let root_signer_key = test_root_signer();
    let nonce = ractor::call!(
        service,
        LspServiceMessage::IssueTenantRegistryNonce,
        root_signer_key.pubkey()
    )
    .unwrap()
    .unwrap();
    let payload = TenantRegistryPayload::new(lsp_key.pubkey(), root_signer_key.pubkey(), nonce);
    let issued = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            payload,
            reply,
        }
    })
    .unwrap()
    .unwrap();
    let token = issued.access_token;
    let auth = BiscuitAuth::from_pubkey(biscuit_root.public().to_string()).unwrap();
    auth.check_permission("get_channel_signing_status", &token)
        .unwrap();
    auth.check_permission("list_channels", &token).unwrap();
    assert!(auth
        .check_permission("lsp_register_tenant", &token)
        .is_err());
    assert!(auth.check_permission("new_invoice", &token).is_err());
    let expected_node = tenant_watchtower_node_id(&issued.status.record.tenant_pubkey);
    let (biscuit, _) = auth.check_permission("get_invoice", &token).unwrap();
    assert_eq!(
        crate::rpc::biscuit::extract_tenant_id(&biscuit).unwrap(),
        Some(issued.status.record.tenant_id)
    );
    assert_eq!(
        crate::rpc::biscuit::extract_node_id(&biscuit).unwrap(),
        expected_node
    );
    assert_eq!(
        crate::rpc::biscuit::scoped_rpc_node_id(&biscuit).unwrap(),
        expected_node
    );
    crate::rpc::tenant::enforce_tenant_method_allowlist("get_invoice", &biscuit).unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist("create_watch_channel", &biscuit).unwrap();
    assert!(crate::rpc::tenant::enforce_tenant_method_allowlist("open_channel", &biscuit).is_err());
    auth.check_permission("create_preimage", &token).unwrap();
}

async fn register_test_invoice(
    service: &ActorRef<LspServiceMessage>,
    tenant_id: TenantId,
    tenant_key: &Privkey,
    payment_hash: Hash256,
) {
    register_test_invoice_with_buffer(service, tenant_id, tenant_key, payment_hash, None).await;
}

async fn register_test_invoice_with_buffer(
    service: &ActorRef<LspServiceMessage>,
    tenant_id: TenantId,
    tenant_key: &Privkey,
    payment_hash: Hash256,
    buffer_duration_ms: Option<u64>,
) {
    register_test_invoice_for_tenant(
        service,
        tenant_id,
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
                .get_by_payment_hash(&payment_hash)
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
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory::new(starts));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([13; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
            tenant.tenant_pubkey,
            private_channel_id,
        ))
        .unwrap();
    wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::InFlight).await;
    assert_eq!(dispatches.load(Ordering::Relaxed), 1);

    let key = manager
        .get_by_payment_hash(&payment_hash)
        .unwrap()
        .unwrap()
        .key();
    service
        .send_message(LspServiceMessage::ExpireDelivery(key))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn offline_tenant_does_not_block_an_online_tenant() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let starts = Arc::new(AtomicUsize::new(0));
    let u1_root = test_root_signer_n(1);
    let u2_root = test_root_signer_n(2);
    let u1_id = TenantId::from_root_signer_pubkey(&u1_root.pubkey());
    let u2_id = TenantId::from_root_signer_pubkey(&u2_root.pubkey());
    let factory = Arc::new(
        FakeRuntimeFactory::new(starts.clone())
            .with_protocol_secret(u1_id.clone(), 1)
            .with_protocol_secret(u2_id.clone(), 2),
    );
    let dispatches = Arc::new(AtomicUsize::new(0));
    let lsp_key = Privkey::from(&[9; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        lsp_key.clone(),
        dispatches.clone(),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    assert_eq!(
        register_authenticated_test_tenant(&service, &lsp_key, &u1_root).await,
        u1_id
    );
    assert_eq!(
        register_authenticated_test_tenant(&service, &lsp_key, &u2_root).await,
        u2_id
    );
    let u1_key = test_protocol_key(1);
    let u2_key = test_protocol_key(2);
    let u1_payment_hash = Hash256::from([31; 32]);
    let u2_payment_hash = Hash256::from([32; 32]);
    register_test_invoice_for_tenant(&service, u1_id.clone(), &u1_key, u1_payment_hash, None).await;
    register_test_invoice_for_tenant(&service, u2_id.clone(), &u2_key, u2_payment_hash, None).await;
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
        tenant_id: u1_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: u1_key.pubkey(),
        private_channel_id: Some(u1_channel),
        created_at: 42,
    };
    let u2 = HostedTenantRecord {
        tenant_id: u2_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: u2_key.pubkey(),
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
        manager
            .get_by_payment_hash(&u1_payment_hash)
            .unwrap()
            .unwrap()
            .status,
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
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([14; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
async fn settling_delivery_resumes_upstream_failure_after_restart_marker() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([43; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([44; 32]);
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let key = manager
        .get_by_payment_hash(&payment_hash)
        .unwrap()
        .unwrap()
        .key();
    manager
        .transition_with_error(
            &key,
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Failed,
                failure: Some(
                    "hosted tenant was unavailable before the buffer deadline".to_string(),
                ),
            },
            Some((
                "hosted tenant was unavailable before the buffer deadline".to_string(),
                Some(TlcErrorCode::TemporaryNodeFailure),
            )),
            now + 1,
        )
        .unwrap();

    service
        .send_message(LspServiceMessage::ResumeDelivery(key))
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
async fn restart_fails_deferred_delivery_after_upstream_tlc_removal() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory: Arc<dyn TenantRuntimeFactory> =
        Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([41; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
        LspPaymentDeliveryStatus::Failed {
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
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeRuntimeFactory::new(starts.clone()));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([20; 32]);
    register_test_invoice_with_buffer(
        &service,
        tenant_id.clone(),
        &tenant_key,
        payment_hash,
        Some(0),
    )
    .await;
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    assert_eq!(manager.get_by_payment_hash(&payment_hash).unwrap(), None);
    assert_eq!(starts.load(Ordering::Relaxed), 0);
    assert_eq!(dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn tenant_with_pending_delivery_cannot_be_evicted() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([21; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
            tenant_id.clone(),
            reply,
        ))
        .unwrap()
        .unwrap_err(),
        format!("tenant {tenant_id} has unfinished hosted payment deliveries")
    );
}

#[tokio::test]
async fn transient_dispatch_failure_returns_to_deferred_and_retries() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([22; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([29; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let delivery = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(delivery.attempt_count, 2);
    assert_eq!(
        delivery.last_error.as_deref(),
        Some("temporary dispatch failure")
    );
    assert_eq!(
        delivery.last_error_code,
        Some(TlcErrorCode::TemporaryNodeFailure)
    );
}

#[tokio::test]
async fn permanent_dispatch_failure_fails_upstream_without_retrying() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let upstream_failures = Arc::new(AtomicUsize::new(0));
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service_with_all_dispatch_failures(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(1)),
        upstream_failures.clone(),
        Arc::new(AtomicBool::new(true)),
    )
    .await;
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([24; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([31; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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

    wait_for_delivery_status(
        &manager,
        payment_hash,
        LspPaymentDeliveryStatus::Failed {
            reason: "invoice is cancelled".to_string(),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let delivery = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(delivery.attempt_count, 1);
    assert_eq!(delivery.last_error.as_deref(), Some("invoice is cancelled"));
    assert_eq!(
        delivery.last_error_code,
        Some(TlcErrorCode::InvoiceCancelled)
    );
    assert_eq!(dispatches.load(Ordering::Relaxed), 1);
    assert_eq!(upstream_failures.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn transient_payment_outcome_retries_delivery_before_deadline() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let upstream_failures = Arc::new(AtomicUsize::new(0));
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        upstream_failures.clone(),
    )
    .await;
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([25; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([32; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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

    assert_eq!(
        ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady {
            payment_hash,
            payment_status: PaymentStatus::Failed,
            failure: Some("peer is offline".to_string()),
            failure_code: Some(TlcErrorCode::TemporaryNodeFailure),
            reply,
        })
        .unwrap()
        .unwrap(),
        LspPaymentOutcomeDecision::RetryDelivery
    );
    let deferred = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(deferred.status, LspPaymentDeliveryStatus::Deferred);
    assert_eq!(deferred.attempt_count, 1);
    assert_eq!(deferred.last_error.as_deref(), Some("peer is offline"));

    wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::InFlight).await;
    let retried = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(retried.attempt_count, 2);
    assert_eq!(dispatches.load(Ordering::Relaxed), 2);
    assert_eq!(upstream_failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn permanent_payment_outcomes_settle_upstream_without_redispatch() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let upstream_failures = Arc::new(AtomicUsize::new(0));
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        Privkey::from(&[9; 32]),
        dispatches.clone(),
        upstream_failures.clone(),
    )
    .await;
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let private_channel_id = Hash256::from([33; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        private_channel_id: Some(private_channel_id),
        created_at: 42,
    };
    let cases = [
        (
            TlcErrorCode::IncorrectOrUnknownPaymentDetails,
            "incorrect payment details",
        ),
        (TlcErrorCode::InvoiceExpired, "invoice expired"),
        (TlcErrorCode::InvoiceCancelled, "invoice cancelled"),
        (
            TlcErrorCode::FinalIncorrectExpiryDelta,
            "final expiry mismatch",
        ),
        (
            TlcErrorCode::FinalIncorrectTlcAmount,
            "final amount mismatch",
        ),
        (TlcErrorCode::HoldTlcTimeout, "hold TLC timed out"),
    ];

    for (index, (error_code, reason)) in cases.into_iter().enumerate() {
        let payment_hash = Hash256::from([40 + index as u8; 32]);
        register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
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
        .unwrap();
        wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::InFlight).await;

        assert_eq!(
            ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady {
                payment_hash,
                payment_status: PaymentStatus::Failed,
                failure: Some(reason.to_string()),
                failure_code: Some(error_code),
                reply,
            })
            .unwrap()
            .unwrap(),
            LspPaymentOutcomeDecision::SettleUpstream,
            "{error_code:?} must not be retried"
        );
        let settling = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
        assert_eq!(
            settling.status,
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Failed,
                failure: Some(reason.to_string()),
            }
        );
        assert_eq!(settling.attempt_count, 1);
        assert_eq!(settling.last_error.as_deref(), Some(reason));
        assert_eq!(settling.last_error_code, Some(error_code));

        service
            .send_message(LspServiceMessage::PaymentOutcomeSettled {
                payment_hash,
                payment_status: PaymentStatus::Failed,
                failure: Some(reason.to_string()),
            })
            .unwrap();
        wait_for_delivery_status(
            &manager,
            payment_hash,
            LspPaymentDeliveryStatus::Failed {
                reason: reason.to_string(),
            },
        )
        .await;
    }

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(dispatches.load(Ordering::Relaxed), cases.len());
    assert_eq!(upstream_failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn downstream_outcome_is_persisted_before_upstream_settlement() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
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
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([23; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
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
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
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
    let key = manager
        .get_by_payment_hash(&payment_hash)
        .unwrap()
        .unwrap()
        .key();
    manager
        .transition(
            &key,
            LspPaymentDeliveryStatus::Dispatching,
            crate::now_timestamp_as_millis_u64(),
        )
        .and_then(|_| {
            manager.transition(
                &key,
                LspPaymentDeliveryStatus::InFlight,
                crate::now_timestamp_as_millis_u64(),
            )
        })
        .unwrap();

    for _ in 0..2 {
        assert_eq!(
            ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady {
                payment_hash,
                payment_status: PaymentStatus::Success,
                failure: None,
                failure_code: None,
                reply,
            })
            .unwrap()
            .unwrap(),
            LspPaymentOutcomeDecision::SettleUpstream
        );
    }
    let settling = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(
        settling.status,
        LspPaymentDeliveryStatus::SettlingUpstream {
            payment_status: PaymentStatus::Success,
            failure: None,
        }
    );
    let reopened = LspPaymentDeliveryManager::new(store);
    assert_eq!(
        reopened.get_by_payment_hash(&payment_hash).unwrap(),
        Some(settling)
    );

    service
        .send_message(LspServiceMessage::PaymentOutcomeSettled {
            payment_hash,
            payment_status: PaymentStatus::Success,
            failure: None,
        })
        .unwrap();
    wait_for_delivery_status(&reopened, payment_hash, LspPaymentDeliveryStatus::Succeeded).await;
}
