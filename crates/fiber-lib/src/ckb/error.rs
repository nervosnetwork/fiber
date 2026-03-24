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

    #[error("Peer sent us an invalid funding tx")]
    InvalidPeerFundingTx,

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

/// Walk the `source()` chain of an error and return `true` if any link is a
/// transient I/O or CKB RPC error (i.e. something likely to succeed on retry).
///
/// Because ckb-sdk wraps transport errors inside `anyhow::Error` (via
/// `CellCollectorError`, `TransactionDependencyError`, etc.) and these are not
/// always reachable through the standard `source()` chain, we additionally
/// inspect the error's `Display` output for patterns typical of transient
/// network/HTTP failures.
fn error_chain_has_transient(err: &(dyn std::error::Error + 'static)) -> bool {
    // First: walk the source chain looking for concrete transient types.
    let mut cur: &dyn std::error::Error = err;
    loop {
        if cur.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
        if cur.downcast_ref::<RpcError>().is_some() {
            return true;
        }
        match cur.source() {
            Some(next) => cur = next,
            None => break,
        }
    }

    // Second: fall back to checking the full error message for patterns that
    // indicate transient transport / HTTP / connection errors. These are
    // commonly produced by reqwest / hyper / io layers that get wrapped by
    // anyhow inside the ckb-sdk cell collector / dependency provider.
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("connection aborted")
        || msg.contains("broken pipe")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("temporarily unavailable")
        || msg.contains("network is unreachable")
        || msg.contains("eof")
        || msg.contains("http error")
}

impl FundingError {
    /// Returns `true` when the error is likely transient (network / RPC) and
    /// the operation may succeed if retried.
    ///
    /// For `CkbTxBuilderError` and `CkbTxUnlockError` the cause chain is
    /// inspected: the error is considered temporary only when a transient inner
    /// error (e.g. `std::io::Error` or `ckb_sdk::RpcError`) is found.
    pub fn is_temporary(&self) -> bool {
        use FundingError::*;
        match self {
            CkbRpcError(_) | RactorError(_) | IoError(_) | SerdeError(_) | FromUtf8Error(_) => true,
            CkbTxBuilderError(e) => error_chain_has_transient(e),
            CkbTxUnlockError(e) => error_chain_has_transient(e),
            _ => false,
        }
    }
}

#[derive(Error, Debug)]
pub enum CkbChainError {
    #[error("Funding error: {0}")]
    FundingError(#[from] FundingError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_sdk::traits::CellCollectorError;

    #[test]
    fn tx_builder_error_with_io_cause_is_temporary() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        let inner = CellCollectorError::Internal(io_err.into());
        let err = FundingError::CkbTxBuilderError(TxBuilderError::CellCollector(inner));
        assert!(err.is_temporary());
    }

    #[test]
    fn tx_builder_error_without_transient_cause_is_not_temporary() {
        let err = FundingError::CkbTxBuilderError(TxBuilderError::InvalidParameter(
            anyhow::anyhow!("capacity overflow"),
        ));
        assert!(!err.is_temporary());
    }

    #[test]
    fn tx_builder_error_transient_display_fallback_without_io_in_chain() {
        #[derive(Debug)]
        struct OpaqueSdkError;

        impl std::fmt::Display for OpaqueSdkError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "cell collector: connection reset by peer")
            }
        }

        impl std::error::Error for OpaqueSdkError {}

        let err = FundingError::CkbTxBuilderError(TxBuilderError::InvalidParameter(
            anyhow::Error::new(OpaqueSdkError),
        ));
        assert!(err.is_temporary());
    }

    #[test]
    fn unlock_error_with_io_cause_is_temporary() {
        let io_err =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let err = FundingError::CkbTxUnlockError(UnlockError::Other(io_err.into()));
        assert!(err.is_temporary());
    }

    #[test]
    fn unlock_error_without_transient_cause_is_not_temporary() {
        let err = FundingError::CkbTxUnlockError(UnlockError::SignContextTypeIncorrect);
        assert!(!err.is_temporary());
    }

    #[test]
    fn always_temporary_variants() {
        let io_err = FundingError::IoError(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken",
        ));
        assert!(io_err.is_temporary());

        let serde_err: Result<(), _> = serde_json::from_str::<()>("bad");
        let serde_err = FundingError::SerdeError(serde_err.unwrap_err());
        assert!(serde_err.is_temporary());
    }

    #[test]
    fn never_temporary_variants() {
        assert!(!FundingError::AbsentTx.is_temporary());
        assert!(!FundingError::DeadCell.is_temporary());
        assert!(!FundingError::OverflowError.is_temporary());
        assert!(!FundingError::InvalidPeerFundingTx.is_temporary());
    }
}
