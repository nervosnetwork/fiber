use crate::time::SystemTimeError;

use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CchError {
    #[error("Database error: {0}")]
    DbError(#[from] super::order::CchDbError),
    #[error("BTC invoice parse error: {0}")]
    BTCInvoiceParseError(#[from] lightning_invoice::ParseOrSemanticError),
    #[error("BTC invoice expired")]
    BTCInvoiceExpired,
    #[error("BTC invoice missing amount")]
    BTCInvoiceMissingAmount,
    #[error("CKB invoice error: {0}")]
    CKBInvoiceError(#[from] crate::invoice::InvoiceError),
    #[error("CKB invoice missing amount")]
    CKBInvoiceMissingAmount,
    #[error("ReceiveBTC order payment amount is too small")]
    ReceiveBTCOrderAmountTooSmall,
    #[error("ReceiveBTC order payment amount is too large")]
    ReceiveBTCOrderAmountTooLarge,
    #[error("Expect preimage in settled payment but missing")]
    SettledPaymentMissingPreimage,
    #[error("System time error: {0}")]
    SystemTimeError(#[from] SystemTimeError),
    #[error("JSON serialization error: {0}")]
    JSONSerializationError(#[from] serde_json::Error),
    #[error("Hex decoding error from string: {0}")]
    HexDecodingError(String),
    #[error("Lnd channel error: {0}")]
    LndChannelError(#[from] lnd_grpc_tonic_client::channel::Error),
    #[error("Lnd RPC error: {0}")]
    LndRpcError(String),
}

pub type CchResult<T> = std::result::Result<T, CchError>;

impl From<CchError> for ErrorObjectOwned {
    fn from(val: CchError) -> Self {
        // TODO: categorize error codes
        ErrorObjectOwned::owned(
            CALL_EXECUTION_FAILED_CODE,
            val.to_string(),
            Option::<()>::None,
        )
    }
}
