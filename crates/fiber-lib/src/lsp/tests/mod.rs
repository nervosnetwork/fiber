use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use ckb_types::packed::Script;
use fiber_store::backend::StorageBackend;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use tempfile::tempdir;

use crate::fiber::network::{
    FiberActorCommand, FiberActorMessage, HostedTenantActivity, NetworkActorMessage,
};
use crate::fiber_types::{
    Hash256, HashAlgorithm, PaymentStatus, PrevTlcInfo, Privkey, SettlementKey,
    TenantRegistryPayload, TenantRegistrySignature, TlcErrorCode, WatchChannelKeys,
};
use crate::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};

use crate::fiber::delivery::{
    DeliveryDecision, DeliveryDispatchError, PaymentOutcomeDecision, PaymentOutcomeRequest,
    SettledPaymentOutcome, TrampolineForwardingRequest,
};
use crate::lsp::{
    is_permanent_hosted_payment_failure, select_tenant_channel, BiscuitTokenIssuer,
    HostedTenantRecord, HostedTenantRuntime, LspConfig, LspInvoiceRegistry,
    LspPaymentDeliveryLimits, LspPaymentDeliveryManager, LspPaymentDeliveryStatus, LspService,
    LspServiceArgs, LspServiceMessage, TenantChannelRecord, TenantId, TenantRegistry,
    TenantRuntimeFactory, TenantSupervisor, DEFAULT_LSP_BUFFER_DURATION_MS,
    LSP_DELIVERY_SAFETY_MARGIN_MS, MAX_LSP_BUFFER_DURATION_MS,
};
use crate::store::{open_store, NodeNamespace, Store};

#[cfg(not(target_arch = "wasm32"))]
mod integration;
#[cfg(not(target_arch = "wasm32"))]
mod query_rpc;
#[cfg(not(target_arch = "wasm32"))]
mod signer_isolation;
#[cfg(not(target_arch = "wasm32"))]
mod token;

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
        token_ttl_secs: crate::lsp_config::DEFAULT_LSP_TOKEN_TTL_SECS,
        max_buffer_duration_ms: MAX_LSP_BUFFER_DURATION_MS,
        max_pending_deliveries: 1_024,
        max_pending_deliveries_per_tenant: 64,
        max_watch_channels_per_tenant: crate::lsp_config::DEFAULT_MAX_WATCH_CHANNELS_PER_TENANT,
        max_watch_channels: crate::lsp_config::DEFAULT_MAX_WATCH_CHANNELS,
        max_watch_preimages_per_tenant: crate::lsp_config::DEFAULT_MAX_WATCH_PREIMAGES_PER_TENANT,
        max_watch_preimages: crate::lsp_config::DEFAULT_MAX_WATCH_PREIMAGES,
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
        created_at: 42,
        token_epoch: 0,
    };

    assert_eq!(registry.register(record.clone()).unwrap(), record);
    assert_eq!(registry.register(record.clone()).unwrap(), record);
    let duplicate_key = HostedTenantRecord {
        tenant_id: TenantId::new("u2").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: record.tenant_pubkey,
        created_at: 43,
        token_epoch: 0,
    };
    assert_eq!(
        registry.register(duplicate_key).unwrap_err(),
        "protocol key is already registered to tenant u1"
    );

    let reopened = TenantRegistry::new(store);
    assert_eq!(reopened.get(&record.tenant_id).unwrap(), Some(record));
    assert_eq!(reopened.list().unwrap().len(), 1);
}

#[test]
fn tenant_registry_finds_by_key_after_a_reopen() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let root_signer = Privkey::from(&[13; 32]).pubkey();
    let records: Vec<_> = (1u8..=3)
        .map(|i| HostedTenantRecord {
            tenant_id: TenantId::new(format!("u{i}")).unwrap(),
            root_signer_pubkey: (i == 3).then_some(root_signer),
            tenant_pubkey: Privkey::from(&[i; 32]).pubkey(),
            created_at: i as u64,
            token_epoch: 0,
        })
        .collect();
    for record in &records {
        registry.register(record.clone()).unwrap();
    }
    assert_eq!(registry.count().unwrap(), 3);
    for record in &records {
        assert_eq!(
            registry
                .find_by_tenant_pubkey(&record.tenant_pubkey)
                .unwrap(),
            Some(record.clone())
        );
    }
    assert_eq!(
        registry
            .find_by_tenant_pubkey(&Privkey::from(&[9; 32]).pubkey())
            .unwrap(),
        None
    );
    assert_eq!(
        registry.find_by_root_signer_pubkey(&root_signer).unwrap(),
        Some(records[2].clone())
    );
    assert_eq!(
        registry
            .find_by_root_signer_pubkey(&records[0].tenant_pubkey)
            .unwrap(),
        None
    );

    // A registry made on the same store, as `is_internal_channel` does for each channel, finds
    // them from the store alone.
    let reopened = TenantRegistry::new(store);
    assert_eq!(reopened.count().unwrap(), 3);
    assert_eq!(
        reopened
            .find_by_tenant_pubkey(&records[1].tenant_pubkey)
            .unwrap(),
        Some(records[1].clone())
    );
    assert_eq!(
        reopened.find_by_root_signer_pubkey(&root_signer).unwrap(),
        Some(records[2].clone())
    );
    let duplicate_key = HostedTenantRecord {
        tenant_id: TenantId::new("u4").unwrap(),
        tenant_pubkey: records[1].tenant_pubkey,
        ..records[0].clone()
    };
    assert_eq!(
        reopened.register(duplicate_key).unwrap_err(),
        "protocol key is already registered to tenant u2"
    );
}

#[test]
fn tenant_registry_binds_multiple_channels_with_assets() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let record = HostedTenantRecord {
        tenant_id: TenantId::new("u1").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[1; 32]).pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    registry.register(record.clone()).unwrap();

    let ckb_channel = Hash256::from([41; 32]);
    let udt_channel = Hash256::from([42; 32]);
    let udt_script = Script::default();
    registry
        .bind_channel(&record.tenant_id, ckb_channel, None)
        .unwrap();
    registry
        .bind_channel(&record.tenant_id, udt_channel, Some(udt_script.clone()))
        .unwrap();

    let channels = registry.list_channels(&record.tenant_id).unwrap();
    assert_eq!(channels.len(), 2);
    assert!(channels.iter().any(|channel| {
        channel.channel_id == ckb_channel && channel.funding_udt_type_script.is_none()
    }));
    assert!(channels.iter().any(|channel| {
        channel.channel_id == udt_channel
            && channel.funding_udt_type_script == Some(udt_script.clone())
    }));
    let reopened = TenantRegistry::new(store);
    assert_eq!(reopened.list_channels(&record.tenant_id).unwrap().len(), 2);
    reopened
        .bind_channel(&record.tenant_id, udt_channel, Some(udt_script.clone()))
        .unwrap();
    assert_eq!(reopened.list_channels(&record.tenant_id).unwrap().len(), 2);
    assert!(reopened
        .bind_channel(&record.tenant_id, udt_channel, None)
        .is_err());
}

#[test]
fn select_tenant_channel_matches_asset_and_prefers_online() {
    let ckb_channel = TenantChannelRecord {
        channel_id: Hash256::from([51; 32]),
        funding_udt_type_script: None,
        bound_at: 0,
    };
    let udt_channel = TenantChannelRecord {
        channel_id: Hash256::from([52; 32]),
        funding_udt_type_script: Some(Script::default()),
        bound_at: 0,
    };
    let channels = vec![ckb_channel.clone(), udt_channel.clone()];
    let mut ready = HashSet::new();
    ready.insert(udt_channel.channel_id);

    assert_eq!(
        select_tenant_channel(&channels, Some(&ready), Some(&Script::default()))
            .unwrap()
            .channel_id,
        udt_channel.channel_id
    );
    assert_eq!(
        select_tenant_channel(&channels, Some(&ready), None)
            .unwrap()
            .channel_id,
        ckb_channel.channel_id
    );
    assert_eq!(
        select_tenant_channel(&channels, Some(&HashSet::new()), Some(&Script::default()))
            .unwrap()
            .channel_id,
        udt_channel.channel_id
    );
    assert!(select_tenant_channel(
        &[ckb_channel],
        Some(&HashSet::new()),
        Some(&Script::default())
    )
    .is_err());
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
        created_at: 42,
        token_epoch: 0,
    };
    let channel_id = Hash256::from([9u8; 32]);
    let node_id = crate::lsp::tenant_watchtower_node_id(&record.tenant_pubkey);
    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::external(record.tenant_pubkey),
            remote_settlement: Privkey::from(&[2; 32]).pubkey(),
            local_funding: record.tenant_pubkey,
            remote_funding: Privkey::from(&[3; 32]).pubkey(),
        },
        SettlementData {
            local_amount: 11,
            remote_amount: 22,
            tlcs: Vec::new(),
        },
        fiber_types::ChannelFeatures::V2,
        None,
    );

    super::watch::ensure_hosted_watch_channel(
        &Default::default(),
        &store,
        &record,
        channel_id,
        &Default::default(),
    )
    .unwrap();

    let watched = store
        .get_watch_channel(&node_id, &channel_id)
        .expect("watch row remains");
    assert_eq!(watched.local_settlement_data.local_amount, 11);
    assert_eq!(watched.local_settlement_data.remote_amount, 22);
}

#[cfg(feature = "watchtower")]
#[test]
fn ensure_hosted_watch_channel_enforces_watchtower_quotas() {
    use crate::fiber::channel::{ChannelActorState, ChannelActorStateStore};
    use crate::lsp_config::WatchtowerQuotas;
    use crate::store::NodeNamespace;
    use crate::watchtower::WatchtowerStore;
    use fiber_types::sample::StoreSample;

    let root = tempdir().expect("temporary directory");
    std::fs::create_dir_all(root.path().join("fiber")).expect("create store dir");
    let store = open_store(root.path().join("fiber/store")).expect("open store");
    let record = HostedTenantRecord {
        tenant_id: TenantId::new("quota-tenant").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[11; 32]).pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let node_id = crate::lsp::tenant_watchtower_node_id(&record.tenant_pubkey);
    let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(record.tenant_id.as_str()));

    let make_channel = |seed: u64, id_byte: u8| -> (Hash256, ChannelActorState) {
        let mut data = fiber_types::ChannelActorData::samples(seed).remove(0);
        let remote_keys = fiber_types::ChannelActorData::samples(seed + 1000)
            .remove(0)
            .local_channel_public_keys;
        let id = Hash256::from([id_byte; 32]);
        data.id = id;
        data.remote_channel_public_keys = Some(remote_keys);
        data.tlc_state.offered_tlcs.tlcs.clear();
        data.tlc_state.received_tlcs.tlcs.clear();
        data.channel_features = fiber_types::ChannelFeatures::V2;
        (id, ChannelActorState::from(data))
    };

    let (ch1, state1) = make_channel(101, 0x31);
    let (ch2, state2) = make_channel(102, 0x32);
    let (ch3, state3) = make_channel(103, 0x33);
    tenant_store.insert_channel_actor_state(state1);
    tenant_store.insert_channel_actor_state(state2);
    // Put ch3 only in the host store to exercise the fallback inversion branch too.
    store.insert_channel_actor_state(state3);

    let quotas = WatchtowerQuotas {
        max_channels_per_node: 1,
        max_channels_total: 1,
        max_preimages_per_node: 4,
        max_preimages_total: 4,
    };
    let events = Default::default();

    // First channel is admitted within quota.
    super::watch::ensure_hosted_watch_channel(&events, &store, &record, ch1, &quotas)
        .expect("first channel within quota");
    assert!(store.get_watch_channel(&node_id, &ch1).is_some());

    // Re-ensuring an already-monitored channel remains idempotent even at quota.
    super::watch::ensure_hosted_watch_channel(&events, &store, &record, ch1, &quotas)
        .expect("existing channel remains idempotent at quota");

    // Second channel via tenant-store branch is rejected by per-node quota without persisting.
    let err = super::watch::ensure_hosted_watch_channel(&events, &store, &record, ch2, &quotas)
        .expect_err("second channel must be rejected by watchtower quota");
    assert!(err.contains("quota exceeded"), "unexpected error: {err}");
    assert!(store.get_watch_channel(&node_id, &ch2).is_none());

    // Third channel via host-store fallback branch is also rejected by quota without persisting.
    let err = super::watch::ensure_hosted_watch_channel(&events, &store, &record, ch3, &quotas)
        .expect_err("fallback host-store branch must also enforce watchtower quota");
    assert!(err.contains("quota exceeded"), "unexpected error: {err}");
    assert!(store.get_watch_channel(&node_id, &ch3).is_none());
}

#[test]
fn authenticated_tenant_registry_consumes_and_persists_request_ids() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let root_signer_pubkey = Privkey::from(&[7; 32]).pubkey();
    let tenant_id = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
    let record = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: Some(root_signer_pubkey),
        tenant_pubkey: Privkey::from(&[8; 32]).pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let first = [1; 32];
    let second = [2; 32];
    assert_eq!(
        registry
            .register_authenticated(record.clone(), first)
            .unwrap(),
        record
    );
    assert!(registry
        .register_authenticated(record.clone(), first)
        .unwrap_err()
        .contains("already consumed"));
    assert_eq!(
        registry
            .refresh_authenticated(root_signer_pubkey, second)
            .unwrap(),
        record
    );
    assert!(registry
        .refresh_authenticated(root_signer_pubkey, second)
        .unwrap_err()
        .contains("already consumed"));

    let reopened = TenantRegistry::new(store.clone());
    assert_eq!(reopened.get(&tenant_id).unwrap(), Some(record.clone()));
    assert!(reopened
        .refresh_authenticated(root_signer_pubkey, second)
        .unwrap_err()
        .contains("already consumed"));
    assert_eq!(
        reopened
            .refresh_authenticated(root_signer_pubkey, [3; 32])
            .unwrap(),
        record
    );
    assert!(reopened
        .register_authenticated(record.clone(), [4; 32])
        .unwrap_err()
        .contains("already registered"));

    // Request ids are consumed per RootSigner, so another identity may reuse one.
    let other_root = Privkey::from(&[9; 32]).pubkey();
    let other_record = HostedTenantRecord {
        tenant_id: TenantId::from_root_signer_pubkey(&other_root),
        root_signer_pubkey: Some(other_root),
        tenant_pubkey: Privkey::from(&[10; 32]).pubkey(),
        created_at: 43,
        token_epoch: 0,
    };
    assert_eq!(
        reopened
            .register_authenticated(other_record.clone(), first)
            .unwrap(),
        other_record
    );

    // Concurrent refreshes must mint credentials for this request at most once.
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let registry = reopened.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                registry.refresh_authenticated(root_signer_pubkey, [5; 32])
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    for error in outcomes.into_iter().filter_map(Result::err) {
        assert!(error.contains("already consumed"), "{error}");
    }

    // Close the physical database, not just the registry wrapper, before reopening.
    drop(reopened);
    drop(registry);
    drop(store);
    let reopened = TenantRegistry::new(open_lsp_store(&config));
    assert_eq!(reopened.get(&tenant_id).unwrap(), Some(record));
    assert!(reopened
        .refresh_authenticated(root_signer_pubkey, [5; 32])
        .unwrap_err()
        .contains("already consumed"));
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
            created_at: 42,
            token_epoch: 0,
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
            created_at: 42,
            token_epoch: 0,
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
            created_at: 42,
            token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
    assert_eq!(
        reopened.get(&tenant.tenant_pubkey, &payment_hash).unwrap(),
        Some(registration)
    );
}

#[test]
fn hosted_invoice_registration_retry_reuses_existing_for_matching_terms_and_rejects_mismatch() {
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
        created_at: 42,
        token_epoch: 0,
    };
    let payment_hash = Hash256::from([17; 32]);
    let first_invoice = signed_invoice(&tenant_key, payment_hash);
    let first_registration = invoices
        .register(&tenant, first_invoice, None, lsp_key.pubkey(), &lsp_key)
        .expect("register initial hosted invoice");

    std::thread::sleep(Duration::from_millis(2));
    let retried_invoice = signed_invoice(&tenant_key, payment_hash);
    assert_ne!(
        first_registration.invoice.data.timestamp,
        retried_invoice.data.timestamp
    );
    let retried_registration = invoices
        .register(&tenant, retried_invoice, None, lsp_key.pubkey(), &lsp_key)
        .expect("retry with matching hosted invoice terms should reuse existing registration");
    assert_eq!(retried_registration, first_registration);

    let conflicting_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(2_000))
        .payment_hash(payment_hash)
        .expiry_time(Duration::from_secs(60 * 60))
        .build_with_sign(|message| {
            secp256k1::SECP256K1.sign_ecdsa_recoverable(message, &tenant_key.0)
        })
        .expect("build conflicting hosted invoice");
    assert_eq!(
        invoices
            .register(
                &tenant,
                conflicting_invoice,
                None,
                lsp_key.pubkey(),
                &lsp_key,
            )
            .unwrap_err(),
        "payment hash is already registered to another hosted invoice"
    );

    let no_expiry_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1_000))
        .payment_hash(Hash256::from([18; 32]))
        .build_with_sign(|message| {
            secp256k1::SECP256K1.sign_ecdsa_recoverable(message, &tenant_key.0)
        })
        .expect("build invoice without expiry");
    assert_eq!(
        invoices
            .register(&tenant, no_expiry_invoice, None, lsp_key.pubkey(), &lsp_key)
            .unwrap_err(),
        "hosted invoice must have a finite expiry"
    );
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
            Hash256::from([21; 32]),
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    assert_eq!(delivery.buffer_deadline, now + 120_000);

    let reopened = LspPaymentDeliveryManager::new(store);
    assert_eq!(reopened.list_pending().unwrap(), vec![delivery]);
}

#[test]
fn payment_delivery_uses_selected_tenant_channel_and_rejects_asset_mismatch() {
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
        created_at: 42,
        token_epoch: 0,
    };
    let payment_hash = Hash256::from([31; 32]);
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
    let channel = TenantChannelRecord {
        channel_id: Hash256::from([61; 32]),
        funding_udt_type_script: None,
        bound_at: 0,
    };

    let delivery = deliveries
        .accept_on_channel(
            &registration,
            &tenant,
            &channel,
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    assert_eq!(delivery.private_channel_id, channel.channel_id);

    let mut udt_channel = channel;
    udt_channel.channel_id = Hash256::from([62; 32]);
    udt_channel.funding_udt_type_script = Some(Script::default());
    let error = deliveries
        .accept_on_channel(
            &registration,
            &tenant,
            &udt_channel,
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap_err();
    assert!(
        error.contains("asset does not match"),
        "unexpected error: {error}"
    );
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
        created_at: 42,
        token_epoch: 0,
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
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            first_request.clone(),
            now,
        )
        .unwrap();

    assert_eq!(
        deliveries
            .accept(
                &registration,
                &tenant,
                Hash256::from([21; 32]),
                first_request.clone(),
                now + 1
            )
            .unwrap(),
        first
    );
    let mut conflicting_request = first_request.clone();
    conflicting_request.remaining_trampoline_onion.push(4);
    assert_eq!(
        deliveries
            .accept(
                &registration,
                &tenant,
                Hash256::from([21; 32]),
                conflicting_request,
                now + 2
            )
            .unwrap_err(),
        "hosted payment execution already exists with different data"
    );
    let mut second_request = first_request.clone();
    second_request.previous_tlc.prev_tlc_id += 1;
    assert!(deliveries
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            second_request.clone(),
            now + 3,
        )
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
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            second_request.clone(),
            now + 5,
        )
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
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            third_request,
            now + 7,
        )
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
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            replay_after_success,
            now + 12,
        )
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
        created_at: 42,
        token_epoch: 0,
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
            Hash256::from([21; 32]),
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
        created_at: 42,
        token_epoch: 0,
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
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            request.clone(),
            now,
        )
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
        created_at: 42,
        token_epoch: 0,
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
            Hash256::from([21; 32]),
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
                    reply.send(Err(DeliveryDispatchError::Permanent {
                        reason: "invoice is cancelled".to_string(),
                        error_code: TlcErrorCode::InvoiceCancelled,
                    }))
                } else if should_fail {
                    reply.send(Err(DeliveryDispatchError::Temporary {
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
            signing_events: Default::default(),
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

fn fresh_registration_request_id() -> [u8; 32] {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed) as u64;
    let mut request_id = [0u8; 32];
    request_id[..8].copy_from_slice(&sequence.to_be_bytes());
    request_id
}

fn registration_payload(
    lsp_key: &Privkey,
    root_signer_key: &Privkey,
    request_id: [u8; 32],
    signed_at: u64,
) -> TenantRegistryPayload {
    TenantRegistryPayload::new(
        lsp_key.pubkey(),
        root_signer_key.pubkey(),
        request_id,
        signed_at,
    )
}

async fn register_authenticated_test_tenant(
    service: &ActorRef<LspServiceMessage>,
    lsp_key: &Privkey,
    root_signer_key: &Privkey,
) -> TenantId {
    let payload = registration_payload(
        lsp_key,
        root_signer_key,
        fresh_registration_request_id(),
        crate::now_timestamp_as_millis_u64(),
    );
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
async fn service_registers_only_valid_root_signer_proofs_and_consumes_requests() {
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
    let now = crate::now_timestamp_as_millis_u64();
    let request_id = [5; 32];

    let wrong_lsp_payload = TenantRegistryPayload::new(
        Privkey::from(&[6; 32]).pubkey(),
        root_signer_pubkey,
        request_id,
        now,
    );
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

    let payload = registration_payload(&lsp_key, &root_signer_key, request_id, now);
    let wrong_signature = sign_tenant_registry_payload(&Privkey::from(&[8; 32]), &payload);
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

    let mut tampered_timestamp = payload.clone();
    tampered_timestamp.signed_at = now.saturating_add(1);
    let tampered_timestamp_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            payload: tampered_timestamp,
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(tampered_timestamp_result.contains("invalid RootSigner registration proof"));

    let mut tampered_request_id = payload.clone();
    tampered_request_id.request_id = [6; 32];
    let tampered_request_id_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            payload: tampered_request_id,
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(tampered_request_id_result.contains("invalid RootSigner registration proof"));

    let expired_payload = registration_payload(
        &lsp_key,
        &root_signer_key,
        [7; 32],
        now.saturating_sub(crate::fiber_types::TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS + 1),
    );
    let expired_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &expired_payload),
            payload: expired_payload,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(expired_result.contains("too old"));

    let future_payload = registration_payload(
        &lsp_key,
        &root_signer_key,
        [8; 32],
        now.saturating_add(
            crate::fiber_types::TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS + 60_000,
        ),
    );
    let future_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &future_payload),
            payload: future_payload,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(future_result.contains("too far in the future"));
    assert!(registry.list().unwrap().is_empty());

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
    assert!(!registration.access_token.is_empty());
    assert_eq!(
        registry.list().unwrap(),
        vec![registration.status.record.clone()]
    );

    let replay_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            payload: payload.clone(),
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(replay_result.contains("already consumed"));

    let resigned_replay = registration_payload(
        &lsp_key,
        &root_signer_key,
        request_id,
        crate::now_timestamp_as_millis_u64(),
    );
    let resigned_replay_result = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &resigned_replay),
            payload: resigned_replay,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(resigned_replay_result.contains("already consumed"));
    assert_eq!(registry.list().unwrap(), vec![registration.status.record]);
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
            signing_events: Default::default(),
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
    let payload = registration_payload(
        &lsp_key,
        &root_signer_key,
        fresh_registration_request_id(),
        crate::now_timestamp_as_millis_u64(),
    );
    let before_issued_ms = crate::now_timestamp_as_millis_u64() / 1_000 * 1_000;
    let issued = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_signer_key, &payload),
            payload,
            reply,
        }
    })
    .unwrap()
    .unwrap();
    let ttl_ms = crate::lsp_config::DEFAULT_LSP_TOKEN_TTL_SECS * 1_000;
    assert!((before_issued_ms + ttl_ms
        ..=crate::now_timestamp_as_millis_u64() / 1_000 * 1_000 + ttl_ms)
        .contains(&issued.expires_at_ms));
    let token = issued.access_token;
    assert!(
        !token.contains('='),
        "issued tokens need no WS padding removal"
    );
    let auth = BiscuitAuth::from_pubkey(biscuit_root.public().to_string()).unwrap();
    auth.check_permission("get_channel_signing_status", &token)
        .unwrap();
    auth.check_permission("list_channels", &token).unwrap();
    assert!(auth
        .check_permission("lsp_register_tenant", &token)
        .is_err());
    auth.check_permission("new_invoice", &token).unwrap();
    auth.check_permission("send_payment", &token).unwrap();
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
        crate::rpc::tenant::scoped_rpc_node_id(&biscuit).unwrap(),
        expected_node
    );
    crate::rpc::tenant::enforce_tenant_method_allowlist("get_invoice", &biscuit).unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist("create_watch_channel", &biscuit).unwrap();
    assert!(crate::rpc::tenant::enforce_tenant_method_allowlist("open_channel", &biscuit).is_err());
    auth.check_permission("create_preimage", &token).unwrap();
}

#[tokio::test]
async fn authenticated_registration_refreshes_token_after_lost_reply_and_restart() {
    use crate::lsp::tenant_watchtower_node_id;
    use crate::rpc::biscuit::BiscuitAuth;
    use biscuit_auth::KeyPair;

    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let biscuit_root = KeyPair::new();
    let issuer = BiscuitTokenIssuer::from_private_key(
        &biscuit_root.private().to_prefixed_string(),
        &biscuit_root.public().to_string(),
    )
    .unwrap();
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let lsp_key = Privkey::from(&[9; 32]);
    let public_network_actor = Actor::spawn(None, NoopNetworkActor, ()).await.unwrap().0;
    let args = || LspServiceArgs {
        config: config.clone(),
        public_node_id: lsp_key.pubkey(),
        public_network_actor: public_network_actor.clone(),
        watchtower_store: store.clone(),
        signing_events: Default::default(),
        store: store.clone(),
        runtime_factory: factory.clone(),
        signing_key: lsp_key.clone(),
        token_issuer: issuer.clone(),
    };
    let (service, handle) = Actor::spawn(None, LspService, args()).await.unwrap();
    let root_key = test_root_signer();
    let tenant_id = TenantId::from_root_signer_pubkey(&root_key.pubkey());
    let first_payload = registration_payload(
        &lsp_key,
        &root_key,
        fresh_registration_request_id(),
        crate::now_timestamp_as_millis_u64(),
    );
    let (send, receive) = tokio::sync::oneshot::channel();
    drop(receive); // The registration response cannot reach the client.
    service
        .send_message(LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_key, &first_payload),
            payload: first_payload.clone(),
            reply: send.into(),
        })
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    let channel_id = Hash256::from([76; 32]);
    let original = registry.bind_channel(&tenant_id, channel_id, None).unwrap();
    registry
        .bind_channel(&tenant_id, Hash256::from([77; 32]), Some(Script::default()))
        .unwrap();
    let original_channels = registry.list_channels(&tenant_id).unwrap();
    service.stop(None);
    handle.await.unwrap();
    let (service, handle) = Actor::spawn(None, LspService, args()).await.unwrap();

    let replay_after_restart = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_key, &first_payload),
            payload: first_payload,
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(replay_after_restart.contains("already consumed"));
    assert_eq!(registry.get(&tenant_id).unwrap(), Some(original.clone()));

    let payload = registration_payload(
        &lsp_key,
        &root_key,
        fresh_registration_request_id(),
        crate::now_timestamp_as_millis_u64(),
    );
    let invalid = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&Privkey::from(&[5; 32]), &payload),
            payload: payload.clone(),
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(invalid.contains("invalid RootSigner registration proof"));
    assert_eq!(registry.get(&tenant_id).unwrap(), Some(original.clone()));

    let refreshed = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_key, &payload),
            payload: payload.clone(),
            reply,
        }
    })
    .unwrap()
    .expect("fresh RootSigner proof must refresh the existing tenant token");
    assert!(!refreshed.created);
    assert_eq!(refreshed.status.record, original);
    assert_eq!(
        refreshed
            .status
            .channels
            .iter()
            .map(|status| status.channel.clone())
            .collect::<Vec<_>>(),
        original_channels
    );
    assert_eq!(registry.list().unwrap(), vec![original.clone()]);
    let auth = BiscuitAuth::from_pubkey(biscuit_root.public().to_string()).unwrap();
    let (biscuit, _) = auth
        .check_permission("get_invoice", &refreshed.access_token)
        .unwrap();
    assert_eq!(
        crate::rpc::biscuit::extract_tenant_id(&biscuit).unwrap(),
        Some(tenant_id)
    );
    assert_eq!(
        crate::rpc::tenant::scoped_rpc_node_id(&biscuit).unwrap(),
        tenant_watchtower_node_id(&original.tenant_pubkey)
    );
    assert!(auth
        .check_permission("lsp_register_tenant", &refreshed.access_token)
        .is_err());
    let replay = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_key, &payload),
            payload: payload.clone(),
            reply,
        }
    })
    .unwrap()
    .unwrap_err();
    assert!(replay.contains("already consumed"));
    assert_eq!(registry.list().unwrap(), vec![original.clone()]);

    let fresh_payload = registration_payload(
        &lsp_key,
        &root_key,
        fresh_registration_request_id(),
        crate::now_timestamp_as_millis_u64(),
    );
    let refreshed_again = ractor::call!(service, |reply| {
        LspServiceMessage::RegisterAuthenticatedTenant {
            signature: sign_tenant_registry_payload(&root_key, &fresh_payload),
            payload: fresh_payload,
            reply,
        }
    })
    .unwrap()
    .unwrap();
    assert!(!refreshed_again.created);
    assert_eq!(refreshed_again.status.record, original);
    assert_eq!(
        refreshed_again
            .status
            .channels
            .into_iter()
            .map(|status| status.channel)
            .collect::<Vec<_>>(),
        original_channels
    );
    service.stop(None);
    handle.await.unwrap();
    public_network_actor.stop(None);
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

struct BufferedDeliveryTest {
    _root: tempfile::TempDir,
    config: LspConfig,
    store: Store,
    manager: LspPaymentDeliveryManager<Store>,
    service: ActorRef<LspServiceMessage>,
    tenant: HostedTenantRecord,
    request: TrampolineForwardingRequest,
    dispatches: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
}

impl BufferedDeliveryTest {
    fn channel_a() -> Hash256 {
        Hash256::from([23; 32])
    }

    fn channel_b() -> Hash256 {
        Hash256::from([24; 32])
    }

    async fn new() -> Self {
        let root = tempdir().expect("temporary directory");
        let config = lsp_config(root.path().join("lsp"));
        let store = open_lsp_store(&config);
        let manager = LspPaymentDeliveryManager::new(store.clone());
        let dispatches = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let lsp_key = Privkey::from(&[9; 32]);
        let tenant_key = Privkey::from(&[1; 32]);
        let service = start_test_lsp_service(
            store.clone(),
            config.clone(),
            Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
            lsp_key.clone(),
            dispatches.clone(),
            failures.clone(),
        )
        .await;
        let tenant_id =
            register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
        let payment_hash = Hash256::from([93; 32]);
        register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
        let tenant = HostedTenantRecord {
            tenant_id,
            root_signer_pubkey: None,
            tenant_pubkey: tenant_key.pubkey(),
            created_at: 42,
            token_epoch: 0,
        };
        let request =
            hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64());
        let test = Self {
            _root: root,
            config,
            store,
            manager,
            service,
            tenant,
            request,
            dispatches,
            failures,
        };
        test.bind_channel(Self::channel_a(), None);
        test
    }

    fn bind_channel(&self, channel_id: Hash256, asset: Option<Script>) {
        use crate::fiber::channel::{ChannelActorState, ChannelActorStateStore};
        use crate::store::sample::StoreSample;

        TenantRegistry::new(self.store.clone())
            .bind_channel(&self.tenant.tenant_id, channel_id, asset.clone())
            .unwrap();
        let mut channel = ChannelActorState::samples(42).remove(0);
        channel.id = channel_id;
        channel.remote_pubkey = self.tenant.tenant_pubkey;
        channel.remote_channel_public_keys = Some(channel.local_channel_public_keys.clone());
        channel.funding_udt_type_script = asset;
        self.store.insert_channel_actor_state(channel.clone());
        self.store
            .namespaced(NodeNamespace::hosted_tenant(self.tenant.tenant_id.as_str()))
            .insert_channel_actor_state(channel);
    }

    fn cancel_invoice(&self) {
        let tenant_store = self
            .store
            .namespaced(NodeNamespace::hosted_tenant(self.tenant.tenant_id.as_str()));
        let invoice = LspInvoiceRegistry::new(self.store.clone())
            .get(&self.tenant.tenant_pubkey, &self.request.payment_hash)
            .unwrap()
            .unwrap()
            .invoice;
        tenant_store.insert_invoice(invoice, None).unwrap();
        tenant_store
            .update_invoice_status(&self.request.payment_hash, CkbInvoiceStatus::Cancelled)
            .unwrap();
    }

    fn set_channel_state(&self, channel_id: Hash256, status: crate::fiber_types::ChannelState) {
        use crate::fiber::channel::ChannelActorStateStore;

        let mut channel = self.store.get_channel_actor_state(&channel_id).unwrap();
        channel.state = status;
        self.store.insert_channel_actor_state(channel);
    }

    async fn accept(&self) -> crate::lsp::LspPaymentDelivery {
        assert_eq!(
            ractor::call!(self.service, |reply| {
                LspServiceMessage::AcceptTrampolineDelivery(self.request.clone(), reply)
            })
            .unwrap()
            .unwrap(),
            DeliveryDecision::Buffered,
        );
        self.manager
            .get_by_payment_hash(&self.request.payment_hash)
            .unwrap()
            .unwrap()
    }

    async fn online(&self, channel_id: Hash256) {
        self.service
            .send_message(LspServiceMessage::TenantChannelOnline(
                self.tenant.tenant_pubkey,
                channel_id,
            ))
            .unwrap();
        ractor::call!(self.service, LspServiceMessage::GetStatus).unwrap();
    }

    async fn resume(&self, key: crate::lsp::LspPaymentDeliveryKey) {
        self.service
            .send_message(LspServiceMessage::ResumeDelivery(key))
            .unwrap();
        // Wait for the explicit retry without depending on a wall-clock timer.
        ractor::call!(self.service, LspServiceMessage::GetStatus).unwrap();
    }

    async fn restart(&mut self) {
        self.service.stop_and_wait(None, None).await.unwrap();
        self.service = start_test_lsp_service(
            self.store.clone(),
            self.config.clone(),
            Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
            Privkey::from(&[9; 32]),
            self.dispatches.clone(),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        ractor::call!(self.service, LspServiceMessage::GetStatus).unwrap();
    }
}

#[tokio::test]
async fn cancelled_hosted_invoice_fails_new_delivery_without_dispatch() {
    let test = BufferedDeliveryTest::new().await;
    test.cancel_invoice();
    let accepted = test.accept().await;

    wait_for_delivery_status(
        &test.manager,
        accepted.payment_hash,
        LspPaymentDeliveryStatus::Failed {
            reason: "hosted invoice was cancelled".to_string(),
        },
    )
    .await;
    let delivery = test.manager.get(&accepted.key()).unwrap().unwrap();
    assert_eq!(
        delivery.last_error_code,
        Some(TlcErrorCode::InvoiceCancelled)
    );
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(test.failures.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn cancelling_buffered_hosted_invoice_fails_upstream_without_waiting_for_deadline() {
    let test = BufferedDeliveryTest::new().await;
    let accepted = test.accept().await;
    assert_eq!(accepted.status, LspPaymentDeliveryStatus::Deferred);
    test.cancel_invoice();
    test.service
        .send_message(LspServiceMessage::InvoiceCancelled {
            tenant_id: test.tenant.tenant_id.clone(),
            payment_hash: accepted.payment_hash,
        })
        .unwrap();
    // The first barrier follows the cancellation notice; the second follows
    // the ResumeDelivery message it enqueues. No retry timer needs to fire.
    ractor::call!(test.service, LspServiceMessage::GetStatus).unwrap();
    ractor::call!(test.service, LspServiceMessage::GetStatus).unwrap();

    let delivery = test.manager.get(&accepted.key()).unwrap().unwrap();
    assert_eq!(
        delivery.status,
        LspPaymentDeliveryStatus::Failed {
            reason: "hosted invoice was cancelled".to_string(),
        }
    );
    assert_eq!(
        delivery.last_error_code,
        Some(TlcErrorCode::InvoiceCancelled)
    );
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(test.failures.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn cancellation_notice_cannot_wake_another_tenants_delivery() {
    let test = BufferedDeliveryTest::new().await;
    let accepted = test.accept().await;
    test.service
        .send_message(LspServiceMessage::InvoiceCancelled {
            tenant_id: TenantId::new("other-tenant").unwrap(),
            payment_hash: accepted.payment_hash,
        })
        .unwrap();
    ractor::call!(test.service, LspServiceMessage::GetStatus).unwrap();

    let delivery = test.manager.get(&accepted.key()).unwrap().unwrap();
    assert_eq!(delivery.status, LspPaymentDeliveryStatus::Deferred);
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(test.failures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn restart_fails_buffered_delivery_for_cancelled_hosted_invoice() {
    let mut test = BufferedDeliveryTest::new().await;
    let accepted = test.accept().await;
    test.service.stop_and_wait(None, None).await.unwrap();
    test.cancel_invoice();
    test.service = start_test_lsp_service(
        test.store.clone(),
        test.config.clone(),
        Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
        Privkey::from(&[9; 32]),
        test.dispatches.clone(),
        test.failures.clone(),
    )
    .await;

    wait_for_delivery_status(
        &test.manager,
        accepted.payment_hash,
        LspPaymentDeliveryStatus::Failed {
            reason: "hosted invoice was cancelled".to_string(),
        },
    )
    .await;
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 0);
    assert_eq!(test.failures.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn buffered_delivery_reselects_online_same_asset_channel() {
    let test = BufferedDeliveryTest::new().await;
    test.bind_channel(BufferedDeliveryTest::channel_b(), None);
    let original = test.accept().await;
    assert_eq!(
        original.private_channel_id,
        BufferedDeliveryTest::channel_a()
    );
    test.online(BufferedDeliveryTest::channel_b()).await;
    test.resume(original.key()).await;

    let delivery = test.manager.get(&original.key()).unwrap().unwrap();
    assert_eq!(
        delivery.private_channel_id,
        BufferedDeliveryTest::channel_b()
    );
    assert_eq!(delivery.status, LspPaymentDeliveryStatus::InFlight);
    assert_eq!(delivery.request, original.request);
    assert_eq!(delivery.buffer_deadline, original.buffer_deadline);
    assert_eq!(delivery.attempt_count, 1);
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn buffered_delivery_does_not_reselect_another_asset() {
    let test = BufferedDeliveryTest::new().await;
    test.bind_channel(BufferedDeliveryTest::channel_b(), Some(Script::default()));
    let original = test.accept().await;
    test.online(BufferedDeliveryTest::channel_b()).await;
    test.resume(original.key()).await;

    let delivery = test.manager.get(&original.key()).unwrap().unwrap();
    assert_eq!(
        delivery.private_channel_id,
        BufferedDeliveryTest::channel_a()
    );
    assert_eq!(delivery.status, LspPaymentDeliveryStatus::Deferred);
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn buffered_delivery_keeps_channel_after_dispatch_starts() {
    for status in [
        LspPaymentDeliveryStatus::Deferred,
        LspPaymentDeliveryStatus::Dispatching,
        LspPaymentDeliveryStatus::InFlight,
    ] {
        let test = BufferedDeliveryTest::new().await;
        test.bind_channel(BufferedDeliveryTest::channel_b(), None);
        let original = test.accept().await;
        test.manager
            .begin_dispatch(&original.key(), original.updated_at)
            .unwrap();
        test.manager
            .transition(&original.key(), status.clone(), original.updated_at)
            .unwrap();
        test.online(BufferedDeliveryTest::channel_b()).await;
        test.resume(original.key()).await;

        let delivery = test.manager.get(&original.key()).unwrap().unwrap();
        assert_eq!(
            delivery.private_channel_id,
            BufferedDeliveryTest::channel_a()
        );
        assert_eq!(delivery.status, status);
        assert_eq!(delivery.attempt_count, 1);
        assert_eq!(test.dispatches.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn buffered_delivery_reselection_survives_service_restart() {
    let mut test = BufferedDeliveryTest::new().await;
    test.bind_channel(BufferedDeliveryTest::channel_b(), None);
    let original = test.accept().await;
    test.online(BufferedDeliveryTest::channel_b()).await;
    test.resume(original.key()).await;
    let reselected = test.manager.get(&original.key()).unwrap().unwrap();
    assert_eq!(
        reselected.private_channel_id,
        BufferedDeliveryTest::channel_b()
    );

    test.restart().await;
    test.online(BufferedDeliveryTest::channel_b()).await;
    test.resume(original.key()).await;
    assert_eq!(
        test.manager.get(&original.key()).unwrap().unwrap(),
        reselected
    );
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn buffered_delivery_replay_preserves_existing_execution_without_eligible_channels() {
    use crate::fiber_types::{ChannelState, CloseFlags};

    let test = BufferedDeliveryTest::new().await;
    test.bind_channel(BufferedDeliveryTest::channel_b(), None);
    test.online(BufferedDeliveryTest::channel_b()).await;
    let original = test.accept().await;
    test.resume(original.key()).await;
    let dispatched = test.manager.get(&original.key()).unwrap().unwrap();
    assert_eq!(
        dispatched.private_channel_id,
        BufferedDeliveryTest::channel_b()
    );

    // A becomes the selector's preferred channel, but is not a new execution identity.
    test.online(BufferedDeliveryTest::channel_a()).await;
    assert_eq!(test.accept().await, dispatched);
    for channel_id in [
        BufferedDeliveryTest::channel_a(),
        BufferedDeliveryTest::channel_b(),
    ] {
        test.set_channel_state(channel_id, ChannelState::Closed(CloseFlags::empty()));
    }
    assert_eq!(test.accept().await, dispatched);
    assert_eq!(test.dispatches.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn buffered_delivery_skips_closed_and_shutting_down_channels() {
    use crate::fiber_types::{ChannelState, CloseFlags, ShuttingDownFlags};

    for status in [
        ChannelState::Closed(CloseFlags::empty()),
        ChannelState::ShuttingDown(ShuttingDownFlags::empty()),
    ] {
        let test = BufferedDeliveryTest::new().await;
        test.bind_channel(BufferedDeliveryTest::channel_b(), None);
        test.set_channel_state(BufferedDeliveryTest::channel_a(), status);
        test.online(BufferedDeliveryTest::channel_a()).await;
        test.online(BufferedDeliveryTest::channel_b()).await;
        let delivery = test.accept().await;
        assert_eq!(
            delivery.private_channel_id,
            BufferedDeliveryTest::channel_b()
        );
    }
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
        created_at: 42,
        token_epoch: 0,
    };
    let now = crate::now_timestamp_as_millis_u64();

    // A payment addressed to some other node must not be taken over just because
    // its payment hash matches a hosted invoice: the tenant cannot peel an onion
    // built for that node, so the payment would fail instead of reaching it.
    let mut foreign = hosted_forwarding_request(&tenant, payment_hash, now);
    foreign.next_node_id = Privkey::from(&[0x77; 32]).pubkey();
    assert_eq!(
        ractor::call!(service, |reply| {
            LspServiceMessage::AcceptTrampolineDelivery(foreign, reply)
        })
        .unwrap()
        .unwrap(),
        DeliveryDecision::NotHosted,
        "a trampoline hop aimed at another node is not a hosted delivery"
    );

    assert_eq!(
        ractor::call!(service, |reply| {
            LspServiceMessage::AcceptTrampolineDelivery(
                hosted_forwarding_request(&tenant, payment_hash, now),
                reply,
            )
        })
        .unwrap()
        .unwrap(),
        DeliveryDecision::Buffered
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
        created_at: 42,
        token_epoch: 0,
    };
    let u2 = HostedTenantRecord {
        tenant_id: u2_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: u2_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
        DeliveryDecision::NotHosted
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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
async fn temporary_dispatch_failure_and_payment_outcome_ready_coalesce_to_single_retry() {
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
        Arc::new(AtomicUsize::new(2)),
        upstream_failures.clone(),
    )
    .await;
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([26; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([36; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    let tenant = HostedTenantRecord {
        tenant_id,
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
    };

    tokio::time::pause();
    ractor::call!(service, |reply| {
        LspServiceMessage::AcceptTrampolineDelivery(
            hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64()),
            reply,
        )
    })
    .unwrap()
    .unwrap();

    // The status call follows the initial dispatch in the service mailbox.
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    let deferred = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(deferred.status, LspPaymentDeliveryStatus::Deferred);
    assert_eq!(deferred.attempt_count, 1);
    assert_eq!(dispatches.load(Ordering::Relaxed), 1);

    // Simulate the concurrent PaymentOutcomeReady notification for the same
    // temporary routing failure; it must coalesce onto the already-armed retry timer.
    assert_eq!(
        ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady(
            PaymentOutcomeRequest {
                session_key: deferred.request.hosted_session_key(),
                payment_status: PaymentStatus::Failed,
                failure: Some("temporary routing failure".to_string()),
                failure_code: Some(TlcErrorCode::TemporaryNodeFailure),
            },
            reply,
        ))
        .unwrap()
        .unwrap(),
        PaymentOutcomeDecision::RetryDelivery
    );

    // Advance just past the first retry; attempt #2 also fails temporarily.
    tokio::time::advance(Duration::from_millis(1_100)).await;
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert_eq!(dispatches.load(Ordering::Relaxed), 2);

    // If PaymentOutcomeReady had armed a second timer, it would fire right after
    // attempt #2 and immediately trigger attempt #3 well before the next 1s retry.
    tokio::time::advance(Duration::from_millis(100)).await;
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    let after_second = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(dispatches.load(Ordering::Relaxed), 2);
    assert_eq!(after_second.attempt_count, 2);
    assert_eq!(after_second.status, LspPaymentDeliveryStatus::Deferred);

    // The timer armed after attempt #2 still fires and succeeds on attempt #3.
    tokio::time::advance(Duration::from_millis(1_000)).await;
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    let settled_in_flight = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    assert_eq!(dispatches.load(Ordering::Relaxed), 3);
    assert_eq!(settled_in_flight.attempt_count, 3);
    assert_eq!(upstream_failures.load(Ordering::Relaxed), 0);
    service.stop(None);
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
        created_at: 42,
        token_epoch: 0,
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
        created_at: 42,
        token_epoch: 0,
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

    let before = manager.get_by_payment_hash(&payment_hash).unwrap().unwrap();
    let owner = before.request.hosted_session_key();
    let mut wrong_tlc = owner;
    wrong_tlc.upstream_tlc_key.as_mut().unwrap().incoming_tlc_id += 1;
    let mut wrong_channel = owner;
    wrong_channel
        .upstream_tlc_key
        .as_mut()
        .unwrap()
        .incoming_channel_id = crate::gen_rand_sha256_hash();
    let mut wrong_hash = owner;
    wrong_hash.payment_hash = crate::gen_rand_sha256_hash();
    // Neither half of the key alone identifies the delivery. Ordinary payments,
    // another channel/TLC, and a mismatched hash must leave it unchanged.
    for session_key in [payment_hash.into(), wrong_tlc, wrong_channel, wrong_hash] {
        assert_eq!(
            ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady(
                PaymentOutcomeRequest {
                    session_key,
                    payment_status: PaymentStatus::Failed,
                    failure: Some("unrelated same-hash execution failed".into()),
                    failure_code: Some(TlcErrorCode::InvoiceCancelled),
                },
                reply,
            ))
            .unwrap()
            .unwrap(),
            PaymentOutcomeDecision::SettleUpstream
        );
        service
            .send_message(LspServiceMessage::PaymentOutcomeSettled(
                SettledPaymentOutcome {
                    session_key,
                    payment_status: PaymentStatus::Failed,
                    failure: Some("unrelated same-hash execution failed".into()),
                },
            ))
            .unwrap();
        // The query is ordered after the notification in the service mailbox.
        let after = ractor::call!(service, |reply| LspServiceMessage::GetPaymentDelivery(
            payment_hash,
            reply
        ))
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(after, before);
    }

    assert_eq!(
        ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady(
            PaymentOutcomeRequest {
                session_key: manager
                    .get_by_payment_hash(&payment_hash)
                    .unwrap()
                    .unwrap()
                    .request
                    .hosted_session_key(),
                payment_status: PaymentStatus::Failed,
                failure: Some("peer is offline".to_string()),
                failure_code: Some(TlcErrorCode::TemporaryNodeFailure),
            },
            reply,
        ))
        .unwrap()
        .unwrap(),
        PaymentOutcomeDecision::RetryDelivery
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
        created_at: 42,
        token_epoch: 0,
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
            ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady(
                PaymentOutcomeRequest {
                    session_key: manager
                        .get_by_payment_hash(&payment_hash)
                        .unwrap()
                        .unwrap()
                        .request
                        .hosted_session_key(),
                    payment_status: PaymentStatus::Failed,
                    failure: Some(reason.to_string()),
                    failure_code: Some(error_code),
                },
                reply,
            ))
            .unwrap()
            .unwrap(),
            PaymentOutcomeDecision::SettleUpstream,
            "{error_code:?} must not be retried"
        );
        assert_eq!(
            ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady(
                PaymentOutcomeRequest {
                    session_key: manager
                        .get_by_payment_hash(&payment_hash)
                        .unwrap()
                        .unwrap()
                        .request
                        .hosted_session_key(),
                    payment_status: PaymentStatus::Failed,
                    failure: Some(format!("reworded: {reason}")),
                    failure_code: Some(TlcErrorCode::TemporaryNodeFailure),
                },
                reply,
            ))
            .unwrap()
            .unwrap(),
            PaymentOutcomeDecision::SettleUpstream,
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
            .send_message(LspServiceMessage::PaymentOutcomeSettled(
                SettledPaymentOutcome {
                    session_key: manager
                        .get_by_payment_hash(&payment_hash)
                        .unwrap()
                        .unwrap()
                        .request
                        .hosted_session_key(),
                    payment_status: PaymentStatus::Failed,
                    failure: Some(reason.to_string()),
                },
            ))
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
        created_at: 42,
        token_epoch: 0,
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
            ractor::call!(service, |reply| LspServiceMessage::PaymentOutcomeReady(
                PaymentOutcomeRequest {
                    session_key: manager
                        .get_by_payment_hash(&payment_hash)
                        .unwrap()
                        .unwrap()
                        .request
                        .hosted_session_key(),
                    payment_status: PaymentStatus::Success,
                    failure: None,
                    failure_code: None,
                },
                reply,
            ))
            .unwrap()
            .unwrap(),
            PaymentOutcomeDecision::SettleUpstream
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
        .send_message(LspServiceMessage::PaymentOutcomeSettled(
            SettledPaymentOutcome {
                session_key: manager
                    .get_by_payment_hash(&payment_hash)
                    .unwrap()
                    .unwrap()
                    .request
                    .hosted_session_key(),
                payment_status: PaymentStatus::Success,
                failure: None,
            },
        ))
        .unwrap();
    wait_for_delivery_status(&reopened, payment_hash, LspPaymentDeliveryStatus::Succeeded).await;
}

struct ReconciliationNetworkActor;

#[async_trait]
impl Actor for ReconciliationNetworkActor {
    type Msg = NetworkActorMessage;
    type State = (PaymentStatus, TlcErrorCode, Arc<AtomicUsize>);
    type Arguments = Self::State;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let NetworkActorMessage::Fiber(FiberActorMessage::Command(command)) = message else {
            return Ok(());
        };
        match command {
            FiberActorCommand::GetPayment(key, reply) => {
                let payment_hash = key.payment_hash;
                let _ = reply.send(Ok(crate::fiber::network::SendPaymentResponse {
                    payment_hash,
                    payment_preimage: None,
                    status: state.0,
                    created_at: 0,
                    last_updated_at: 0,
                    failed_error: Some("persisted session failure".to_string()),
                    failed_error_code: Some(state.1),
                    custom_records: None,
                    fee: 0,
                    routers: vec![],
                }));
            }
            FiberActorCommand::ReconcileBufferedTrampolineSettlement { reply, .. } => {
                state.2.fetch_add(1, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            FiberActorCommand::DispatchBufferedTrampoline { .. } => {
                panic!("settling delivery must not redispatch")
            }
            _ => {}
        }
        Ok(())
    }
}

async fn check_settling_delivery_reconciliation(code: TlcErrorCode, status: PaymentStatus) {
    let root = tempdir().unwrap();
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("reconciliation").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let payment_hash = Hash256::from([11; 32]);
    let registration = LspInvoiceRegistry::new(store.clone())
        .register(
            &tenant,
            signed_invoice(&tenant_key, payment_hash),
            None,
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap();
    let now = crate::now_timestamp_as_millis_u64();
    let delivery = manager
        .accept(
            &registration,
            &tenant,
            Hash256::from([21; 32]),
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    manager
        .transition_with_error(
            &delivery.key(),
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Failed,
                failure: Some("original settlement failure".to_string()),
            },
            Some(("original settlement failure".to_string(), Some(code))),
            now,
        )
        .unwrap();
    let settlements = Arc::new(AtomicUsize::new(0));
    let (network, _) = Actor::spawn(
        None,
        ReconciliationNetworkActor,
        (status, code, settlements.clone()),
    )
    .await
    .unwrap();
    let (service, _) = Actor::spawn(
        None,
        LspService,
        LspServiceArgs {
            config,
            public_node_id: lsp_key.pubkey(),
            public_network_actor: network.clone(),
            watchtower_store: store.clone(),
            signing_events: Default::default(),
            store,
            runtime_factory: Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
            signing_key: lsp_key,
            token_issuer: test_token_issuer(),
        },
    )
    .await
    .unwrap();
    // Startup queues ResumeDelivery before this barrier. An invalid transition kills the actor.
    ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000)
        .expect("service survives reconciliation");
    let recovered = manager.get(&delivery.key()).unwrap().unwrap();
    assert_eq!(recovered.attempt_count, 0);
    match status {
        PaymentStatus::Success => assert_eq!(recovered.status, LspPaymentDeliveryStatus::Succeeded),
        PaymentStatus::Failed => assert!(matches!(
            recovered.status,
            LspPaymentDeliveryStatus::Failed { .. }
        )),
        _ => assert_eq!(recovered.status, LspPaymentDeliveryStatus::InFlight),
    }
    assert_eq!(
        settlements.load(Ordering::Relaxed),
        usize::from(status.is_final())
    );
    service.stop(None);
    network.stop(None);
}

#[tokio::test]
async fn settling_delivery_does_not_retry_temporary_session_failure() {
    check_settling_delivery_reconciliation(
        TlcErrorCode::TemporaryNodeFailure,
        PaymentStatus::Failed,
    )
    .await;
}

#[tokio::test]
async fn settling_delivery_tolerates_changed_failure_description() {
    check_settling_delivery_reconciliation(TlcErrorCode::InvoiceCancelled, PaymentStatus::Failed)
        .await;
}

struct QueryingOutcomeActor;

#[async_trait]
impl Actor for QueryingOutcomeActor {
    type Msg = LspServiceMessage;
    type State = (
        ActorRef<NetworkActorMessage>,
        Option<tokio::sync::oneshot::Sender<bool>>,
    );
    type Arguments = Self::State;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LspServiceMessage::PaymentOutcomeReady(request, reply) => {
                let payment_hash = request.session_key.payment_hash;
                // Deterministically exercise the reverse call while NetworkActor awaits our decision.
                let result = ractor::call_t!(
                    state.0,
                    |reply| NetworkActorMessage::new_command(FiberActorCommand::GetPayment(
                        payment_hash.into(),
                        reply
                    )),
                    1_000
                );
                if !matches!(result, Ok(Ok(_))) {
                    let _ = state.1.take().unwrap().send(false);
                }
                let _ = reply.send(Ok(PaymentOutcomeDecision::SettleUpstream));
            }
            LspServiceMessage::PaymentOutcomeSettled(..) => {
                if let Some(done) = state.1.take() {
                    let _ = done.send(true);
                }
            }
            LspServiceMessage::ListTenants(reply) => {
                let _ = reply.send(Ok(vec![]));
            }
            _ => {}
        }
        Ok(())
    }
}

#[tokio::test]
async fn payment_outcome_notification_allows_network_queries_before_settlement() {
    use crate::fiber::graph::NetworkGraphStateStore;
    use crate::fiber::network::{FiberActorEvent, PublicNetworkCommand};
    use crate::fiber::payment::{PaymentSession, PaymentSessionExt};

    let node = crate::tests::NetworkNode::new().await;
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("outcome-query").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[3; 32]).pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let payment_hash = Hash256::from([91; 32]);
    let request =
        hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64())
            .into_send_payment_data()
            .unwrap();
    let mut session = PaymentSession::new_session(&node.store, request, 0);
    session.status = PaymentStatus::Failed;
    node.store.insert_payment_session(session);
    let (done, completion) = tokio::sync::oneshot::channel();
    let (service, _) = Actor::spawn(
        None,
        QueryingOutcomeActor,
        (node.network_actor.clone(), Some(done)),
    )
    .await
    .unwrap();
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            PublicNetworkCommand::SetDeliveryPort(crate::lsp::LspDeliveryPort::without_fast_path(
                service.clone(),
            )),
        ))
        .unwrap();
    node.network_actor
        .send_message(NetworkActorMessage::Fiber(FiberActorMessage::Event(
            FiberActorEvent::PaymentActorStopped(payment_hash.into(), service.get_id(), None),
        )))
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), completion)
            .await
            .unwrap()
            .unwrap(),
        "NetworkActor must serve queries while awaiting the LSP outcome decision"
    );
    service.stop(None);
}

#[tokio::test]
async fn settling_delivery_recovers_concurrent_success() {
    check_settling_delivery_reconciliation(
        TlcErrorCode::TemporaryNodeFailure,
        PaymentStatus::Success,
    )
    .await;
}

#[tokio::test]
async fn settling_delivery_recovers_running_payment_without_redispatch() {
    check_settling_delivery_reconciliation(
        TlcErrorCode::TemporaryNodeFailure,
        PaymentStatus::Inflight,
    )
    .await;
}

#[tokio::test]
async fn delayed_payment_outcome_does_not_settle_replacement_execution() {
    use crate::fiber::graph::NetworkGraphStateStore;
    use crate::fiber::network::PublicNetworkCommand;
    use crate::fiber::payment::{PaymentSession, PaymentSessionExt};

    let node = crate::tests::NetworkNode::new().await;
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("outcome-replacement").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[3; 32]).pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let payment_hash = Hash256::from([92; 32]);
    let request =
        hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64())
            .into_send_payment_data()
            .unwrap();
    let mut original = PaymentSession::new_session(&node.store, request, 0);
    original.status = PaymentStatus::Failed;
    let mut replacement = original.clone();
    // Even identical timestamps/status must not confuse two incoming TLC executions.
    replacement
        .request
        .trampoline_context
        .as_mut()
        .unwrap()
        .previous_tlcs[0]
        .prev_tlc_id += 1;
    node.store.insert_payment_session(replacement.clone());
    let (done, mut completion) = tokio::sync::oneshot::channel();
    let (service, _) = Actor::spawn(
        None,
        QueryingOutcomeActor,
        (node.network_actor.clone(), Some(done)),
    )
    .await
    .unwrap();
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            PublicNetworkCommand::SetDeliveryPort(crate::lsp::LspDeliveryPort::without_fast_path(
                service.clone(),
            )),
        ))
        .unwrap();
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            FiberActorCommand::SettleLspPaymentOutcome {
                session: Box::new(original),
                last_error_packet: None,
            },
        ))
        .unwrap();
    ractor::call_t!(
        node.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::GetPayment(
            payment_hash.into(),
            reply
        )),
        1_000
    )
    .unwrap()
    .unwrap();
    ractor::call_t!(service, LspServiceMessage::ListTenants, 1_000)
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            completion.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "stale outcome must not report the replacement execution as settled"
    );
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            FiberActorCommand::SettleLspPaymentOutcome {
                session: Box::new(replacement),
                last_error_packet: None,
            },
        ))
        .unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), completion)
        .await
        .unwrap()
        .unwrap());
    service.stop(None);
}

#[test]
fn settling_delivery_only_upgrades_failure_to_proven_success() {
    let failure = LspPaymentDeliveryStatus::SettlingUpstream {
        payment_status: PaymentStatus::Failed,
        failure: Some("buffer expired".to_string()),
    };
    let success = LspPaymentDeliveryStatus::SettlingUpstream {
        payment_status: PaymentStatus::Success,
        failure: None,
    };
    assert!(failure.check_next_valid(&success));
    assert!(!success.check_next_valid(&failure));
    assert!(!failure.check_next_valid(&LspPaymentDeliveryStatus::Deferred));
    assert!(!success.check_next_valid(&LspPaymentDeliveryStatus::Deferred));
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn over_quota_hosted_channel_online_is_not_marked_ready_and_cannot_receive_payments() {
    use crate::fiber::channel::{
        ChannelActorState, ChannelActorStateStore, ChannelOpenRecordStore, TenantChannelOpening,
    };
    use crate::fiber::network::OpenChannelWithExternalFundingCommand;
    use crate::store::sample::StoreSample;
    use crate::watchtower::WatchtowerStore;

    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.max_watch_channels_per_tenant = 1;
    config.max_watch_channels = 2;
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let dispatches = Arc::new(AtomicUsize::new(0));
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store.clone(),
        config.clone(),
        Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
        lsp_key.clone(),
        dispatches.clone(),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([0x99; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let tenant = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let node_id = crate::lsp::tenant_watchtower_node_id(&tenant.tenant_pubkey);
    let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));

    let ch_watched = Hash256::from([0x61; 32]);
    let ch_over_quota = Hash256::from([0x62; 32]);
    for (seed, channel_id) in [(111u64, ch_watched), (112u64, ch_over_quota)] {
        let mut channel = ChannelActorState::samples(seed).remove(0);
        channel.id = channel_id;
        channel.remote_pubkey = tenant.tenant_pubkey;
        channel.remote_channel_public_keys = Some(channel.local_channel_public_keys.clone());
        channel.funding_udt_type_script = None;
        store.insert_channel_actor_state(channel.clone());
        tenant_store.insert_channel_actor_state(channel);
    }

    // Admission quota passes before any watched channel or opening exists.
    ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_id.clone(), reply)
    })
    .unwrap()
    .expect("first channel fits within quota");

    // An in-flight unwatched TenantChannelOpening (with active open record) consumes a quota slot.
    let open_command = OpenChannelWithExternalFundingCommand {
        pubkey: lsp_key.pubkey(),
        funding_amount: 100_000_000_000,
        public: false,
        shutdown_script: Script::default(),
        funding_lock_script: Script::default(),
        funding_lock_script_cell_deps: Vec::new(),
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
    };
    tenant_store.insert_channel_open_record(fiber_types::ChannelOpenRecord::new(
        ch_watched,
        lsp_key.pubkey(),
        open_command.funding_amount,
    ));
    tenant_store.put_tenant_channel_opening(TenantChannelOpening {
        command: open_command.clone(),
        channel_id: ch_watched,
        result: None,
        terminated: false,
    });
    let pending_err = ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_id.clone(), reply)
    })
    .unwrap()
    .expect_err("in-flight unwatched opening must count toward tenant watchtower quota");
    assert!(pending_err.contains("quota exceeded"), "{pending_err}");
    tenant_store.delete_tenant_channel_opening(&ch_watched);
    tenant_store.delete_channel_open_record(&ch_watched);

    // Bring ch_watched online -> creates watch row #1, binds channel, marks ready.
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant.tenant_pubkey,
            ch_watched,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert!(store.get_watch_channel(&node_id, &ch_watched).is_some());

    // Admission quota now rejects opening a 2nd channel for this tenant.
    let quota_err = ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_id.clone(), reply)
    })
    .unwrap()
    .expect_err("second channel must be rejected by watchtower admission quota");
    assert!(quota_err.contains("quota exceeded"), "{quota_err}");

    // Take ch_watched offline and buffer a payment while no channel is online.
    service
        .send_message(LspServiceMessage::TenantChannelOffline(
            tenant.tenant_pubkey,
            ch_watched,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();

    let request =
        hosted_forwarding_request(&tenant, payment_hash, crate::now_timestamp_as_millis_u64());
    assert_eq!(
        ractor::call!(service, |reply| {
            LspServiceMessage::AcceptTrampolineDelivery(request.clone(), reply)
        })
        .unwrap()
        .unwrap(),
        DeliveryDecision::Buffered,
    );
    let delivery = manager
        .get_by_payment_hash(&payment_hash)
        .unwrap()
        .expect("buffered delivery");
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);

    // Now bring ch_over_quota online: watchtower registration fails quota, so it must NOT
    // be bound, must NOT be marked ready, and must NOT wake or receive the pending payment.
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant.tenant_pubkey,
            ch_over_quota,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert!(store.get_watch_channel(&node_id, &ch_over_quota).is_none());

    let tenants = ractor::call!(service, LspServiceMessage::ListTenants)
        .unwrap()
        .unwrap();
    let status = tenants
        .into_iter()
        .find(|t| t.record.tenant_id == tenant_id)
        .expect("tenant status");
    assert!(
        !status
            .channels
            .iter()
            .any(|ch| ch.channel.channel_id == ch_over_quota),
        "over-quota unwatched channel must not be bound for delivery"
    );

    // Even an explicit ResumeDelivery must NOT dispatch onto the unwatched channel.
    service
        .send_message(LspServiceMessage::ResumeDelivery(delivery.key()))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        0,
        "pending payment must never dispatch onto an over-quota unwatched channel"
    );
    let still_waiting = manager.get(&delivery.key()).unwrap().unwrap();
    assert_eq!(still_waiting.private_channel_id, ch_watched);
    assert_ne!(still_waiting.status, LspPaymentDeliveryStatus::InFlight);

    // Even if ch_over_quota was pre-bound in registry before quota tightened, bringing it
    // online still refuses to mark it ready or dispatch deliveries to it.
    TenantRegistry::new(store.clone())
        .bind_channel(&tenant_id, ch_over_quota, None)
        .unwrap();
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant.tenant_pubkey,
            ch_over_quota,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    service
        .send_message(LspServiceMessage::ResumeDelivery(delivery.key()))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        0,
        "pre-bound over-quota unwatched channel must still not become ready or receive payments"
    );
    let tenants = ractor::call!(service, LspServiceMessage::ListTenants)
        .unwrap()
        .unwrap();
    let status = tenants
        .into_iter()
        .find(|t| t.record.tenant_id == tenant_id)
        .expect("tenant status");
    assert!(
        !status
            .channels
            .iter()
            .any(|ch| ch.channel.channel_id == ch_over_quota && ch.online),
        "pre-bound over-quota channel must remain offline/not ready"
    );

    service.stop_and_wait(None, None).await.unwrap();
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn hosted_channel_admission_quota_releases_closed_channels_enforces_global_pending_and_blocks_rpc_bypasses(
) {
    use crate::fiber::channel::{
        ChannelActorState, ChannelActorStateStore, ChannelOpenRecordStore, TenantChannelOpening,
    };
    use crate::fiber::network::OpenChannelWithExternalFundingCommand;
    use crate::rpc::channel::{
        to_rpc_channel_open_signer_material, ChannelRpcServer, ChannelRpcServerImpl,
        OpenChannelParams, OpenChannelWithExternalFundingParams,
    };
    use crate::rpc::tenant::AuthenticatedTenant;
    use crate::store::sample::StoreSample;
    use crate::watchtower::WatchtowerStore;
    use fiber_json_types::ChannelOpenSignerMaterial as JsonChannelOpenSignerMaterial;
    use fiber_types::{
        ChannelBasePublicKeys, ChannelOpenRecord, ChannelOpenSignerMaterial, ChannelOpeningStatus,
        ChannelState, CloseFlags,
    };
    use jsonrpsee::Extensions;
    use musig2::SecNonce;

    struct QuotaTestTenantActor;

    #[async_trait]
    impl Actor for QuotaTestTenantActor {
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
            match message {
                NetworkActorMessage::Fiber(FiberActorMessage::Command(
                    FiberActorCommand::OpenChannel(_, _),
                )) => {
                    panic!("over-quota open_channel must be rejected before reaching tenant actor");
                }
                NetworkActorMessage::Fiber(FiberActorMessage::Command(
                    FiberActorCommand::OpenTenantChannel {
                        admission_error,
                        reply,
                        ..
                    },
                )) => {
                    let err = admission_error
                        .expect("open_channel_with_external_funding must forward admission_error");
                    let _ = reply.send(Err(err));
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct QuotaTestRuntimeFactory {
        store: Store,
        public_node_id: fiber_types::Pubkey,
        tenant_pubkey: fiber_types::Pubkey,
    }

    #[async_trait]
    impl TenantRuntimeFactory for QuotaTestRuntimeFactory {
        fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
            Ok(HostedTenantRecord {
                tenant_id: tenant_id.clone(),
                root_signer_pubkey: None,
                tenant_pubkey: self.tenant_pubkey,
                created_at: 42,
                token_epoch: 0,
            })
        }

        async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
            let actor = Actor::spawn(None, QuotaTestTenantActor, ())
                .await
                .map_err(|error| error.to_string())?
                .0;
            let tenant_store = self
                .store
                .namespaced(NodeNamespace::hosted_tenant(record.tenant_id.as_str()));
            let rpc_context = crate::rpc::tenant::HostedTenantRpcContext {
                tenant_id: record.tenant_id.clone(),
                public_node_id: self.public_node_id,
                fiber_actor: crate::fiber::FiberActorRef::from_network(&actor),
                store: tenant_store,
                config: crate::FiberConfig::default(),
            };
            Ok(
                HostedTenantRuntime::network_backed(record.tenant_pubkey, actor)
                    .with_rpc_context(rpc_context),
            )
        }
    }

    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.max_watch_channels_per_tenant = 1;
    config.max_watch_channels = 2;
    let store = open_lsp_store(&config);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store.clone(),
        config.clone(),
        Arc::new(QuotaTestRuntimeFactory {
            store: store.clone(),
            public_node_id: lsp_key.pubkey(),
            tenant_pubkey: tenant_key.pubkey(),
        }),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let node_id = crate::lsp::tenant_watchtower_node_id(&tenant_key.pubkey());
    let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));

    let make_signer_material =
        |seed: u8| -> (ChannelOpenSignerMaterial, JsonChannelOpenSignerMaterial) {
            let base_keys = ChannelBasePublicKeys {
                funding_pubkey: Privkey::from(&[seed; 32]).pubkey(),
                tlc_base_key: Privkey::from(&[seed.wrapping_add(1); 32]).pubkey(),
            };
            let nonce = |offset: u8| {
                SecNonce::build([seed.wrapping_add(offset); 32])
                    .build()
                    .public_nonce()
            };
            let internal = ChannelOpenSignerMaterial {
                base_public_keys: base_keys,
                v2_nonces: vec![nonce(2), nonce(3)],
                first_commitment_point: Privkey::from(&[seed.wrapping_add(4); 32]).pubkey(),
                second_commitment_point: Privkey::from(&[seed.wrapping_add(5); 32]).pubkey(),
                commitment_nonce: nonce(6),
                next_commitment_nonce: nonce(7),
                revocation_nonce: nonce(8),
                channel_announcement_nonce: None,
            };
            let json = to_rpc_channel_open_signer_material(&internal);
            (internal, json)
        };

    // 1. Open and watch ch1, then close ch1 and remove its watchtower row while
    //    TenantChannelOpening + ChannelOpenRecord(ChannelReady) remain persisted.
    //    Closing ch1 must release its quota slot (never leak pending_unwatched).
    let ch1 = Hash256::from([0x71; 32]);
    let mut state1 = ChannelActorState::samples(201).remove(0);
    state1.id = ch1;
    state1.remote_pubkey = tenant_key.pubkey();
    state1.remote_channel_public_keys = Some(state1.local_channel_public_keys.clone());
    state1.funding_udt_type_script = None;
    store.insert_channel_actor_state(state1.clone());
    tenant_store.insert_channel_actor_state(state1.clone());

    let (mat1, _) = make_signer_material(10);
    let mut ready_record = ChannelOpenRecord::new(ch1, lsp_key.pubkey(), 100_000_000_000);
    ready_record.update_status(ChannelOpeningStatus::ChannelReady);
    tenant_store.insert_channel_open_record(ready_record);
    tenant_store.put_tenant_channel_opening(TenantChannelOpening {
        command: OpenChannelWithExternalFundingCommand {
            pubkey: lsp_key.pubkey(),
            funding_amount: 100_000_000_000,
            public: false,
            shutdown_script: Script::default(),
            funding_lock_script: Script::default(),
            funding_lock_script_cell_deps: Vec::new(),
            funding_udt_type_script: None,
            commitment_fee_rate: None,
            commitment_delay_epoch: None,
            funding_fee_rate: None,
            tlc_expiry_delta: None,
            tlc_min_value: None,
            tlc_fee_proportional_millionths: None,
            max_tlc_value_in_flight: None,
            max_tlc_number_in_flight: None,
            external_channel_signer: Some(mat1),
        },
        channel_id: ch1,
        result: None,
        terminated: false,
    });
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            ch1,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert!(store.get_watch_channel(&node_id, &ch1).is_some());

    // Close ch1 and remove watchtower row.
    state1.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
    tenant_store.insert_channel_actor_state(state1);
    assert!(store.remove_watch_channel(node_id.clone(), ch1));
    ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_id.clone(), reply)
    })
    .unwrap()
    .expect("closing a watched channel must release its watchtower quota slot");

    // 1b. A cold/restarted tenant with an orphaned WaitingForPeer opening (no ChannelActorState,
    //     result: None) must NOT deadlock its own quota=1 admission check.
    let cold_orphan_id = Hash256::from([0x79; 32]);
    tenant_store.insert_channel_open_record(ChannelOpenRecord::new(
        cold_orphan_id,
        lsp_key.pubkey(),
        100_000_000_000,
    ));
    tenant_store.put_tenant_channel_opening(TenantChannelOpening {
        command: OpenChannelWithExternalFundingCommand {
            pubkey: lsp_key.pubkey(),
            funding_amount: 100_000_000_000,
            public: false,
            shutdown_script: Script::default(),
            funding_lock_script: Script::default(),
            funding_lock_script_cell_deps: Vec::new(),
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
        channel_id: cold_orphan_id,
        result: None,
        terminated: false,
    });
    ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_id.clone(), reply)
    })
    .unwrap()
    .expect("orphaned pre-state opening on cold tenant must not deadlock quota=1");
    tenant_store.delete_tenant_channel_opening(&cold_orphan_id);
    tenant_store.delete_channel_open_record(&cold_orphan_id);

    // 2. Fill the tenant's 1-channel quota with ch2 and exercise direct RPC admission checks.
    let ch2 = Hash256::from([0x72; 32]);
    let mut state2 = ChannelActorState::samples(202).remove(0);
    state2.id = ch2;
    state2.remote_pubkey = tenant_key.pubkey();
    state2.remote_channel_public_keys = Some(state2.local_channel_public_keys.clone());
    state2.funding_udt_type_script = None;
    store.insert_channel_actor_state(state2.clone());
    tenant_store.insert_channel_actor_state(state2);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            ch2,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();
    assert!(store.get_watch_channel(&node_id, &ch2).is_some());

    let (public_actor, _) = Actor::spawn(None, BusyNetworkActor, ()).await.unwrap();
    let rpc = ChannelRpcServerImpl::new(public_actor.clone(), store.clone())
        .with_lsp_actor(Some(service.clone()));
    let mut ext = Extensions::new();
    ext.insert(AuthenticatedTenant(tenant_id.clone()));

    // 2a. `open_channel` rejects when watchtower quota is full.
    let err = ChannelRpcServer::open_channel(
        &rpc,
        &ext,
        OpenChannelParams {
            pubkey: lsp_key.pubkey().into(),
            funding_amount: 100_000_000_000,
            public: Some(false),
            one_way: None,
            funding_udt_type_script: None,
            shutdown_script: None,
            commitment_delay_epoch: None,
            commitment_fee_rate: None,
            funding_fee_rate: None,
            tlc_expiry_delta: None,
            tlc_min_value: None,
            tlc_fee_proportional_millionths: None,
            max_tlc_value_in_flight: None,
            max_tlc_number_in_flight: None,
        },
    )
    .await
    .err()
    .expect("open_channel must enforce watchtower quota for hosted tenants");
    assert!(err.to_string().contains("quota exceeded"), "{err}");

    // 2b. `open_channel_with_external_funding` evaluates watchtower quota and forwards
    //     `admission_error` into `OpenTenantChannel` (which enforces it on new/non-retry opens).
    let (_, json_new) = make_signer_material(20);
    let err = ChannelRpcServer::open_channel_with_external_funding(
        &rpc,
        &ext,
        OpenChannelWithExternalFundingParams {
            pubkey: lsp_key.pubkey().into(),
            funding_amount: 100_000_000_000,
            public: Some(false),
            funding_udt_type_script: None,
            shutdown_script: Script::default().into(),
            funding_lock_script: Script::default().into(),
            funding_lock_script_cell_deps: Some(Vec::new()),
            commitment_delay_epoch: None,
            commitment_fee_rate: None,
            funding_fee_rate: None,
            tlc_expiry_delta: None,
            tlc_min_value: None,
            tlc_fee_proportional_millionths: None,
            max_tlc_value_in_flight: None,
            max_tlc_number_in_flight: None,
            external_channel_signer: Some(json_new),
        },
    )
    .await
    .expect_err("new external-funding open must reject when watchtower quota is full");
    assert!(err.to_string().contains("quota exceeded"), "{err}");

    public_actor.stop(None);
    service.stop_and_wait(None, None).await.unwrap();

    // 3. Global `max_channels_total` must include `pending_unwatched` across OTHER tenants:
    //    with `max_watch_channels_per_tenant = 2, max_watch_channels = 2`,
    //    Tenant A has 1 watched channel (and 0 pending openings of its own) and
    //    Tenant B has 1 pending unwatched opening
    //    => `channels_total (1) + pending_total (1) >= 2` rejects Tenant A's 2nd open.
    //    Checking Tenant A specifically guarantees dropping the cross-tenant global pending
    //    term causes this test to fail.
    let root2 = tempdir().expect("temporary directory");
    let mut config2 = lsp_config(root2.path().join("lsp"));
    config2.max_watch_channels_per_tenant = 2;
    config2.max_watch_channels = 2;
    let store2 = open_lsp_store(&config2);
    let tenant_b_root = Privkey::from(&[0x55; 32]);
    let expected_tenant_b_id = TenantId::from_root_signer_pubkey(&tenant_b_root.pubkey());
    let service2 = start_test_lsp_service(
        store2.clone(),
        config2.clone(),
        Arc::new(
            FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))
                .with_protocol_secret(expected_tenant_b_id.clone(), 2),
        ),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_a_id =
        register_authenticated_test_tenant(&service2, &lsp_key, &test_root_signer()).await;
    let tenant_b_id = register_authenticated_test_tenant(&service2, &lsp_key, &tenant_b_root).await;

    let ch_a = Hash256::from([0x81; 32]);
    let mut state_a = ChannelActorState::samples(210).remove(0);
    state_a.id = ch_a;
    state_a.remote_pubkey = tenant_key.pubkey();
    state_a.remote_channel_public_keys = Some(state_a.local_channel_public_keys.clone());
    store2.insert_channel_actor_state(state_a.clone());
    store2
        .namespaced(NodeNamespace::hosted_tenant(tenant_a_id.as_str()))
        .insert_channel_actor_state(state_a);
    service2
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            ch_a,
        ))
        .unwrap();
    ractor::call!(service2, LspServiceMessage::GetStatus).unwrap();

    // Put an orphaned pre-state opening for cold Tenant B first: it must not consume global quota.
    let ch_b_pending = Hash256::from([0x82; 32]);
    let tenant_b_store = store2.namespaced(NodeNamespace::hosted_tenant(tenant_b_id.as_str()));
    tenant_b_store.insert_channel_open_record(ChannelOpenRecord::new(
        ch_b_pending,
        lsp_key.pubkey(),
        100_000_000_000,
    ));
    tenant_b_store.put_tenant_channel_opening(TenantChannelOpening {
        command: OpenChannelWithExternalFundingCommand {
            pubkey: lsp_key.pubkey(),
            funding_amount: 100_000_000_000,
            public: false,
            shutdown_script: Script::default(),
            funding_lock_script: Script::default(),
            funding_lock_script_cell_deps: Vec::new(),
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
        channel_id: ch_b_pending,
        result: None,
        terminated: false,
    });
    service2
        .send_message(LspServiceMessage::RefreshTenantWatchtowerChannelQuota(
            tenant_b_id.clone(),
        ))
        .unwrap();
    ractor::call!(service2, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id.clone(), reply)
    })
    .unwrap()
    .expect("orphaned pre-state opening on cold tenant must not consume global quota for Tenant A");

    // Once Tenant B has a persisted ChannelActorState for the unwatched opening, global quota
    // must reject Tenant A (`channels_total (1) + pending_total (1) >= 2`) even though Tenant A
    // has 0 pending openings of its own and `channels_per_node(A) == 1 < 2`.
    let mut state_b_pending = ChannelActorState::samples(211).remove(0);
    state_b_pending.id = ch_b_pending;
    state_b_pending.remote_pubkey = lsp_key.pubkey();
    tenant_b_store.insert_channel_actor_state(state_b_pending);
    service2
        .send_message(LspServiceMessage::RefreshTenantWatchtowerChannelQuota(
            tenant_b_id.clone(),
        ))
        .unwrap();

    let global_err = ractor::call!(service2, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id.clone(), reply)
    })
    .unwrap()
    .expect_err("global quota must account for OTHER tenants' pending unwatched openings");
    assert!(
        global_err.contains("global watchtower channel quota exceeded"),
        "{global_err}"
    );
    service2.stop_and_wait(None, None).await.unwrap();

    // Restarting LspService must initialize the cached cross-tenant pending counter in pre_start
    // so Tenant A is still rejected immediately after restart without scanning on every check.
    let service3 = start_test_lsp_service(
        store2.clone(),
        config2,
        Arc::new(
            FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))
                .with_protocol_secret(expected_tenant_b_id, 2),
        ),
        lsp_key,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let restart_global_err = ractor::call!(service3, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id, reply)
    })
    .unwrap()
    .expect_err("pre_start must seed cross-tenant pending unwatched count");
    assert!(
        restart_global_err.contains("global watchtower channel quota exceeded"),
        "{restart_global_err}"
    );
    service3.stop_and_wait(None, None).await.unwrap();
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn hosted_channel_admission_quota_serializes_concurrent_opens_and_releases_on_error_or_drop()
{
    use crate::fiber::channel::{
        derive_temp_channel_id_from_tlc_key, ChannelOpenRecordStore, TenantChannelOpening,
    };
    use crate::fiber::network::OpenChannelResponse;
    use crate::rpc::channel::{
        to_rpc_channel_open_signer_material, ChannelRpcServer, ChannelRpcServerImpl,
        OpenChannelParams, OpenChannelWithExternalFundingParams,
    };
    use crate::rpc::tenant::AuthenticatedTenant;
    use fiber_json_types::ChannelOpenSignerMaterial as JsonChannelOpenSignerMaterial;
    use fiber_types::{ChannelBasePublicKeys, ChannelOpenRecord, ChannelOpenSignerMaterial};
    use jsonrpsee::Extensions;
    use musig2::SecNonce;
    use tokio::sync::{watch, Mutex as TokioMutex};

    let make_signer_material =
        |seed: u8| -> (ChannelOpenSignerMaterial, JsonChannelOpenSignerMaterial) {
            let base_keys = ChannelBasePublicKeys {
                funding_pubkey: Privkey::from(&[seed; 32]).pubkey(),
                tlc_base_key: Privkey::from(&[seed.wrapping_add(1); 32]).pubkey(),
            };
            let nonce = |offset: u8| {
                SecNonce::build([seed.wrapping_add(offset); 32])
                    .build()
                    .public_nonce()
            };
            let internal = ChannelOpenSignerMaterial {
                base_public_keys: base_keys,
                v2_nonces: vec![nonce(2), nonce(3)],
                first_commitment_point: Privkey::from(&[seed.wrapping_add(4); 32]).pubkey(),
                second_commitment_point: Privkey::from(&[seed.wrapping_add(5); 32]).pubkey(),
                commitment_nonce: nonce(6),
                next_commitment_nonce: nonce(7),
                revocation_nonce: nonce(8),
                channel_announcement_nonce: None,
            };
            let json = to_rpc_channel_open_signer_material(&internal);
            (internal, json)
        };

    let make_external_params =
        |lsp_pubkey: fiber_types::Pubkey, signer: JsonChannelOpenSignerMaterial| {
            OpenChannelWithExternalFundingParams {
                pubkey: lsp_pubkey.into(),
                funding_amount: 100_000_000_000,
                public: Some(false),
                funding_udt_type_script: None,
                shutdown_script: Script::default().into(),
                funding_lock_script: Script::default().into(),
                funding_lock_script_cell_deps: Some(Vec::new()),
                commitment_delay_epoch: None,
                commitment_fee_rate: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
                external_channel_signer: Some(signer),
            }
        };

    #[derive(Clone)]
    enum ActorBehavior {
        /// Hold admitted openings until `release` becomes `true`; optionally persist
        /// `TenantChannelOpening` immediately upon arrival (like real `FiberActor`).
        HoldUntilRelease {
            entered: watch::Sender<usize>,
            release: watch::Receiver<bool>,
            persist_before_wait: bool,
            persist_on_complete: bool,
        },
        /// Return an error without persisting anything (tests RAII release on error).
        Fail(String),
    }

    struct ConcurrentOpenControl {
        behavior: TokioMutex<ActorBehavior>,
        admitted_external_opens: AtomicUsize,
        admitted_open_channels: AtomicUsize,
    }

    struct ConcurrentOpenTenantActor {
        tenant_store: Store,
        control: Arc<ConcurrentOpenControl>,
    }

    #[async_trait]
    impl Actor for ConcurrentOpenTenantActor {
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
            match message {
                NetworkActorMessage::Fiber(FiberActorMessage::Command(
                    FiberActorCommand::OpenTenantChannel {
                        command,
                        admission_error,
                        reply,
                    },
                )) => {
                    if let Some(err) = admission_error {
                        let _ = reply.send(Err(err));
                        return Ok(());
                    }
                    self.control
                        .admitted_external_opens
                        .fetch_add(1, Ordering::SeqCst);
                    let behavior = self.control.behavior.lock().await.clone();
                    let tenant_store = self.tenant_store.clone();
                    match behavior {
                        ActorBehavior::Fail(msg) => {
                            let _ = reply.send(Err(msg));
                        }
                        ActorBehavior::HoldUntilRelease {
                            entered,
                            release,
                            persist_before_wait,
                            persist_on_complete,
                        } => {
                            let temp_id = derive_temp_channel_id_from_tlc_key(
                                &command
                                    .external_channel_signer
                                    .as_ref()
                                    .expect("signer")
                                    .base_public_keys
                                    .tlc_base_key,
                            );
                            let opening_result =
                                fiber_json_types::OpenChannelWithExternalFundingResult {
                                    channel_id: temp_id.into(),
                                    unsigned_funding_tx: ckb_jsonrpc_types::Transaction::default(),
                                    opening_context: None,
                                };
                            if persist_before_wait {
                                tenant_store.insert_channel_open_record(ChannelOpenRecord::new(
                                    temp_id,
                                    command.pubkey,
                                    command.funding_amount,
                                ));
                                tenant_store.put_tenant_channel_opening(TenantChannelOpening {
                                    command: command.clone(),
                                    channel_id: temp_id,
                                    result: None,
                                    terminated: false,
                                });
                            }
                            let mut release = release;
                            entered.send_modify(|count| *count += 1);
                            tokio::spawn(async move {
                                let _ = release.wait_for(|&released| released).await;
                                if persist_on_complete {
                                    tenant_store.put_tenant_channel_opening(TenantChannelOpening {
                                        command,
                                        channel_id: temp_id,
                                        result: Some(opening_result.clone()),
                                        terminated: false,
                                    });
                                }
                                let _ = reply.send(Ok(opening_result));
                            });
                        }
                    }
                }
                NetworkActorMessage::Fiber(FiberActorMessage::Command(
                    FiberActorCommand::OpenChannel(command, reply),
                )) => {
                    self.control
                        .admitted_open_channels
                        .fetch_add(1, Ordering::SeqCst);
                    let behavior = self.control.behavior.lock().await.clone();
                    let tenant_store = self.tenant_store.clone();
                    match behavior {
                        ActorBehavior::Fail(msg) => {
                            let _ = reply.send(Err(msg));
                        }
                        ActorBehavior::HoldUntilRelease {
                            entered,
                            release,
                            persist_on_complete,
                            ..
                        } => {
                            let temp_id = Hash256::from([0x91; 32]);
                            let mut release = release;
                            entered.send_modify(|count| *count += 1);
                            tokio::spawn(async move {
                                let _ = release.wait_for(|&released| released).await;
                                if persist_on_complete {
                                    tenant_store.insert_channel_open_record(
                                        ChannelOpenRecord::new(
                                            temp_id,
                                            command.pubkey,
                                            command.funding_amount,
                                        ),
                                    );
                                }
                                let _ = reply.send(Ok(OpenChannelResponse {
                                    channel_id: temp_id,
                                }));
                            });
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct ConcurrentOpenRuntimeFactory {
        store: Store,
        public_node_id: fiber_types::Pubkey,
        tenant_pubkeys: HashMap<TenantId, fiber_types::Pubkey>,
        controls: HashMap<TenantId, Arc<ConcurrentOpenControl>>,
    }

    #[async_trait]
    impl TenantRuntimeFactory for ConcurrentOpenRuntimeFactory {
        fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
            let tenant_pubkey = *self
                .tenant_pubkeys
                .get(tenant_id)
                .ok_or_else(|| format!("unexpected tenant {tenant_id}"))?;
            Ok(HostedTenantRecord {
                tenant_id: tenant_id.clone(),
                root_signer_pubkey: None,
                tenant_pubkey,
                created_at: 42,
                token_epoch: 0,
            })
        }

        async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
            let tenant_store = self
                .store
                .namespaced(NodeNamespace::hosted_tenant(record.tenant_id.as_str()));
            let control = self
                .controls
                .get(&record.tenant_id)
                .expect("tenant control")
                .clone();
            let actor = Actor::spawn(
                None,
                ConcurrentOpenTenantActor {
                    tenant_store: tenant_store.clone(),
                    control,
                },
                (),
            )
            .await
            .map_err(|error| error.to_string())?
            .0;
            let rpc_context = crate::rpc::tenant::HostedTenantRpcContext {
                tenant_id: record.tenant_id.clone(),
                public_node_id: self.public_node_id,
                fiber_actor: crate::fiber::FiberActorRef::from_network(&actor),
                store: tenant_store,
                config: crate::FiberConfig::default(),
            };
            Ok(
                HostedTenantRuntime::network_backed(record.tenant_pubkey, actor)
                    .with_rpc_context(rpc_context),
            )
        }
    }

    let lsp_key = Privkey::from(&[9; 32]);
    let root_a = test_root_signer();
    let root_b = Privkey::from(&[0x66; 32]);
    let tenant_a_expected = TenantId::from_root_signer_pubkey(&root_a.pubkey());
    let tenant_b_expected = TenantId::from_root_signer_pubkey(&root_b.pubkey());
    let tenant_a_key = Privkey::from(&[1; 32]);
    let tenant_b_key = Privkey::from(&[2; 32]);

    // 1. At max_watch_channels_per_tenant = 1: two concurrent open_channel_with_external_funding
    //    requests joined via tokio::join! must serialize atomically: exactly 1 succeeds and 1 is
    //    rejected with "quota exceeded".
    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.max_watch_channels_per_tenant = 1;
    config.max_watch_channels = 2;
    let store = open_lsp_store(&config);
    let (entered_tx, mut entered_rx) = watch::channel(0usize);
    let (release_tx, release_rx) = watch::channel(false);
    let control_a = Arc::new(ConcurrentOpenControl {
        behavior: TokioMutex::new(ActorBehavior::HoldUntilRelease {
            entered: entered_tx,
            release: release_rx,
            persist_before_wait: false,
            persist_on_complete: true,
        }),
        admitted_external_opens: AtomicUsize::new(0),
        admitted_open_channels: AtomicUsize::new(0),
    });
    let mut tenant_pubkeys = HashMap::new();
    tenant_pubkeys.insert(tenant_a_expected.clone(), tenant_a_key.pubkey());
    let mut controls = HashMap::new();
    controls.insert(tenant_a_expected.clone(), control_a.clone());

    let service = start_test_lsp_service(
        store.clone(),
        config.clone(),
        Arc::new(ConcurrentOpenRuntimeFactory {
            store: store.clone(),
            public_node_id: lsp_key.pubkey(),
            tenant_pubkeys,
            controls,
        }),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_a_id = register_authenticated_test_tenant(&service, &lsp_key, &root_a).await;
    let (public_actor, _) = Actor::spawn(None, BusyNetworkActor, ()).await.unwrap();
    let rpc = ChannelRpcServerImpl::new(public_actor.clone(), store.clone())
        .with_lsp_actor(Some(service.clone()));
    let mut ext_a = Extensions::new();
    ext_a.insert(AuthenticatedTenant(tenant_a_id.clone()));

    let (_, signer_1) = make_signer_material(30);
    let (_, signer_2) = make_signer_material(40);
    let params_1 = make_external_params(lsp_key.pubkey(), signer_1);
    let params_2 = make_external_params(lsp_key.pubkey(), signer_2);

    let (res1, res2) = tokio::join!(
        ChannelRpcServer::open_channel_with_external_funding(&rpc, &ext_a, params_1),
        async {
            // Wait until the first request holds its reservation inside the tenant actor so both
            // requests are guaranteed to be in-flight concurrently.
            let _ = entered_rx.wait_for(|&count| count >= 1).await;
            let res =
                ChannelRpcServer::open_channel_with_external_funding(&rpc, &ext_a, params_2).await;
            let _ = release_tx.send(true);
            res
        }
    );
    assert!(res1.is_ok(), "first concurrent open must succeed: {res1:?}");
    let err2 = res2.expect_err("second concurrent open must be rejected by atomic reservation");
    assert!(err2.to_string().contains("quota exceeded"), "{err2}");
    assert_eq!(
        control_a.admitted_external_opens.load(Ordering::SeqCst),
        1,
        "only one concurrent open may pass admission"
    );

    // Zero-gap handoff: once Call 1 confirmed, the persisted TenantChannelOpening continues to
    // occupy the slot, so a sequential 3rd open with a new signer is also rejected.
    let (_, signer_3) = make_signer_material(50);
    let err3 = ChannelRpcServer::open_channel_with_external_funding(
        &rpc,
        &ext_a,
        make_external_params(lsp_key.pubkey(), signer_3),
    )
    .await
    .expect_err("confirmed opening must remain counted without gap");
    assert!(err3.to_string().contains("quota exceeded"), "{err3}");

    // Clear tenant A's persisted opening so we can test RAII error/drop release and open_channel.
    let tenant_a_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_a_id.as_str()));
    for opening in tenant_a_store.list_tenant_channel_openings() {
        tenant_a_store.delete_tenant_channel_opening(&opening.channel_id);
        tenant_a_store.delete_channel_open_record(&opening.channel_id);
    }
    service
        .send_message(LspServiceMessage::RefreshTenantWatchtowerChannelQuota(
            tenant_a_id.clone(),
        ))
        .unwrap();

    // 2. RAII release on RPC error: an actor failure must release the reservation immediately.
    *control_a.behavior.lock().await = ActorBehavior::Fail("simulated actor failure".to_string());
    let (_, signer_err) = make_signer_material(60);
    let actor_err = ChannelRpcServer::open_channel_with_external_funding(
        &rpc,
        &ext_a,
        make_external_params(lsp_key.pubkey(), signer_err),
    )
    .await
    .expect_err("actor error should propagate");
    assert!(
        actor_err.to_string().contains("simulated actor failure"),
        "{actor_err}"
    );
    ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id.clone(), reply)
    })
    .unwrap()
    .expect("failed open must immediately release its quota reservation");

    // 3. RAII release on future cancellation/drop: dropping an in-flight RPC future must release
    //    its quota reservation via WatchtowerQuotaReservationGuard::drop.
    let (drop_entered_tx, mut drop_entered_rx) = watch::channel(0usize);
    let (drop_release_tx, drop_release_rx) = watch::channel(false);
    *control_a.behavior.lock().await = ActorBehavior::HoldUntilRelease {
        entered: drop_entered_tx,
        release: drop_release_rx,
        persist_before_wait: false,
        persist_on_complete: false,
    };
    let (_, signer_drop) = make_signer_material(70);
    let drop_params = make_external_params(lsp_key.pubkey(), signer_drop);
    {
        let mut inflight = Box::pin(ChannelRpcServer::open_channel_with_external_funding(
            &rpc,
            &ext_a,
            drop_params,
        ));
        tokio::select! {
            _ = &mut inflight => panic!("in-flight open should stay pending until released"),
            _ = drop_entered_rx.wait_for(|&count| count >= 1) => {}
        }
        // `inflight` is dropped here while holding the reservation guard.
    }
    let _ = drop_release_tx.send(true);
    ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id.clone(), reply)
    })
    .unwrap()
    .expect("dropped in-flight RPC future must release its quota reservation");

    // 4. Two concurrent `open_channel` requests at quota=1: exactly 1 succeeds and 1 is rejected.
    let (oc_entered_tx, mut oc_entered_rx) = watch::channel(0usize);
    let (oc_release_tx, oc_release_rx) = watch::channel(false);
    *control_a.behavior.lock().await = ActorBehavior::HoldUntilRelease {
        entered: oc_entered_tx,
        release: oc_release_rx,
        persist_before_wait: false,
        persist_on_complete: false,
    };
    let open_channel_params = || OpenChannelParams {
        pubkey: lsp_key.pubkey().into(),
        funding_amount: 100_000_000_000,
        public: Some(false),
        one_way: None,
        funding_udt_type_script: None,
        shutdown_script: None,
        commitment_delay_epoch: None,
        commitment_fee_rate: None,
        funding_fee_rate: None,
        tlc_expiry_delta: None,
        tlc_min_value: None,
        tlc_fee_proportional_millionths: None,
        max_tlc_value_in_flight: None,
        max_tlc_number_in_flight: None,
    };
    let (oc1, oc2) = tokio::join!(
        ChannelRpcServer::open_channel(&rpc, &ext_a, open_channel_params()),
        async {
            let _ = oc_entered_rx.wait_for(|&count| count >= 1).await;
            let res = ChannelRpcServer::open_channel(&rpc, &ext_a, open_channel_params()).await;
            let _ = oc_release_tx.send(true);
            res
        }
    );
    assert!(
        oc1.is_ok(),
        "first concurrent open_channel must succeed: {:?}",
        oc1.err()
    );
    let oc2_err = oc2
        .err()
        .expect("second concurrent open_channel must be rejected");
    assert!(oc2_err.to_string().contains("quota exceeded"), "{oc2_err}");
    assert_eq!(
        control_a.admitted_open_channels.load(Ordering::SeqCst),
        1,
        "only one concurrent open_channel may reach the tenant actor"
    );

    public_actor.stop(None);
    service.stop_and_wait(None, None).await.unwrap();

    // 5. At max_watch_channels_per_tenant = 2, persisting TenantChannelOpening while Call 1 is
    //    still in-flight must NOT double-count Call 1 and falsely reject Call 2; a 3rd concurrent
    //    open must still be rejected.
    let root_dc = tempdir().expect("temporary directory");
    let mut config_dc = lsp_config(root_dc.path().join("lsp"));
    config_dc.max_watch_channels_per_tenant = 2;
    config_dc.max_watch_channels = 4;
    let store_dc = open_lsp_store(&config_dc);
    let (entered_dc_tx, mut entered_dc_rx1) = watch::channel(0usize);
    let mut entered_dc_rx2 = entered_dc_rx1.clone();
    let (release_dc_tx, release_dc_rx) = watch::channel(false);
    let control_dc = Arc::new(ConcurrentOpenControl {
        behavior: TokioMutex::new(ActorBehavior::HoldUntilRelease {
            entered: entered_dc_tx,
            release: release_dc_rx,
            persist_before_wait: true,
            persist_on_complete: true,
        }),
        admitted_external_opens: AtomicUsize::new(0),
        admitted_open_channels: AtomicUsize::new(0),
    });
    let mut tenant_pubkeys_dc = HashMap::new();
    tenant_pubkeys_dc.insert(tenant_a_expected.clone(), tenant_a_key.pubkey());
    let mut controls_dc = HashMap::new();
    controls_dc.insert(tenant_a_expected.clone(), control_dc.clone());

    let service_dc = start_test_lsp_service(
        store_dc.clone(),
        config_dc,
        Arc::new(ConcurrentOpenRuntimeFactory {
            store: store_dc.clone(),
            public_node_id: lsp_key.pubkey(),
            tenant_pubkeys: tenant_pubkeys_dc,
            controls: controls_dc,
        }),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_a_dc = register_authenticated_test_tenant(&service_dc, &lsp_key, &root_a).await;
    let (public_actor_dc, _) = Actor::spawn(None, BusyNetworkActor, ()).await.unwrap();
    let rpc_dc = ChannelRpcServerImpl::new(public_actor_dc.clone(), store_dc.clone())
        .with_lsp_actor(Some(service_dc.clone()));
    let mut ext_dc = Extensions::new();
    ext_dc.insert(AuthenticatedTenant(tenant_a_dc));

    let (_, signer_dc1) = make_signer_material(80);
    let (_, signer_dc2) = make_signer_material(90);
    let (_, signer_dc3) = make_signer_material(100);
    let (dc1, dc2, dc3) = tokio::join!(
        ChannelRpcServer::open_channel_with_external_funding(
            &rpc_dc,
            &ext_dc,
            make_external_params(lsp_key.pubkey(), signer_dc1),
        ),
        async {
            let _ = entered_dc_rx1.wait_for(|&count| count >= 1).await;
            ChannelRpcServer::open_channel_with_external_funding(
                &rpc_dc,
                &ext_dc,
                make_external_params(lsp_key.pubkey(), signer_dc2),
            )
            .await
        },
        async {
            let _ = entered_dc_rx2.wait_for(|&count| count >= 2).await;
            let res = ChannelRpcServer::open_channel_with_external_funding(
                &rpc_dc,
                &ext_dc,
                make_external_params(lsp_key.pubkey(), signer_dc3),
            )
            .await;
            let _ = release_dc_tx.send(true);
            res
        }
    );
    assert!(dc1.is_ok(), "first open at quota=2 must succeed: {dc1:?}");
    assert!(
        dc2.is_ok(),
        "second concurrent open at quota=2 must not be double-counted: {dc2:?}"
    );
    let dc3_err = dc3.expect_err("third concurrent open at quota=2 must be rejected");
    assert!(dc3_err.to_string().contains("quota exceeded"), "{dc3_err}");

    public_actor_dc.stop(None);
    service_dc.stop_and_wait(None, None).await.unwrap();

    // 6. Cross-tenant concurrent race for the last global quota slot
    //    (max_watch_channels_per_tenant = 1, max_watch_channels = 1):
    //    Tenant A and Tenant B race concurrently -> Tenant A takes the global slot while Tenant B
    //    has 0 channels of its own, so Tenant B is rejected with
    //    "global watchtower channel quota exceeded".
    let root_g = tempdir().expect("temporary directory");
    let mut config_g = lsp_config(root_g.path().join("lsp"));
    config_g.max_watch_channels_per_tenant = 1;
    config_g.max_watch_channels = 1;
    let store_g = open_lsp_store(&config_g);
    let (entered_g_tx, mut entered_g_rx) = watch::channel(0usize);
    let (release_g_tx, release_g_rx) = watch::channel(false);
    let control_ga = Arc::new(ConcurrentOpenControl {
        behavior: TokioMutex::new(ActorBehavior::HoldUntilRelease {
            entered: entered_g_tx,
            release: release_g_rx,
            persist_before_wait: false,
            persist_on_complete: true,
        }),
        admitted_external_opens: AtomicUsize::new(0),
        admitted_open_channels: AtomicUsize::new(0),
    });
    let control_gb = Arc::new(ConcurrentOpenControl {
        behavior: TokioMutex::new(ActorBehavior::Fail(
            "Tenant B should be rejected by global quota before actor execution".to_string(),
        )),
        admitted_external_opens: AtomicUsize::new(0),
        admitted_open_channels: AtomicUsize::new(0),
    });
    let mut tenant_pubkeys_g = HashMap::new();
    tenant_pubkeys_g.insert(tenant_a_expected.clone(), tenant_a_key.pubkey());
    tenant_pubkeys_g.insert(tenant_b_expected.clone(), tenant_b_key.pubkey());
    let mut controls_g = HashMap::new();
    controls_g.insert(tenant_a_expected, control_ga.clone());
    controls_g.insert(tenant_b_expected, control_gb.clone());

    let service_g = start_test_lsp_service(
        store_g.clone(),
        config_g,
        Arc::new(ConcurrentOpenRuntimeFactory {
            store: store_g.clone(),
            public_node_id: lsp_key.pubkey(),
            tenant_pubkeys: tenant_pubkeys_g,
            controls: controls_g,
        }),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_a_g = register_authenticated_test_tenant(&service_g, &lsp_key, &root_a).await;
    let tenant_b_g = register_authenticated_test_tenant(&service_g, &lsp_key, &root_b).await;
    let (public_actor_g, _) = Actor::spawn(None, BusyNetworkActor, ()).await.unwrap();
    let rpc_g = ChannelRpcServerImpl::new(public_actor_g.clone(), store_g.clone())
        .with_lsp_actor(Some(service_g.clone()));
    let mut ext_ga = Extensions::new();
    ext_ga.insert(AuthenticatedTenant(tenant_a_g));
    let mut ext_gb = Extensions::new();
    ext_gb.insert(AuthenticatedTenant(tenant_b_g));

    let (_, signer_ga) = make_signer_material(110);
    let (_, signer_gb) = make_signer_material(120);
    let (res_ga, res_gb) = tokio::join!(
        ChannelRpcServer::open_channel_with_external_funding(
            &rpc_g,
            &ext_ga,
            make_external_params(lsp_key.pubkey(), signer_ga),
        ),
        async {
            let _ = entered_g_rx.wait_for(|&count| count >= 1).await;
            let res = ChannelRpcServer::open_channel_with_external_funding(
                &rpc_g,
                &ext_gb,
                make_external_params(lsp_key.pubkey(), signer_gb),
            )
            .await;
            let _ = release_g_tx.send(true);
            res
        }
    );
    assert!(
        res_ga.is_ok(),
        "Tenant A must win the global slot: {res_ga:?}"
    );
    let err_gb = res_gb.expect_err("Tenant B must be rejected by global quota reservation");
    assert!(
        err_gb
            .to_string()
            .contains("global watchtower channel quota exceeded"),
        "{err_gb}"
    );
    assert_eq!(control_ga.admitted_external_opens.load(Ordering::SeqCst), 1);
    assert_eq!(control_gb.admitted_external_opens.load(Ordering::SeqCst), 0);

    public_actor_g.stop(None);
    service_g.stop_and_wait(None, None).await.unwrap();
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn watchtower_rpc_create_and_remove_watch_channel_refresh_lsp_global_quota_cache() {
    use crate::fiber::channel::{
        ChannelActorState, ChannelActorStateStore, ChannelOpenRecordStore, TenantChannelOpening,
    };
    use crate::fiber::network::OpenChannelWithExternalFundingCommand;
    use crate::rpc::watchtower::{
        CreateWatchChannelParams, RemoveWatchChannelParams, RpcContext, WatchtowerRpcServer,
        WatchtowerRpcServerImpl,
    };
    use crate::store::sample::StoreSample;
    use crate::watchtower::WatchtowerStore;

    let root = tempdir().expect("temporary directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.max_watch_channels_per_tenant = 2;
    config.max_watch_channels = 2;
    let store = open_lsp_store(&config);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_a_key = Privkey::from(&[1; 32]);
    let tenant_b_root = Privkey::from(&[0x55; 32]);
    let expected_tenant_b_id = TenantId::from_root_signer_pubkey(&tenant_b_root.pubkey());
    let tenant_b_key = Privkey::from(&[2; 32]);

    let service = start_test_lsp_service(
        store.clone(),
        config.clone(),
        Arc::new(
            FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))
                .with_protocol_secret(expected_tenant_b_id, 2),
        ),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let tenant_a_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let tenant_b_id = register_authenticated_test_tenant(&service, &lsp_key, &tenant_b_root).await;

    // Seed Tenant B with 1 persisted unwatched pending opening ch_b (channels_total = 0,
    // pending_unwatched_by_tenant[B] = 1, pending_unwatched_total = 1).
    let ch_b = Hash256::from([0x92; 32]);
    let tenant_b_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_b_id.as_str()));
    let mut state_b = ChannelActorState::samples(220).remove(0);
    state_b.id = ch_b;
    state_b.remote_pubkey = lsp_key.pubkey();
    tenant_b_store.insert_channel_actor_state(state_b);
    tenant_b_store.put_tenant_channel_opening(TenantChannelOpening {
        command: OpenChannelWithExternalFundingCommand {
            pubkey: lsp_key.pubkey(),
            funding_amount: 100_000_000_000,
            public: false,
            shutdown_script: Script::default(),
            funding_lock_script: Script::default(),
            funding_lock_script_cell_deps: Vec::new(),
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
        channel_id: ch_b,
        result: None,
        terminated: false,
    });
    service
        .send_message(LspServiceMessage::RefreshTenantWatchtowerChannelQuota(
            tenant_b_id.clone(),
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();

    let watchtower_rpc = WatchtowerRpcServerImpl::new(store.clone())
        .with_quotas(config.watchtower_quotas())
        .with_lsp_enabled(true)
        .with_lsp_actor(Some(service.clone()));
    let node_id_b = crate::lsp::tenant_watchtower_node_id(&tenant_b_key.pubkey());
    let ctx_b = RpcContext {
        node_id: node_id_b.to_string(),
        tenant_scoped: true,
    };

    // 1. Tenant B creates a watchtower row for ch_b directly via Watchtower RPC.
    //    `channels_total` becomes 1; LspService must refresh Tenant B's cached pending count
    //    down to 0 so ch_b is NOT double-counted (`1 + 1 >= 2`) against Tenant A.
    let key = Privkey::from(&[0x33; 32]);
    WatchtowerRpcServer::create_watch_channel(
        &watchtower_rpc,
        ctx_b.clone(),
        CreateWatchChannelParams {
            channel_features: fiber_types::ChannelFeatures::V2.into(),
            channel_id: ch_b.into(),
            funding_udt_type_script: None,
            local_settlement_key: None,
            local_settlement_key_pubkey: Some(key.pubkey().into()),
            remote_settlement_key: key.pubkey().into(),
            local_funding_pubkey: key.pubkey().into(),
            remote_funding_pubkey: key.pubkey().into(),
            settlement_data: fiber_json_types::SettlementData {
                local_amount: 1000,
                remote_amount: 1000,
                tlcs: vec![],
            },
            payout_lock_script: None,
        },
    )
    .await
    .expect("tenant create_watch_channel must succeed");
    assert!(store.get_watch_channel(&node_id_b, &ch_b).is_some());
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();

    ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id.clone(), reply)
    })
    .unwrap()
    .expect(
        "create_watch_channel must refresh LspService cache so Tenant A is not falsely rejected",
    );

    // 2. Symmetric case: add 1 watched channel for Tenant A (channels_total = 2), then Tenant B
    //    removes ch_b's watch row via Watchtower RPC while ch_b's non-closed opening remains
    //    persisted. `channels_total` drops to 1 and LspService must refresh Tenant B's pending
    //    count back to 1 so global quota (`1 + 1 >= 2`) still rejects Tenant A.
    let ch_a = Hash256::from([0x93; 32]);
    let mut state_a = ChannelActorState::samples(221).remove(0);
    state_a.id = ch_a;
    state_a.remote_pubkey = tenant_a_key.pubkey();
    state_a.remote_channel_public_keys = Some(state_a.local_channel_public_keys.clone());
    store.insert_channel_actor_state(state_a.clone());
    store
        .namespaced(NodeNamespace::hosted_tenant(tenant_a_id.as_str()))
        .insert_channel_actor_state(state_a);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_a_key.pubkey(),
            ch_a,
        ))
        .unwrap();
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();

    WatchtowerRpcServer::remove_watch_channel(
        &watchtower_rpc,
        ctx_b,
        RemoveWatchChannelParams {
            channel_id: ch_b.into(),
        },
    )
    .await
    .expect("tenant remove_watch_channel must succeed");
    assert!(store.get_watch_channel(&node_id_b, &ch_b).is_none());
    ractor::call!(service, LspServiceMessage::GetStatus).unwrap();

    let err = ractor::call!(service, |reply| {
        LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id, reply)
    })
    .unwrap()
    .expect_err(
        "remove_watch_channel must refresh LspService cache so unwatched opening is counted",
    );
    assert!(
        err.contains("global watchtower channel quota exceeded"),
        "{err}"
    );

    service.stop_and_wait(None, None).await.unwrap();
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn cancelled_open_tenant_channel_aborts_before_persisting_and_channel_created_refreshes_global_quota(
) {
    use crate::fiber::channel::ChannelOpenRecordStore;
    use crate::fiber::network::OpenChannelWithExternalFundingCommand;
    use crate::lsp::FiberTenantRuntimeFactory;
    use crate::tests::NetworkNode;
    use biscuit_auth::KeyPair;
    use ckb_types::prelude::{Builder, Entity, Pack};
    use fiber_types::{ChannelBasePublicKeys, ChannelOpenSignerMaterial};
    use musig2::SecNonce;

    let public_t = NetworkNode::new_n_interconnected_nodes_with_config(1, |index| {
        crate::tests::NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("quota-cancel-{index}")))
            .base_dir_prefix(&format!("quota-cancel-{index}-"))
            .fiber_config_updater(|config| {
                config.auto_accept_channel_ckb_funding_amount = Some(100_000_000_000);
            })
            .build()
    })
    .await
    .pop()
    .expect("one node");

    let root = tempdir().expect("temporary LSP directory");
    let mut config = lsp_config(root.path().join("lsp"));
    config.max_watch_channels_per_tenant = 1;
    config.max_watch_channels = 1;
    let root_actor = crate::tests::get_test_root_actor().await;
    let runtime_factory = Arc::new(FiberTenantRuntimeFactory::new(
        public_t.signing_events.clone(),
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
    let service = Actor::spawn_linked(
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
            signing_events: public_t.signing_events.clone(),
        },
        root_actor.get_cell(),
    )
    .await
    .expect("start LSP service")
    .0;

    let root_a = test_root_signer();
    let root_b = Privkey::from(&[0x77; 32]);
    let tenant_a_id =
        register_authenticated_test_tenant(&service, &public_t.private_key, &root_a).await;
    let tenant_b_id =
        register_authenticated_test_tenant(&service, &public_t.private_key, &root_b).await;

    let tenant_b_ctx = ractor::call!(service, |reply| {
        LspServiceMessage::GetTenantRpcContext(tenant_b_id.clone(), reply)
    })
    .unwrap()
    .expect("activate tenant B runtime");

    let make_command = |seed: u8| {
        let base_keys = ChannelBasePublicKeys {
            funding_pubkey: Privkey::from(&[seed; 32]).pubkey(),
            tlc_base_key: Privkey::from(&[seed.wrapping_add(1); 32]).pubkey(),
        };
        let nonce = |offset: u8| {
            SecNonce::build([seed.wrapping_add(offset); 32])
                .build()
                .public_nonce()
        };
        let signer = ChannelOpenSignerMaterial {
            base_public_keys: base_keys,
            v2_nonces: vec![nonce(2), nonce(3)],
            first_commitment_point: Privkey::from(&[seed.wrapping_add(4); 32]).pubkey(),
            second_commitment_point: Privkey::from(&[seed.wrapping_add(5); 32]).pubkey(),
            commitment_nonce: nonce(6),
            next_commitment_nonce: nonce(7),
            revocation_nonce: nonce(8),
            channel_announcement_nonce: None,
        };
        OpenChannelWithExternalFundingCommand {
            pubkey: public_t.pubkey,
            funding_amount: 100_000_000_000,
            public: false,
            shutdown_script: Script::default(),
            funding_lock_script: Script::new_builder().args([0xaau8; 20].pack()).build(),
            funding_lock_script_cell_deps: Vec::new(),
            funding_udt_type_script: None,
            commitment_fee_rate: None,
            commitment_delay_epoch: None,
            funding_fee_rate: None,
            tlc_expiry_delta: None,
            tlc_min_value: None,
            tlc_fee_proportional_millionths: None,
            max_tlc_value_in_flight: None,
            max_tlc_number_in_flight: None,
            external_channel_signer: Some(signer),
        }
    };

    // (a) A cancelled queued OpenTenantChannel (`reply.is_closed() == true`) must abort before
    //     persisting TenantChannelOpening / ChannelOpenRecord or spawning a channel actor.
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    drop(closed_rx);
    tenant_b_ctx
        .fiber_actor
        .send_message(FiberActorMessage::new_command(
            FiberActorCommand::OpenTenantChannel {
                command: make_command(130),
                admission_error: None,
                reply: closed_tx.into(),
            },
        ))
        .expect("send cancelled OpenTenantChannel");
    // Synchronize with tenant B's actor mailbox so OpenTenantChannel has finished processing.
    let activity = ractor::call_t!(
        tenant_b_ctx.fiber_actor,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetHostedTenantActivity(reply)),
        5_000
    )
    .expect("query tenant activity");
    assert_eq!(activity.pending_channel_operations, 0);
    assert!(
        tenant_b_ctx.store.list_tenant_channel_openings().is_empty(),
        "cancelled OpenTenantChannel must not persist TenantChannelOpening"
    );
    assert!(
        tenant_b_ctx.store.get_channel_open_records().is_empty(),
        "cancelled OpenTenantChannel must not persist ChannelOpenRecord"
    );

    // (b) When Tenant B's FiberActor creates a channel (emitting NetworkServiceEvent::ChannelCreated)
    //     even without an active LspService reservation (e.g. if the HTTP future dropped after
    //     open_tenant_channel_inner began), FiberTenantRuntimeFactory must refresh LspService on
    //     ChannelCreated so Tenant A is immediately rejected on global watchtower quota.
    let (live_tx, _live_rx) = tokio::sync::oneshot::channel();
    tenant_b_ctx
        .fiber_actor
        .send_message(FiberActorMessage::new_command(
            FiberActorCommand::OpenTenantChannel {
                command: make_command(140),
                admission_error: None,
                reply: live_tx.into(),
            },
        ))
        .expect("send live OpenTenantChannel");

    let mut rejected_global = false;
    for _ in 0..100 {
        match ractor::call!(service, |reply| {
            LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_a_id.clone(), reply)
        })
        .unwrap()
        {
            Err(err) if err.contains("global watchtower channel quota exceeded") => {
                rejected_global = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(
        rejected_global,
        "ChannelCreated from Tenant B's runtime must refresh LspService global pending quota so Tenant A is rejected"
    );
    assert!(
        !tenant_b_ctx.store.get_channel_open_records().is_empty(),
        "ChannelOpenRecord must be persisted before ChannelCreated"
    );

    service.stop_and_wait(None, None).await.unwrap();
}

struct CountingGetPaymentNetworkActor;

#[async_trait]
impl Actor for CountingGetPaymentNetworkActor {
    type Msg = NetworkActorMessage;
    type State = Arc<AtomicUsize>;
    type Arguments = Arc<AtomicUsize>;

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
        if let NetworkActorMessage::Fiber(FiberActorMessage::Command(
            FiberActorCommand::GetPayment(_, reply),
        )) = message
        {
            state.fetch_add(1, Ordering::SeqCst);
            let _ = reply.send(Err("payment still in flight".to_string()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn duplicate_resume_delivery_does_not_multiply_reconciliation_timers() {
    let root = tempdir().unwrap();
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let tenant_key = Privkey::from(&[3; 32]);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant = HostedTenantRecord {
        tenant_id: TenantId::new("reconcile-dedup").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let payment_hash = Hash256::from([0x41; 32]);
    let registration = LspInvoiceRegistry::new(store.clone())
        .register(
            &tenant,
            signed_invoice(&tenant_key, payment_hash),
            None,
            lsp_key.pubkey(),
            &lsp_key,
        )
        .unwrap();
    let now = crate::now_timestamp_as_millis_u64();
    let delivery = manager
        .accept(
            &registration,
            &tenant,
            Hash256::from([0x42; 32]),
            hosted_forwarding_request(&tenant, payment_hash, now),
            now,
        )
        .unwrap();
    let key = delivery.key();
    manager.begin_dispatch(&key, now).unwrap();
    manager
        .transition(&key, LspPaymentDeliveryStatus::InFlight, now)
        .unwrap();

    let get_payment_calls = Arc::new(AtomicUsize::new(0));
    let (network, _) = Actor::spawn(
        None,
        CountingGetPaymentNetworkActor,
        get_payment_calls.clone(),
    )
    .await
    .unwrap();
    let (service, _) = Actor::spawn(
        None,
        LspService,
        LspServiceArgs {
            config,
            public_node_id: lsp_key.pubkey(),
            public_network_actor: network.clone(),
            watchtower_store: store.clone(),
            signing_events: Default::default(),
            store,
            runtime_factory: Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
            signing_key: lsp_key,
            token_issuer: test_token_issuer(),
        },
    )
    .await
    .unwrap();

    // Startup queues an initial ResumeDelivery that schedules one 1s reconciliation timer.
    ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000).unwrap();
    assert_eq!(get_payment_calls.load(Ordering::SeqCst), 1);

    // Trigger two additional ResumeDelivery messages (e.g. from tenant channel reconnects)
    // while the previous reconciliation timer is still armed. Each must abort the prior timer
    // rather than spawning another independent 1s loop.
    for _ in 0..2 {
        service
            .send_message(LspServiceMessage::ResumeDelivery(key))
            .unwrap();
        ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000).unwrap();
    }
    let baseline = get_payment_calls.load(Ordering::SeqCst);
    assert_eq!(baseline, 3);

    // Wait for the single surviving 1s timer to fire, then observe an additional 400ms window:
    // if prior timers were not aborted, they would also fire within this window (count > baseline + 1).
    tokio::time::timeout(Duration::from_secs(2), async {
        while get_payment_calls.load(Ordering::SeqCst) < baseline + 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reconciliation timer should fire once after 1s");
    tokio::time::sleep(Duration::from_millis(400)).await;
    ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000).unwrap();
    assert_eq!(
        get_payment_calls.load(Ordering::SeqCst),
        baseline + 1,
        "extra ResumeDelivery calls must abort prior reconciliation timers instead of multiplying 1s loops"
    );

    // Transitioning through PaymentOutcomeReady -> PaymentOutcomeSettled settles the delivery
    // and cancels the reconciliation retry timer.
    assert_eq!(
        ractor::call_t!(
            service,
            |reply| LspServiceMessage::PaymentOutcomeReady(
                PaymentOutcomeRequest {
                    session_key: delivery.request.hosted_session_key(),
                    payment_status: PaymentStatus::Success,
                    failure: None,
                    failure_code: None,
                },
                reply,
            ),
            2_000
        )
        .unwrap()
        .unwrap(),
        PaymentOutcomeDecision::SettleUpstream
    );
    service
        .send_message(LspServiceMessage::PaymentOutcomeSettled(
            SettledPaymentOutcome {
                session_key: delivery.request.hosted_session_key(),
                payment_status: PaymentStatus::Success,
                failure: None,
            },
        ))
        .unwrap();
    ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000).unwrap();
    assert_eq!(
        manager.get(&key).unwrap().unwrap().status,
        LspPaymentDeliveryStatus::Succeeded
    );

    service.stop_and_wait(None, None).await.unwrap();
    network.stop(None);
}

#[tokio::test]
async fn accept_trampoline_delivery_skips_when_reply_port_is_closed() {
    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let factory = Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0))));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_key = Privkey::from(&[1; 32]);
    let service = start_test_lsp_service(
        store,
        config,
        factory,
        lsp_key.clone(),
        dispatches.clone(),
        failures.clone(),
    )
    .await;

    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let payment_hash = Hash256::from([0x51; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, payment_hash).await;
    let private_channel_id = Hash256::from([0x52; 32]);
    service
        .send_message(LspServiceMessage::TenantChannelOnline(
            tenant_key.pubkey(),
            private_channel_id,
        ))
        .unwrap();
    ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000).unwrap();

    let tenant = HostedTenantRecord {
        tenant_id,
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let now = crate::now_timestamp_as_millis_u64();
    let request = hosted_forwarding_request(&tenant, payment_hash, now);

    // Drop the receiver half of the reply port before LspService handles the request,
    // simulating Public T timing out after 15s and failing the upstream TLC back to the payer.
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    drop(closed_rx);
    service
        .send_message(LspServiceMessage::AcceptTrampolineDelivery(
            request.clone(),
            closed_tx.into(),
        ))
        .unwrap();
    ractor::call_t!(service, LspServiceMessage::GetStatus, 2_000).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        manager
            .get_by_payment_hash(&payment_hash)
            .unwrap()
            .is_none(),
        "timed-out AcceptTrampolineDelivery must not persist a delivery record"
    );
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        0,
        "timed-out AcceptTrampolineDelivery must not dispatch downstream"
    );
    assert_eq!(failures.load(Ordering::SeqCst), 0);

    // Positive control: the exact same request with a live reply port is accepted and dispatched.
    assert_eq!(
        ractor::call_t!(
            service,
            |reply| LspServiceMessage::AcceptTrampolineDelivery(request, reply),
            2_000
        )
        .unwrap()
        .unwrap(),
        DeliveryDecision::Buffered
    );
    wait_for_delivery_status(&manager, payment_hash, LspPaymentDeliveryStatus::InFlight).await;
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);

    service.stop_and_wait(None, None).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn non_hosted_trampoline_delivery_port_bypasses_stalled_lsp_past_timeout() {
    use crate::fiber::delivery::{
        HostedDeliveryPort, PaymentOutcomeRequest, SettledPaymentOutcome,
    };
    use crate::fiber_types::{PaymentSessionKey, UpstreamTlcKey};

    struct StallingRuntimeFactory {
        tenant_pubkey: fiber_types::Pubkey,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl TenantRuntimeFactory for StallingRuntimeFactory {
        fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
            Ok(HostedTenantRecord {
                tenant_id: tenant_id.clone(),
                root_signer_pubkey: None,
                tenant_pubkey: self.tenant_pubkey,
                created_at: 42,
                token_epoch: 0,
            })
        }

        async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
            self.entered.notify_one();
            self.release.notified().await;
            let (actor, _) = Actor::spawn(None, NoopNetworkActor, ())
                .await
                .map_err(|e| e.to_string())?;
            Ok(HostedTenantRuntime::network_backed(
                record.tenant_pubkey,
                actor,
            ))
        }
    }

    let root = tempdir().expect("temporary directory");
    let config = lsp_config(root.path().join("lsp"));
    let store = open_lsp_store(&config);
    let lsp_key = Privkey::from(&[9; 32]);
    let tenant_key = Privkey::from(&[1; 32]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let service = start_test_lsp_service(
        store.clone(),
        config,
        Arc::new(StallingRuntimeFactory {
            tenant_pubkey: tenant_key.pubkey(),
            entered: entered.clone(),
            release: release.clone(),
        }),
        lsp_key.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;

    let tenant_id =
        register_authenticated_test_tenant(&service, &lsp_key, &test_root_signer()).await;
    let hosted_hash = Hash256::from([0x71; 32]);
    register_test_invoice(&service, tenant_id.clone(), &tenant_key, hosted_hash).await;
    let tenant = HostedTenantRecord {
        tenant_id: tenant_id.clone(),
        root_signer_pubkey: None,
        tenant_pubkey: tenant_key.pubkey(),
        created_at: 42,
        token_epoch: 0,
    };

    // Stall LspService's actor mailbox inside EnsureTenant -> TenantRuntimeFactory::start.
    let ensure_service = service.clone();
    let ensure_tenant = tenant_id.clone();
    let ensure_task = tokio::spawn(async move {
        let _ = ractor::call!(ensure_service, |reply| {
            LspServiceMessage::EnsureTenant(ensure_tenant, reply)
        });
    });
    entered.notified().await;

    let port = crate::lsp::LspDeliveryPort::new(service.clone(), store.clone());
    let now = crate::now_timestamp_as_millis_u64();

    // Non-hosted trampoline forward must short-circuit via metadata_store without touching
    // the stalled LspService actor.
    let unrelated_target = HostedTenantRecord {
        tenant_id: TenantId::new("unrelated-bob").unwrap(),
        root_signer_pubkey: None,
        tenant_pubkey: Privkey::from(&[77; 32]).pubkey(),
        created_at: 42,
        token_epoch: 0,
    };
    let non_hosted_hash = Hash256::from([0x72; 32]);
    let non_hosted_request = hosted_forwarding_request(&unrelated_target, non_hosted_hash, now);
    assert_eq!(
        port.accept_trampoline_delivery(non_hosted_request.clone())
            .await
            .expect("non-hosted trampoline accept must bypass stalled LSP actor"),
        DeliveryDecision::NotHosted
    );

    let non_hosted_session_key = PaymentSessionKey {
        payment_hash: non_hosted_hash,
        upstream_tlc_key: Some(UpstreamTlcKey {
            incoming_channel_id: non_hosted_request.previous_tlc.prev_channel_id,
            incoming_tlc_id: non_hosted_request.previous_tlc.prev_tlc_id,
        }),
    };
    assert_eq!(
        port.decide_payment_outcome(PaymentOutcomeRequest {
            session_key: non_hosted_session_key,
            payment_status: PaymentStatus::Success,
            failure: None,
            failure_code: None,
        })
        .await
        .expect("non-hosted trampoline outcome must bypass stalled LSP actor"),
        PaymentOutcomeDecision::SettleUpstream
    );
    port.report_settled_payment(SettledPaymentOutcome {
        session_key: non_hosted_session_key,
        payment_status: PaymentStatus::Success,
        failure: None,
    });

    // Positive control: a registered hosted invoice still routes to LspService and times out
    // once the stalled actor holds the mailbox past the 15s DELIVERY_CALL_TIMEOUT_MS.
    let hosted_request = hosted_forwarding_request(&tenant, hosted_hash, now);
    let err = port
        .accept_trampoline_delivery(hosted_request)
        .await
        .expect_err("hosted invoice must still route to stalled LspService and hit 15s timeout");
    assert!(
        err.contains("LSP delivery service unavailable"),
        "unexpected timeout error: {err}"
    );

    // Even after advancing past 15s, another non-hosted forward still succeeds immediately.
    let non_hosted_request_2 =
        hosted_forwarding_request(&unrelated_target, Hash256::from([0x73; 32]), now);
    assert_eq!(
        port.accept_trampoline_delivery(non_hosted_request_2)
            .await
            .expect("non-hosted trampoline forward must still succeed after >15s stall"),
        DeliveryDecision::NotHosted
    );

    release.notify_one();
    ensure_task.await.unwrap();
    service.stop_and_wait(None, None).await.unwrap();
}
