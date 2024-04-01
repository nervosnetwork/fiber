use log::{debug, error, info};
use tentacle::multiaddr::Multiaddr;
use tokio::sync::mpsc;
use tokio::{select, signal};
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTracker;

use std::str::FromStr;

use ckb_pcn_node::cch::CchCommand;
use ckb_pcn_node::ckb::Command;
use ckb_pcn_node::{start_cch, start_ckb, start_ldk, start_rpc, Config};

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

#[tokio::main]
pub async fn main() {
    env_logger::init();

    let config = Config::parse();
    debug!("Parsed config: {:?}", &config);

    if let Some(ldk_config) = config.ldk {
        info!("Starting ldk");
        start_ldk(ldk_config).await;
    }

    let ckb_command_sender = match config.ckb {
        Some(ckb_config) => {
            const CHANNEL_SIZE: usize = 4000;
            let (command_sender, command_receiver) = mpsc::channel(CHANNEL_SIZE);
            assert!(
                ckb_config.bootnode_addrs.len() < CHANNEL_SIZE,
                "Too many bootnodes ({} allowed, having {})",
                CHANNEL_SIZE,
                ckb_config.bootnode_addrs.len()
            );
            for bootnode in &ckb_config.bootnode_addrs {
                let addr = Multiaddr::from_str(bootnode).expect("valid bootnode");
                let command = Command::ConnectPeer(addr);
                command_sender
                    .send(command)
                    .await
                    .expect("receiver not closed")
            }

            let (event_sender, mut event_receiver) = mpsc::channel(CHANNEL_SIZE);

            info!("Starting ckb");
            start_ckb(
                ckb_config,
                command_receiver,
                event_sender,
                new_tokio_cancellation_token(),
                new_tokio_task_tracker(),
            )
            .await;

            new_tokio_task_tracker().spawn(async move {
                let token = new_tokio_cancellation_token();
                loop {
                    select! {
                        _ = token.cancelled() => {
                            debug!("Cancellation received, event processing service");
                            break;
                        }
                        event = event_receiver.recv() => {
                            match event {
                                None => {
                                    debug!("Event receiver completed, event processing service");
                                    break;
                                }
                                Some(event) => {
                                    debug!("Received event: {:?}", event);
                                }
                            }
                        }
                    }
                }
                debug!("Event processing service exited");
            });

            // Just keep command sender around until tasks finish.
            // This is a hack to keep the command receiver alive until the tasks are done.
            let cloned_command_sender = command_sender.clone();
            new_tokio_task_tracker().spawn(async move {
                let _command_sender: mpsc::Sender<ckb_pcn_node::ckb::Command> =
                    cloned_command_sender;
                new_tokio_cancellation_token().cancelled().await;
            });

            Some(command_sender)
        }
        None => None,
    };

    let cch_command_sender = match config.cch {
        Some(cch_config) => {
            const CHANNEL_SIZE: usize = 4000;
            let (command_sender, command_receiver) = mpsc::channel::<CchCommand>(CHANNEL_SIZE);
            info!("Starting ldk");
            start_cch(cch_config, command_receiver).await;
            Some(command_sender)
        }
        None => None,
    };

    // Start rpc service
    if let Some(rpc_config) = config.rpc {
        if ckb_command_sender.is_none() && cch_command_sender.is_none() {
            error!("Rpc service requires ckb or chh service to be started. Exiting.");
            return;
        }

        let shutdown_signal = async {
            let token = new_tokio_cancellation_token();
            token.cancelled().await;
        };
        new_tokio_task_tracker().spawn(async move {
            start_rpc(
                rpc_config,
                ckb_command_sender,
                cch_command_sender,
                shutdown_signal,
            )
            .await;
        });
    };

    signal::ctrl_c().await.expect("Failed to listen for event");
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.close().await;
}
