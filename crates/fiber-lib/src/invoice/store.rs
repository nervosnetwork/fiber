use thiserror::Error;

use super::{CkbInvoiceStatus, InvoiceError};
use crate::invoice::CkbInvoice;
use fiber_types::Hash256;

pub trait InvoiceStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice>;
    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError>;
    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: CkbInvoiceStatus,
    ) -> Result<(), InvoiceError>;
    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus>;
}

pub trait PreimageStore {
    /// Insert a preimage into the store, the payment hash should be a 32 bytes hash result of the preimage after `HashAlgorithm` is applied.
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256);

    /// Remove a preimage from the store.
    fn remove_preimage(&self, payment_hash: &Hash256);

    /// Get a preimage from the store.
    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256>;
}

#[derive(Error, Debug)]
pub enum SettleInvoiceError {
    #[error("Invoice not found")]
    InvoiceNotFound,
    #[error("Hash mismatch")]
    HashMismatch,
    #[error("Invoice is still open")]
    InvoiceStillOpen,
    #[error("Invoice is already cancelled")]
    InvoiceAlreadyCancelled,
    #[error("Invoice is already expired")]
    InvoiceAlreadyExpired,
    #[error("Invoice is already paid")]
    InvoiceAlreadyPaid,
    #[error("Internal error: {0}")]
    InternalError(String),
}
