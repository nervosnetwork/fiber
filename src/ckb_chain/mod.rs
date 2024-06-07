mod actor;
mod config;
mod error;
mod funding;

pub use actor::{CkbChainActor, CkbChainMessage, TraceTxRequest};
pub use config::{CkbChainConfig, DEFAULT_CKB_CHAIN_BASE_DIR_NAME};
pub use error::{CkbChainError, FundingError};
pub use funding::{FundingRequest, FundingTx, FundingUdtInfo};

#[cfg(test)]
pub use actor::{submit_tx, MockChainActor};
pub mod contracts;
