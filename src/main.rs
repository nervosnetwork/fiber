use ckb_pcn_node::{start_ckb, start_ldk, Config};
use log::{debug, info};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTracker;

#[tokio::main]
pub async fn main() {
    env_logger::init();

    let config = Config::parse();
    debug!("Parsed config: {:?}", &config);

    match config {
        Config { ckb, ldk } => {
            if let Some(ldk_config) = ldk {
                info!("Starting ldk");
                start_ldk(ldk_config).await;
            }
            if let Some(ckb_config) = ckb {
                info!("Starting ckb");
                start_ckb(
                    ckb_config,
                    new_tokio_cancellation_token(),
                    new_tokio_task_tracker(),
                )
                .await;
            }
        }
    }

    signal::ctrl_c().await.expect("Failed to listen for event");
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.close().await;
}

#[derive(Debug, Clone)]
struct TaskTrackerWithCancellation {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl TaskTrackerWithCancellation {
    fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            token: CancellationToken::new(),
        }
    }

    async fn close(&self) {
        self.token.cancel();
        self.tracker.close();
        self.tracker.wait().await;
    }
}

static TOKIO_TASK_TRACKER_WITH_CANCELLATION: once_cell::sync::Lazy<TaskTrackerWithCancellation> =
    once_cell::sync::Lazy::new(TaskTrackerWithCancellation::new);

/// Create a new CancellationToken for exit signal
fn new_tokio_cancellation_token() -> CancellationToken {
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.token.clone()
}

/// Create a new TaskTracker to track task progress
fn new_tokio_task_tracker() -> TaskTracker {
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.tracker.clone()
}
