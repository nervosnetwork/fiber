use super::*;
use crate::actors::RootActor;
use crate::store::open_store;
use crate::tasks::{new_tokio_cancellation_token, new_tokio_task_tracker};
use std::sync::{Arc, Mutex};

struct WarnEvents {
    messages: Arc<Mutex<Vec<String>>>,
    _guard: tracing::dispatcher::DefaultGuard,
    _second: tracing::Dispatch,
}

impl WarnEvents {
    fn capture() -> Self {
        use tracing_subscriber::layer::SubscriberExt;

        struct Recorder(Arc<Mutex<Vec<String>>>);
        struct Message(String);

        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if *event.metadata().level() == tracing::Level::WARN {
                    let mut message = Message(String::new());
                    event.record(&mut message);
                    self.0.lock().unwrap().push(message.0);
                }
            }
        }

        let messages = Arc::new(Mutex::new(Vec::new()));
        let recorder =
            tracing::Dispatch::new(tracing_subscriber::registry().with(Recorder(messages.clone())));
        let second = tracing::Dispatch::new(tracing_subscriber::registry());
        Self {
            messages,
            _guard: tracing::dispatcher::set_default(&recorder),
            _second: second,
        }
    }

    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn test_backup_warns_when_hosted_tenants_exist_without_tenant_store_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let db_dir = root.path().join("db");
    let ckb_key_path = root.path().join("key");
    let fiber_key_path = root.path().join("sk");
    std::fs::write(&ckb_key_path, b"ckb-key").expect("write ckb key");
    std::fs::write(&fiber_key_path, b"fiber-sk").expect("write fiber sk");

    let store = open_store(&db_dir).expect("open store");
    // Non-tenant namespace (LSP metadata) must not trigger the tenant-key warning.
    store
        .namespaced(NodeNamespace::lsp_metadata())
        .put(b"meta", b"v");

    let (tracker, token) = (new_tokio_task_tracker(), new_tokio_cancellation_token());
    let root_actor = RootActor::start_for_test(tracker, token).await;

    let warnings = WarnEvents::capture();

    // 1. No hosted tenant namespaces in DB, tenant_store_root = None -> no warning.
    let (actor_no_tenants, _) = ractor::Actor::spawn_linked(
        None,
        StoreActor::new(),
        StoreActorInitializationParameter {
            store: store.clone(),
            backup_path: root.path().join("backup-no-tenants"),
            ckb_key_path: ckb_key_path.clone(),
            fiber_key_path: fiber_key_path.clone(),
            tenant_store_root: None,
            backup_interval_hours: 24,
        },
        root_actor.get_cell(),
    )
    .await
    .expect("spawn StoreActor");
    let res =
        ractor::call!(actor_no_tenants, StoreActorMessage::ForceBackup).expect("call ForceBackup");
    assert!(
        res.is_ok(),
        "backup without tenants should succeed: {res:?}"
    );
    assert!(
        warnings.messages().is_empty(),
        "expected no warnings when no hosted tenant namespaces exist, got {:?}",
        warnings.messages()
    );

    // 2. Add a hosted tenant namespace, keep tenant_store_root = None -> warn, still succeed.
    store
        .namespaced(NodeNamespace::hosted_tenant("u1"))
        .put(b"tenant-key", b"tenant-val");
    let (actor_missing_root, _) = ractor::Actor::spawn_linked(
        None,
        StoreActor::new(),
        StoreActorInitializationParameter {
            store: store.clone(),
            backup_path: root.path().join("backup-missing-root"),
            ckb_key_path: ckb_key_path.clone(),
            fiber_key_path: fiber_key_path.clone(),
            tenant_store_root: None,
            backup_interval_hours: 24,
        },
        root_actor.get_cell(),
    )
    .await
    .expect("spawn StoreActor");
    let res = ractor::call!(actor_missing_root, StoreActorMessage::ForceBackup)
        .expect("call ForceBackup");
    assert!(
        res.is_ok(),
        "backup with hosted tenants and no tenant_store_root should warn and still succeed: {res:?}"
    );
    let emitted = warnings.messages();
    assert_eq!(
        emitted.len(),
        1,
        "expected exactly one warning when tenant_store_root is None and hosted tenants exist, got {emitted:?}"
    );
    assert!(
        emitted[0].contains("tenant_store_root") && emitted[0].contains("hosted tenant"),
        "unexpected warning message: {}",
        emitted[0]
    );

    // 3. Configure tenant_store_root -> no additional warning and tenant key is backed up.
    let tenants_root = root.path().join("tenants");
    std::fs::create_dir_all(tenants_root.join("u1")).expect("create tenant dir");
    std::fs::write(tenants_root.join("u1").join("sk"), b"tenant-u1-sk").expect("write tenant sk");
    let backup_with_root = root.path().join("backup-with-root");
    let (actor_with_root, _) = ractor::Actor::spawn_linked(
        None,
        StoreActor::new(),
        StoreActorInitializationParameter {
            store,
            backup_path: backup_with_root.clone(),
            ckb_key_path,
            fiber_key_path,
            tenant_store_root: Some(tenants_root),
            backup_interval_hours: 24,
        },
        root_actor.get_cell(),
    )
    .await
    .expect("spawn StoreActor");
    let res =
        ractor::call!(actor_with_root, StoreActorMessage::ForceBackup).expect("call ForceBackup");
    assert!(
        res.is_ok(),
        "backup with tenant_store_root should succeed: {res:?}"
    );
    assert_eq!(
        warnings.messages().len(),
        1,
        "expected no new warning when tenant_store_root is configured, got {:?}",
        warnings.messages()
    );
    let backed_up_dir = std::fs::read_dir(&backup_with_root)
        .unwrap()
        .next()
        .expect("backup timestamp dir")
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read(backed_up_dir.join("tenants").join("u1").join("sk")).unwrap(),
        b"tenant-u1-sk"
    );
}
