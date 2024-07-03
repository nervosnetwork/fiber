use std::time::SystemTimeError;

use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CchDbError {
    #[error("Inserting duplicated key: {0}")]
    Duplicated(String),

    #[error("Key not found: {0}")]
    NotFound(String),
}

#[derive(Error, Debug)]
pub enum CchError {
    #[error("Database error: {0}")]
    DbError(#[from] CchDbError),
    #[error("BTC invoice parse error: {0}")]
    BTCInvoiceParseError(#[from] lightning_invoice::ParseOrSemanticError),
    #[error("BTC invoice expired")]
    BTCInvoiceExpired,
    #[error("BTC invoice missing amount")]
    BTCInvoiceMissingAmount,
    #[error("CKB invoice error: {0}")]
    CKBInvoiceError(#[from] crate::invoice::InvoiceError),
    #[error("SendBTC order already paid")]
    SendBTCOrderAlreadyPaid,
    #[error("SendBTC received payment amount is too small")]
    SendBTCReceivedAmountTooSmall,
    #[error("ReceiveBTC order payment amount is too small")]
    ReceiveBTCOrderAmountTooSmall,
    #[error("ReceiveBTC order payment amount is too large")]
    ReceiveBTCOrderAmountTooLarge,
    #[error("ReceiveBTC order already paid")]
    ReceiveBTCOrderAlreadyPaid,
    #[error("ReceiveBTC received payment amount is too small")]
    ReceiveBTCReceivedAmountTooSmall,
    #[error("ReceiveBTC expected preimage but missing")]
    ReceiveBTCMissingPreimage,
    #[error("System time error: {0}")]
    SystemTimeError(#[from] SystemTimeError),
    #[error("JSON serialization error: {0}")]
    JSONSerializationError(#[from] serde_json::Error),
    #[error("Hex decoding error")]
    HexDecodingError,
    #[error("Lnd channel error: {0}")]
    LndChannelError(#[from] lnd_grpc_tonic_client::channel::Error),
    #[error("Lnd RPC error: {0}")]
    LndRpcError(String),
}

pub type CchResult<T> = std::result::Result<T, CchError>;

impl Into<ErrorObjectOwned> for CchError {
    fn into(self) -> ErrorObjectOwned {
        // TODO: categorize error codes
        ErrorObjectOwned::owned(
            CALL_EXECUTION_FAILED_CODE,
            self.to_string(),
            Option::<()>::None,
        )
    }
}
