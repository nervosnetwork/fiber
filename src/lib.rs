mod config;
pub use config::Config;

#[cfg(test)]
mod tests;
use fiber::types::Hash256;
use rand::Rng;
#[cfg(test)]
pub use tests::*;

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

mod errors;
pub use errors::{Error, Result};

pub mod actors;

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

pub fn gen_rand_sha256_hash() -> Hash256 {
    let mut rng = rand::thread_rng();
    let mut result = [0u8; 32];
    rng.fill(&mut result[..]);
    result.into()
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
