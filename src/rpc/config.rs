use clap_serde_derive::ClapSerde;

#[derive(ClapSerde, Debug, Clone)]
pub struct RpcConfig {
    // Don't use default_value here. Otherwise the default value will override config from file
    /// listening port for rpc service
    #[arg(name = "RPC_LISTENING_ADDR", long = "rpc-listening-addr", env)]
    pub listening_addr: Option<String>,
}
