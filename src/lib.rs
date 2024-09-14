mod config;
pub use config::Config;

pub mod ckb;
pub mod ldk;
pub use ldk::{start_ldk, LdkConfig};
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

pub fn get_node_prefix() -> &'static str {
    static INSTANCE: once_cell::sync::OnceCell<String> = once_cell::sync::OnceCell::new();
    INSTANCE.get_or_init(|| std::env::var("LOG_PREFIX").unwrap_or_else(|_| "".to_string()))
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
