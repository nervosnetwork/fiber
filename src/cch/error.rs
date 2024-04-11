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
    #[error("BTC invoice expired")]
    BTCInvoiceExpired,
    #[error("BTC invoice missing amount")]
    BTCInvoiceMissingAmount,
    #[error("CKB asset not allowed to exchange BTC")]
    CKBAssetNotAllowed,
    #[error("SendBTC order already paid")]
    SendBTCOrderAlreadyPaid,
}

pub type CchResult<T> = std::result::Result<T, CchError>;
