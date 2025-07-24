use ckb_sdk::{tx_builder::TxBuilderError, unlock::UnlockError, RpcError};
use ractor::RactorErr;
use thiserror::Error;

use crate::ckb::CkbChainMessage;

#[derive(Error, Debug)]
pub enum FundingError {
    #[error("Funding tx is absent")]
    AbsentTx,

    #[error("Failed to build CKB tx: {0}")]
    CkbTxBuilderError(#[from] TxBuilderError),

    #[error("Failed to sign CKB tx: {0}")]
    CkbTxUnlockError(#[from] UnlockError),

    #[error("Dead cell found in the tx")]
    DeadCell,

    #[error("Get overflow error")]
    OverflowError,

    #[error("Failed to call CKB RPC: {0}")]
    CkbRpcError(#[from] RpcError),

    #[error("Failed to send message to CKB chain actor: {0}")]
    RactorError(#[from] RactorErr<CkbChainMessage>),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serde error: {0}")]
    SerdeError(#[from] serde_json::Error),

    #[error("Failed to decode utf-8 string: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),
}

impl FundingError {
    pub fn is_temporary(&self) -> bool {
        use FundingError::*;
        matches!(
            self,
            CkbRpcError(_) | RactorError(_) | IoError(_) | SerdeError(_) | FromUtf8Error(_)
        )
    }
}

#[derive(Error, Debug)]
pub enum CkbChainError {
    #[error("Funding error: {0}")]
    FundingError(#[from] FundingError),
}
