use ckb_pcn_node::{start_ckb, start_ldk, Config};
use log::{debug, info};
use tokio::signal;
use tokio_util::sync::CancellationToken;

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
                start_ckb(ckb_config, new_tokio_exit_rx()).await;
            }
        }
    }

    let _ = signal::ctrl_c().await.expect("Failed to listen for event");
    broadcast_exit_signals();
}

static TOKIO_EXIT: once_cell::sync::Lazy<CancellationToken> =
    once_cell::sync::Lazy::new(CancellationToken::new);

/// Create a new CancellationToken for exit signal
pub fn new_tokio_exit_rx() -> CancellationToken {
    TOKIO_EXIT.clone()
}

/// Broadcast exit signals to all threads and all tokio tasks
pub fn broadcast_exit_signals() {
    debug!("Received exit signal; broadcasting exit signal to all threads");
    TOKIO_EXIT.cancel();
}
