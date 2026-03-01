use crate::{cch::CchOrderStatus, fiber::types::Hash256, invoice::Currency, time::SystemTimeError};

use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use thiserror::Error;

#[derive(Error, Debug, Eq, PartialEq)]
pub enum CchStoreError {
    #[error("Inserting duplicated key: {0}")]
    Duplicated(Hash256),

    #[error("Key not found: {0}")]
    NotFound(Hash256),
}

#[derive(Error, Debug)]
pub enum CchError {
    #[error("Store error: {0}")]
    StoreError(#[from] CchStoreError),
    #[error("Outgoing invoice expiry time is too short")]
    OutgoingInvoiceExpiryTooShort,
    #[error("BTC invoice parse error: {0}")]
    BTCInvoiceParseError(lightning_invoice::ParseOrSemanticError),
    #[error("BTC invoice expired")]
    BTCInvoiceExpired,
    #[error("BTC invoice missing amount")]
    BTCInvoiceMissingAmount,
    #[error("BTC invoice final TLC expiry delta exceeds safe limit for cross-chain swap")]
    BTCInvoiceFinalTlcExpiryDeltaTooLarge,
    #[error("CKB invoice error: {0}")]
    CKBInvoiceError(#[from] crate::invoice::InvoiceError),
    #[error("CKB invoice expired")]
    CKBInvoiceExpired,
    #[error("CKB invoice missing amount")]
    CKBInvoiceMissingAmount,
    #[error("CKB invoice final TLC expiry delta exceeds safe limit for cross-chain swap")]
    CKBInvoiceFinalTlcExpiryDeltaTooLarge,
    #[error("CKB invoice hash algorithm is not SHA256, which is required for LND compatibility")]
    CKBInvoiceIncompatibleHashAlgorithm,
    #[error("BTC invoice network mismatch: expected {expected}, got {actual}")]
    BTCInvoiceNetworkMismatch { expected: String, actual: String },
    #[error("CKB invoice network mismatch: expected {expected}, got {actual}")]
    CKBInvoiceNetworkMismatch {
        expected: Currency,
        actual: Currency,
    },
    #[error("ReceiveBTC order payment amount is too small")]
    ReceiveBTCOrderAmountTooSmall,
    #[error("ReceiveBTC order payment amount is too large")]
    ReceiveBTCOrderAmountTooLarge,
    #[error("Wrapped BTC type script mismatch")]
    WrappedBTCTypescriptMismatch,
    #[error("Expect preimage in settled payment but missing")]
    SettledPaymentMissingPreimage,
    #[error("Preimage hash mismatch")]
    PreimageHashMismatch,
    #[error("Invalid transition from {0:?} to {1:?}")]
    InvalidTransition(CchOrderStatus, CchOrderStatus),
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

impl From<lightning_invoice::ParseOrSemanticError> for CchError {
    fn from(err: lightning_invoice::ParseOrSemanticError) -> Self {
        CchError::BTCInvoiceParseError(err)
    }
}

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
