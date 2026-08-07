use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use fiber_store::backend::StorageBackend;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use tempfile::tempdir;

use crate::fiber::network::NetworkActorMessage;
use crate::fiber_types::{Hash256, Privkey};
use crate::invoice::{Currency, InvoiceBuilder};
use crate::lsp::{
    HostedTenantRecord, HostedTenantRuntime, LspConfig, LspInvoiceRegistry, TenantId,
    TenantRegistry, TenantRuntimeFactory, TenantSupervisor, DEFAULT_LSP_BUFFER_DURATION_MS,
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
        node_id: tenant_key.pubkey(),
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
        node_id: tenant_key.pubkey(),
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
        node_id: tenant_key.pubkey(),
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
