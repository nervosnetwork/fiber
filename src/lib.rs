mod config;
pub use config::Config;

pub mod ckb;
pub mod fiber;
pub use fiber::{start_network, FiberConfig, NetworkServiceEvent};
pub mod cch;
pub use cch::{start_cch, CchActor, CchConfig};
pub mod rpc;
pub use rpc::{start_rpc, RpcConfig};
pub mod invoice;
pub mod store;
pub mod watchtower;
pub use errors::{Error, Result};
pub mod actors;
mod errors;
pub mod tasks;

use git_version::git_version;

const GIT_VERSION: &str = git_version!();

pub fn get_git_versin() -> &'static str {
    GIT_VERSION
}

pub fn get_node_prefix() -> &'static str {
    static INSTANCE: once_cell::sync::OnceCell<String> = once_cell::sync::OnceCell::new();
    INSTANCE.get_or_init(|| std::env::var("LOG_PREFIX").unwrap_or_else(|_| "".to_string()))
}

pub fn now_timestamp_as_millis_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Duration since unix epoch")
        .as_millis() as u64
}

pub mod macros {
    #[macro_export]
    macro_rules! unwrap_or_return {
        ($expr:expr, $msg:expr) => {
            match $expr {
                Ok(val) => val,
                Err(err) => {
                    error!("{}: {:?}", $msg, err);
                    return;
                }
            }
        };
        ($expr:expr) => {
            match $expr {
                Ok(val) => val,
                Err(err) => {
                    error!("{:?}", err);
                    return;
                }
            }
        };
    }
}
