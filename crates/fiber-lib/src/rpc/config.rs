use clap_serde_derive::ClapSerde;

#[cfg(not(feature = "watchtower"))]
const DEFAULT_ENABLED_MODULES: &str = "cch,channel,graph,payment,info,invoice,peer";
#[cfg(feature = "watchtower")]
const DEFAULT_ENABLED_MODULES: &str = "cch,channel,graph,payment,info,invoice,peer,watchtower";

#[derive(ClapSerde, Debug, Clone)]
pub struct RpcConfig {
    // Don't use default_value here. Otherwise the default value will override config from file
    /// listening port for rpc service
    #[arg(name = "RPC_LISTENING_ADDR", long = "rpc-listening-addr", env)]
    pub listening_addr: Option<String>,

    #[default(None)]
    #[arg(name = "RPC_BISCUIT_PUBLIC_KEY", long = "rpc-biscuit-public-key", env)]
    pub biscuit_public_key: Option<String>,

    #[default(DEFAULT_ENABLED_MODULES.split(',').map(ToString::to_string).collect())]
    #[arg(name = "RPC_ENABLED_MODULES", long = "rpc-enabled-modules", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub enabled_modules: Vec<String>,

    /// Enable CORS (Cross-Origin Resource Sharing) for HTTP RPC
    #[default(false)]
    #[arg(name = "RPC_CORS_ENABLED", long = "rpc-cors-enabled", env)]
    pub cors_enabled: bool,

    /// Allowed origins for CORS. If empty when CORS is enabled, allows all origins (*)
    #[default(Vec::new())]
    #[arg(name = "RPC_CORS_ALLOWED_ORIGINS", long = "rpc-cors-allowed-origins", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub cors_allowed_origins: Vec<String>,
}

impl RpcConfig {
    pub fn is_module_enabled(&self, module: &str) -> bool {
        self.enabled_modules.iter().any(|m| m == module)
    }
}
