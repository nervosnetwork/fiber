use thiserror::Error;

#[derive(Error, Debug)]
pub enum CchError {
    #[error("BTC invoice expired")]
    BTCInvoiceExpired,
    #[error("BTC invoice missing amount")]
    BTCInvoiceMissingAmount,
    #[error("CKB asset not allowed to exchange BTC")]
    CKBAssetNotAllowed,
}

pub type CchResult<T> = std::result::Result<T, CchError>;
