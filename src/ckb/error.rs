use ckb_sdk::{tx_builder::TxBuilderError, unlock::UnlockError, RpcError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FundingError {
    #[error("Funding tx is absent")]
    AbsentTx,

    #[error("Failed to call CKB node RPC: {0}")]
    CkbRpcError(#[from] RpcError),

    #[error("Failed to build CKB tx: {0}")]
    CkbTxBuilderError(#[from] TxBuilderError),

    #[error("Failed to sign CKB tx: {0}")]
    CkbTxUnlockError(#[from] UnlockError),

    #[error("Dead cell found in the tx")]
    DeadCell,

    #[error("The channel is invalid to fund")]
    InvalidChannel,
}

#[derive(Error, Debug)]
pub enum CkbChainError {
    #[error("Funding error: {0}")]
    FundingError(#[from] FundingError),
}
