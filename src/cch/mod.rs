use clap_serde_derive::ClapSerde;
use serde::Deserialize;
use serde_with::serde_as;
use tokio::sync::mpsc;

pub async fn start_cch(_config: CchConfig, _command_receiver: mpsc::Receiver<CchCommand>) {}

// Use prefix `cch-`/`CCH_`
#[derive(ClapSerde, Debug, Clone)]
pub struct CchConfig;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum CchCommand {}
