mod config;
pub use config::Config;

pub mod ckb_chain;

pub mod ldk;
pub use ldk::{start_ldk, LdkConfig};
pub mod ckb;
pub use ckb::{start_ckb, CkbConfig, NetworkServiceEvent};
pub mod cch;
pub use cch::{start_cch, CchConfig};

pub mod rpc;
pub use rpc::{start_rpc, RpcConfig};
pub mod invoice;
pub mod store;

mod errors;
pub use errors::{Error, Result};

pub mod actors;

pub mod tasks;

fn get_prefix() -> &'static str {
    static INSTANCE: once_cell::sync::OnceCell<String> = once_cell::sync::OnceCell::new();
    INSTANCE.get_or_init(|| std::env::var("LOG_PREFIX").unwrap_or_else(|_| "".to_string()))
}

macro_rules! define_node_log_functions {
    ($($level:ident => $tracing_fn:ident),+) => {
        $(
            pub fn $level(args: std::fmt::Arguments) {
                tracing::$tracing_fn!("{}{}", get_prefix(), args);
            }
        )+
    };
}

define_node_log_functions!(
    node_debug => debug,
    node_warn => warn,
    node_error => error,
    node_info => info,
    node_trace => trace
);

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

    /// A macro to simplify the usage of `debug_with_node_prefix` function.
    #[macro_export]
    macro_rules! debug {
        ($($arg:tt)*) => {
            $crate::node_debug(format_args!($($arg)*))
        };
    }

    #[macro_export]
    macro_rules! warn {
        ($($arg:tt)*) => {
            $crate::node_warn(format_args!($($arg)*))
        };
    }

    #[macro_export]
    macro_rules! error {
        ($($arg:tt)*) => {
            $crate::node_error(format_args!($($arg)*))
        };
    }

    #[macro_export]
    macro_rules! info {
        ($($arg:tt)*) => {
            $crate::node_info(format_args!($($arg)*))
        };
    }

    #[macro_export]
    macro_rules! trace {
        ($($arg:tt)*) => {
            $crate::node_trace(format_args!($($arg)*))
        };
    }
}
