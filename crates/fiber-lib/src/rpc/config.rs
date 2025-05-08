use clap_serde_derive::ClapSerde;

const DEFAULT_ENABLED_MODULES: &str = "cch,channel,graph,payment,info,invoice,peer";

#[derive(ClapSerde, Debug, Clone)]
pub struct RpcConfig {
    // Don't use default_value here. Otherwise the default value will override config from file
    /// listening port for rpc service
    #[arg(name = "RPC_LISTENING_ADDR", long = "rpc-listening-addr", env)]
    pub listening_addr: Option<String>,

    #[default(DEFAULT_ENABLED_MODULES.split(',').map(ToString::to_string).collect())]
    #[arg(name = "RPC_ENABLED_MODULES", long = "rpc-enabled-modules", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub enabled_modules: Vec<String>,
}

impl RpcConfig {
    pub fn is_module_enabled(&self, module: &str) -> bool {
        self.enabled_modules.iter().any(|m| m == module)
    }
}
