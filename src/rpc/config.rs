use clap_serde_derive::ClapSerde;

#[derive(ClapSerde, Debug, Clone)]
pub struct RpcConfig {
    /// listening port for rpc service
    #[arg(name = "RPC_LISTENING_ADDR", long = "rpc-listening-addr", default_value = "[::]:0", env)]
    pub listening_addr: String,
}
