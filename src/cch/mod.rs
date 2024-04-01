use clap_serde_derive::ClapSerde;
use serde::Deserialize;
use serde_with::serde_as;
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

mod service;
use service::{CchService, CchState};

pub async fn start_cch(
    _config: CchConfig,
    command_receiver: mpsc::Receiver<CchCommand>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let service = CchService::new(CchState {
        command_receiver,
        token,
    });
    tracker.spawn(async move {
        service.run().await;
    });
}

// Use prefix `cch-`/`CCH_`
#[derive(ClapSerde, Debug, Clone)]
pub struct CchConfig {}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum CchCommand {
    SendBTC(SendBTC),
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct SendBTC {
    pub btc_pay_req: String,
}
