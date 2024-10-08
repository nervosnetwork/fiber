mod actor;
mod error;
mod funding;

pub use actor::{CkbChainActor, CkbChainMessage, TraceTxRequest, TraceTxResponse};
pub use config::{CkbConfig, DEFAULT_CKB_BASE_DIR_NAME};
pub use error::{CkbChainError, FundingError};
pub use funding::{FundingRequest, FundingTx};

#[cfg(test)]
pub use actor::{submit_tx, trace_tx, trace_tx_hash, MockChainActor};
pub mod config;
pub mod contracts;
