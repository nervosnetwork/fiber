use log::{debug, info};
use tentacle::multiaddr::Multiaddr;
use tokio::sync::mpsc;
use tokio::{select, signal};
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTracker;

use std::str::FromStr;

use ckb_pcn_node::ckb::Command;
use ckb_pcn_node::{start_ckb, start_ldk, Config, CkbConfig};

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

async fn run_ckb(ckb_config: CkbConfig) {
    const CHANNEL_SIZE: usize = 4000;
    let (command_sender, command_receiver) = mpsc::channel(CHANNEL_SIZE);
    info!("Starting ckb");

    assert!(
        ckb_config.bootnode_addrs.len() < CHANNEL_SIZE,
        "Too many bootnodes ({} allowed, having {})",
        CHANNEL_SIZE,
        ckb_config.bootnode_addrs.len()
    );
    for bootnode in &ckb_config.bootnode_addrs {
        let addr = Multiaddr::from_str(bootnode).expect("valid bootnode");
        let command = Command::Connect(addr);
        command_sender
            .send(command)
            .await
            .expect("receiver not closed")
    }

    new_tokio_task_tracker().spawn(async move {
        let token = new_tokio_cancellation_token();
        let _command_sender: mpsc::Sender<ckb_pcn_node::ckb::Command> = command_sender;
        loop {
            select! {
                _ = token.cancelled() => {
                    debug!("Dropping command sender as tokio cancelled");
                    break;
                }
                // TODO: Do some work related to command sender here,
            }
        }
    });

    start_ckb(
        ckb_config,
        command_receiver,
        new_tokio_cancellation_token(),
        new_tokio_task_tracker(),
    )
    .await;
}

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
                run_ckb(ckb_config).await;
            }
        }
    }

    signal::ctrl_c().await.expect("Failed to listen for event");
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.close().await;
}
