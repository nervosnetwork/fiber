mod config;
pub use config::Config;

#[cfg(any(test, feature = "bench"))]
pub mod tests;

use fiber::types::Hash256;
use rand::Rng;
#[cfg(any(test, feature = "bench"))]
pub use tests::*;

pub mod ckb;
pub mod fiber;
pub use fiber::{start_network, FiberConfig, NetworkServiceEvent};
#[cfg(not(target_arch = "wasm32"))]
pub mod cch;
#[cfg(not(target_arch = "wasm32"))]
pub use cch::{start_cch, CchActor, CchConfig};

pub mod invoice;
pub mod rpc;
pub mod store;
#[cfg(feature = "watchtower")]
pub mod watchtower;

mod errors;
pub use errors::{Error, Result};

pub mod actors;

pub mod tasks;

pub mod utils;

use git_version::git_version;

const GIT_VERSION: &str = git_version!(fallback = "unknown");

pub fn get_git_version() -> &'static str {
    GIT_VERSION
}

pub fn get_git_commit_info() -> String {
    format!(
        "{} {}",
        option_env!("GIT_COMMIT_HASH").unwrap_or("unknown"),
        option_env!("GIT_COMMIT_DATE").unwrap_or("unknown")
    )
}

pub fn get_node_prefix() -> &'static str {
    static INSTANCE: once_cell::sync::OnceCell<String> = once_cell::sync::OnceCell::new();
    INSTANCE.get_or_init(|| std::env::var("LOG_PREFIX").unwrap_or_else(|_| "".to_string()))
}
pub fn now_timestamp_as_millis_u64() -> u64 {
    time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .expect("Duration since unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
thread_local! {
    static MOCKED_TIME: std::cell::RefCell<Option<u64>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn mock_timestamp_as_millis_u64() -> u64 {
    MOCKED_TIME.with(|time| {
        let t = time.borrow();
        match *t {
            Some(mocked_time) => mocked_time,
            None => now_timestamp_as_millis_u64(),
        }
    })
}

#[cfg(test)]
pub fn set_mocked_time(time: u64) {
    MOCKED_TIME.with(|t| {
        *t.borrow_mut() = Some(time);
    });
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

#[cfg(not(target_arch = "wasm32"))]
pub fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    tokio::task::block_in_place(f)
}
#[cfg(target_arch = "wasm32")]
pub fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

#[cfg(not(target_arch = "wasm32"))]
use std::time;
#[cfg(target_arch = "wasm32")]
use web_time as time;

#[cfg(all(test, target_arch = "wasm32"))]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
