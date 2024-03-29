use clap_serde_derive::ClapSerde;

pub async fn start_cch(_config: CchConfig) {}

// Use prefix `cch-`/`CCH_`
#[derive(ClapSerde, Debug, Clone)]
pub struct CchConfig;
