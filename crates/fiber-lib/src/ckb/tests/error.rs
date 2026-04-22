use crate::ckb::error::FundingError;
use crate::ckb::funding::map_tx_builder_error;
use ckb_sdk::{traits::CellCollectorError, tx_builder::TxBuilderError, unlock::UnlockError};

#[test]
fn tx_builder_error_with_io_cause_is_temporary() {
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
    let inner = CellCollectorError::Internal(io_err.into());
    let err = FundingError::CkbTxBuilderError(TxBuilderError::CellCollector(inner));
    assert!(err.is_temporary());
}

#[test]
fn tx_builder_error_without_transient_cause_is_not_temporary() {
    let err = FundingError::CkbTxBuilderError(TxBuilderError::InvalidParameter(anyhow::anyhow!(
        "capacity overflow"
    )));
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
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
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
    assert!(!FundingError::DeadCell.is_temporary());
    assert!(!FundingError::OverflowError.is_temporary());
    assert!(!FundingError::InvalidPeerFundingTx.is_temporary());
}

#[test]
fn absent_tx_is_temporary() {
    assert!(FundingError::AbsentTx.is_temporary());
}

#[test]
fn insufficient_cells_is_not_temporary() {
    let err = FundingError::InsufficientCells(
        "can not find enough UDT owner cells for funding transaction".to_string(),
    );
    assert!(!err.is_temporary());
}

#[test]
fn insufficient_cells_display_includes_detail() {
    let detail = "can not find enough UDT owner cells for funding transaction";
    let err = FundingError::InsufficientCells(detail.to_string());
    let msg = err.to_string();
    assert!(
        msg.contains(detail),
        "expected display to contain detail, got: {msg}"
    );
}

#[test]
fn map_tx_builder_error_converts_insufficient_udt_cells() {
    let inner = TxBuilderError::Other(anyhow::anyhow!(
        "can not find enough UDT owner cells for funding transaction"
    ));
    let err = map_tx_builder_error(inner);
    assert!(
        matches!(err, FundingError::InsufficientCells(_)),
        "expected InsufficientCells, got: {err:?}"
    );
    assert!(!err.is_temporary());
}

#[test]
fn map_tx_builder_error_preserves_other_errors() {
    let inner = TxBuilderError::InvalidParameter(anyhow::anyhow!("bad param"));
    let err = map_tx_builder_error(inner);
    assert!(
        matches!(err, FundingError::CkbTxBuilderError(_)),
        "expected CkbTxBuilderError, got: {err:?}"
    );
}

#[test]
fn map_tx_builder_error_preserves_transient_error() {
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
    let inner = TxBuilderError::CellCollector(CellCollectorError::Internal(io_err.into()));
    let err = map_tx_builder_error(inner);
    assert!(
        matches!(err, FundingError::CkbTxBuilderError(_)),
        "expected CkbTxBuilderError, got: {err:?}"
    );
    assert!(err.is_temporary());
}

#[test]
fn map_tx_builder_error_matches_substring_in_wrapped_message() {
    let inner = TxBuilderError::Other(anyhow::anyhow!(
        "tx build failed: can not find enough UDT owner cells for funding transaction (checked 0 cells)"
    ));
    let err = map_tx_builder_error(inner);
    assert!(
        matches!(err, FundingError::InsufficientCells(_)),
        "expected InsufficientCells for message containing the sentinel, got: {err:?}"
    );
}
