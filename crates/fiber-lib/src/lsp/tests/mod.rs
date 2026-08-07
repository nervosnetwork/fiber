use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use fiber_store::backend::StorageBackend;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use tempfile::tempdir;

use crate::fiber::network::NetworkActorMessage;
use crate::fiber_types::Privkey;
use crate::lsp::{
    HostedTenantRecord, HostedTenantRuntime, LspConfig, TenantId, TenantRegistry,
    TenantRuntimeFactory, TenantSupervisor,
};
use crate::store::open_store;

fn lsp_config(base_dir: PathBuf) -> LspConfig {
    LspConfig {
        base_dir: Some(base_dir),
        tenants: Vec::new(),
        max_active_tenants: 64,
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
        node_id: Privkey::from(&[1; 32]).pubkey(),
        created_at: 42,
    };

    assert_eq!(registry.register(record.clone()).unwrap(), record);
    assert_eq!(registry.register(record.clone()).unwrap(), record);

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
        _message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
}

struct FakeRuntimeFactory {
    starts: Arc<AtomicUsize>,
}

#[async_trait]
impl TenantRuntimeFactory for FakeRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        let secret = if tenant_id.as_str() == "u1" { 1 } else { 2 };
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            node_id: Privkey::from(&[secret; 32]).pubkey(),
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
            node_id: record.node_id,
            network_actor: actor,
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

    assert!(supervisor.evict(&u1.tenant_id));
    assert!(!supervisor.is_active(&u1.tenant_id));
    supervisor.ensure(&u2).await.unwrap();
    assert!(supervisor.is_active(&u2.tenant_id));
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
    assert_eq!(u1.listening_addr(), "/ip4/127.0.0.1/tcp/0");
    assert_ne!(u1.store_path(), u2.store_path());
    assert_ne!(u1.public_key().inner_ref(), u2.public_key().inner_ref());
}
