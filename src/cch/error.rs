use std::time::SystemTimeError;

use ckb_types::packed::Script;
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use thiserror::Error;

use crate::{fiber::types::Hash256, invoice::SettleInvoiceError, store::SubscriptionError};

#[derive(Error, Debug)]
pub enum CchDbError {
    #[error("Inserting duplicated key: {0}")]
    Duplicated(Hash256),

    #[error("Key not found: {0}")]
    NotFound(Hash256),
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
    #[error("CKB invoice missing amount")]
    CKBInvoiceMissingAmount,
    #[error("SendBTC order already paid")]
    SendBTCOrderAlreadyPaid,
    #[error("SendBTC received payment amount is too small")]
    SendBTCReceivedAmountTooSmall,
    #[error("ReceiveBTC order payment amount is too small")]
    CchOrderAmountTooSmall,
    #[error("ReceiveBTC order payment amount is too large")]
    CchOrderAmountTooLarge,
    #[error("ReceiveBTC order already paid")]
    CchOrderAlreadyPaid,
    #[error("ReceiveBTC received payment amount is too small")]
    ReceiveBTCReceivedAmountTooSmall,
    #[error("Invalid UDT script in ReceiveBTC order: expecting {0:?}, got {1:?}")]
    ReceiveBTCInvalidUdtScript(Script, Option<Script>),
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
    #[error("Subscribe fiber updates error: {0}")]
    SubscribeFiberUpdatesError(#[from] SubscriptionError),
    #[error("Task canceled")]
    TaskCanceled,
    #[error("Sending fiber payment error: {0}")]
    SendFiberPaymentError(String),
    #[error("Settling fiber invoice error: {0}")]
    SettleFiberInvoiceError(#[from] SettleInvoiceError),
    #[error("Requesting lnd gRPC error: {0}")]
    LndGrpcRequestError(String),
    #[error("Unexpected lnd data: {0}")]
    UnexpectedLndData(String),
    #[error("Cch order not found: {0}")]
    OrderNotFound(Hash256),
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
