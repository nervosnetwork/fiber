use thiserror::Error;

#[derive(Error, Debug)]
pub enum CchDbError {
    #[error("Duplicated SendBTCOrder with the same payment hash: {0}")]
    DuplicatedSendBTCOrder(String),
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
}

pub type CchResult<T> = std::result::Result<T, CchError>;
