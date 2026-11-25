#[cfg(not(target_arch = "wasm32"))]
use crate::store::NodeNamespace;
use fiber_store::StorageBackend;
use fiber_types::now_timestamp_as_millis_u64;
use ractor::{Actor, ActorProcessingErr, ActorRef};
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

pub struct StoreActorInitializationParameter<S> {
    pub store: S,
    pub backup_path: PathBuf,
    pub ckb_key_path: PathBuf,
    pub fiber_key_path: PathBuf,
    pub tenant_store_root: Option<PathBuf>,
    pub backup_interval_hours: u64,
}

pub enum StoreActorMessage {
    /// Backup requests triggered when channel status changes
    RequestBackup,
    /// Scheduler tick to check if the deadline is reached
    PeriodicCheck,
    /// Manual trigger for rpc
    ForceBackup(ractor::RpcReplyPort<Result<(), String>>),
}

pub struct StoreActorState<S> {
    pub store: S,
    pub backup_path: PathBuf,
    pub ckb_key_path: PathBuf,
    pub fiber_key_path: PathBuf,
    pub tenant_store_root: Option<PathBuf>,
    pub backup_interval_hours: u64,
    /// The specific moment the next backup is scheduled to run
    pub next_backup_time: Instant,
}

pub struct StoreActor<S> {
    pub _phantom: std::marker::PhantomData<S>,
}

impl<S> StoreActor<S> {
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<S> Default for StoreActor<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl<S> Actor for StoreActor<S>
where
    S: StorageBackend + Send + Sync + 'static,
{
    type Msg = StoreActorMessage;
    type State = StoreActorState<S>;
    type Arguments = StoreActorInitializationParameter<S>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // Run a high-frequency tick (e.g., 1s) to act as the scheduler engine
        myself.send_interval(Duration::from_secs(1), || StoreActorMessage::PeriodicCheck);

        let first_deadline = Instant::now() - Duration::from_secs(61);

        Ok(StoreActorState {
            store: args.store,
            backup_path: args.backup_path,
            ckb_key_path: args.ckb_key_path,
            fiber_key_path: args.fiber_key_path,
            tenant_store_root: args.tenant_store_root,
            backup_interval_hours: args.backup_interval_hours,
            next_backup_time: first_deadline,
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let now = Instant::now();
        match message {
            StoreActorMessage::RequestBackup => {
                // Deadline Pulling Logic:
                // If the currently scheduled backup is more than 60s away,
                // pull the deadline closer to (now + 60s).
                // If now time >= scheduled backup,
                // then backup now.
                if now >= state.next_backup_time {
                    state.next_backup_time = now;
                    debug!("StoreActor: Deadline reached, scheduling immediate backup.");
                } else if state.next_backup_time.saturating_duration_since(now)
                    > Duration::from_secs(60)
                {
                    state.next_backup_time = now + Duration::from_secs(60);
                    debug!("StoreActor: High-priority change detected. Backup scheduled for 60s from now.");
                }
            }
            StoreActorMessage::PeriodicCheck => {
                // Scheduler Engine:
                // Execute backup if the deadline has been reached or passed.
                if now >= state.next_backup_time {
                    if let Err(e) = self.do_backup(state).await {
                        error!("StoreActor: Scheduled backup failed but continuing: {}", e);
                    }
                    // After success, reset the deadline to the routine interval (e.g., 24h)
                    state.next_backup_time =
                        now + Duration::from_secs(state.backup_interval_hours * 3600);
                }
            }
            StoreActorMessage::ForceBackup(reply) => {
                // Backup immediately
                let result = self.do_backup(state).await;
                state.next_backup_time =
                    Instant::now() + Duration::from_secs(state.backup_interval_hours * 3600);
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
        }
        Ok(())
    }
}

impl<S> StoreActor<S>
where
    S: StorageBackend + Send + Sync + 'static,
{
    async fn do_backup(&self, state: &mut StoreActorState<S>) -> Result<(), String> {
        info!("StoreActor: Starting backup to {:?}", state.backup_path);
        let timestamp = now_timestamp_as_millis_u64().to_string();
        let backup_path = state.backup_path.join(timestamp);
        match state.store.backup(&backup_path) {
            Ok(_) => {
                info!(
                    "StoreActor: Backup successful. Next routine backup in {} hours.",
                    state.backup_interval_hours
                );
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let hosted_tenant_count = if state.tenant_store_root.is_none() {
                        hosted_tenant_namespaces_in_backend(&state.store).len()
                    } else {
                        0
                    };
                    perform_key_backup_inner(
                        &backup_path,
                        &state.ckb_key_path,
                        &state.fiber_key_path,
                        state.tenant_store_root.as_deref(),
                        hosted_tenant_count,
                    )?;
                }
                Ok(())
            }
            Err(e) => {
                error!("StoreActor: Backup failed: {:?}", e);
                Err(format!("Backup failed: {e}"))
                // Note: We don't reset the deadline here to avoid an immediate retry loop
                // if the failure is persistent (e.g., disk full).
                // It will retry in the next routine cycle or on next RequestBackup.
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn hosted_tenant_namespaces_in_backend<S: StorageBackend>(store: &S) -> Vec<NodeNamespace> {
    let mut seen = HashSet::new();
    let mut namespaces = Vec::new();
    for (key, _) in store.prefix_iterator(NodeNamespace::physical_prefix().to_vec()) {
        if let Some(namespace) = NodeNamespace::parse_hosted_tenant_from_physical_key(&key) {
            if seen.insert(namespace.clone()) {
                namespaces.push(namespace);
            }
        }
    }
    namespaces
}

#[cfg(all(test, not(target_arch = "wasm32")))]
/// Backup the node key files (and hosted tenant identity keys, if present) to a
/// specified path.
pub(crate) fn perform_key_backup(
    target_dir: &Path,
    ckb_key_path: &Path,
    fiber_key_path: &Path,
    tenant_store_root: Option<&Path>,
) -> Result<(), String> {
    perform_key_backup_inner(
        target_dir,
        ckb_key_path,
        fiber_key_path,
        tenant_store_root,
        0,
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn perform_key_backup_inner(
    target_dir: &Path,
    ckb_key_path: &Path,
    fiber_key_path: &Path,
    tenant_store_root: Option<&Path>,
    hosted_tenant_count: usize,
) -> Result<(), String> {
    create_private_dir_all(target_dir)
        .map_err(|e| format!("Failed to create backup dir {:?}: {}", target_dir, e))?;
    let keys_to_copy = [(ckb_key_path, "key"), (fiber_key_path, "sk")];

    for (src_file, dest_name) in keys_to_copy {
        if src_file.is_file() {
            let dest_file = target_dir.join(dest_name);
            copy_private_key_file(src_file, &dest_file)
                .map_err(|e| format!("Failed to copy key file {:?}: {}", src_file, e))?;
            tracing::info!("Successfully backed up key: {}", dest_name);
        } else {
            tracing::warn!("Key file not found at {:?}, skipping", src_file);
        }
    }

    if let Some(tenants_root) = tenant_store_root {
        if tenants_root.is_dir() {
            let entries = std::fs::read_dir(tenants_root).map_err(|e| {
                format!("Failed to read tenant store root {:?}: {}", tenants_root, e)
            })?;
            let dest_tenants_root = target_dir.join("tenants");
            let mut backed_up_tenants = 0usize;
            for entry in entries {
                let entry = entry.map_err(|e| format!("Failed to read tenant entry: {}", e))?;
                let file_type = entry.file_type().map_err(|e| {
                    format!("Failed to inspect tenant entry {:?}: {}", entry.path(), e)
                })?;
                if !file_type.is_dir() {
                    continue;
                }
                let src_key = entry.path().join("sk");
                if !src_key.is_file() {
                    continue;
                }
                if backed_up_tenants == 0 {
                    create_private_dir_all(&dest_tenants_root).map_err(|e| {
                        format!(
                            "Failed to create tenant backup dir {:?}: {}",
                            dest_tenants_root, e
                        )
                    })?;
                }
                let dest_tenant_dir = dest_tenants_root.join(entry.file_name());
                create_private_dir_all(&dest_tenant_dir).map_err(|e| {
                    format!(
                        "Failed to create tenant backup dir {:?}: {}",
                        dest_tenant_dir, e
                    )
                })?;
                let dest_key = dest_tenant_dir.join("sk");
                copy_private_key_file(&src_key, &dest_key)
                    .map_err(|e| format!("Failed to copy tenant key {:?}: {}", src_key, e))?;
                backed_up_tenants += 1;
            }
            if backed_up_tenants > 0 {
                tracing::info!(
                    "Successfully backed up {} hosted tenant key(s)",
                    backed_up_tenants
                );
            }
        }
    } else if hosted_tenant_count > 0 {
        tracing::warn!(
            "StoreActor: Database contains {} hosted tenant namespace(s), but tenant_store_root is not configured; hosted tenant private keys (`sk`) are omitted from backup {:?}",
            hosted_tenant_count,
            target_dir
        );
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn copy_private_key_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
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
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
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
            let recorder = tracing::Dispatch::new(
                tracing_subscriber::registry().with(Recorder(messages.clone())),
            );
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
        let res = ractor::call!(actor_no_tenants, StoreActorMessage::ForceBackup)
            .expect("call ForceBackup");
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
        std::fs::write(tenants_root.join("u1").join("sk"), b"tenant-u1-sk")
            .expect("write tenant sk");
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
        let res = ractor::call!(actor_with_root, StoreActorMessage::ForceBackup)
            .expect("call ForceBackup");
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
}
