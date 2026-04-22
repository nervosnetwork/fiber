use fiber_store::StorageBackend;
use ractor::{Actor, ActorProcessingErr, ActorRef};
#[cfg(not(any(target_arch = "wasm32", test)))]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

pub struct StoreActorInitializationParameter<S> {
    pub store: S,
    pub backup_path: PathBuf,
    pub ckb_key_path: PathBuf,
    pub fiber_key_path: PathBuf,
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
        #[cfg(not(any(target_arch = "wasm32", test)))]
        perform_key_backup(
            &state.backup_path,
            &state.ckb_key_path,
            &state.fiber_key_path,
        )?;
        match state.store.backup(&state.backup_path) {
            Ok(_) => {
                info!(
                    "StoreActor: Backup successful. Next routine backup in {} hours.",
                    state.backup_interval_hours
                );
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

#[cfg(not(any(target_arch = "wasm32", test)))]
/// Backup the node key files to a specified path.
fn perform_key_backup(
    target_dir: &Path,
    ckb_key_path: &Path,
    fiber_key_path: &Path,
) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(target_dir) {
        return Err(format!(
            "Failed to create backup dir {:?}: {}",
            target_dir, e
        ));
    }
    let keys_to_copy = [(ckb_key_path, "key"), (fiber_key_path, "sk")];

    for (src_file, dest_name) in keys_to_copy {
        if src_file.exists() {
            let dest_file = target_dir.join(dest_name);
            if let Err(e) = std::fs::copy(src_file, &dest_file) {
                return Err(format!("Failed to copy key file {:?}: {}", src_file, e));
            }
            tracing::info!("Successfully backed up key: {}", dest_name);
        } else {
            tracing::warn!("Key file not found at {:?}, skipping", src_file);
        }
    }
    Ok(())
}
