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
    /// Ensure the invoice has this preimage without replacing conflicting persisted data.
    fn ensure_invoice_preimage(
        &self,
        payment_hash: Hash256,
        preimage: Hash256,
    ) -> Result<CkbInvoiceStatus, EnsureInvoicePreimageError>;
}

pub trait PreimageStore {
    /// Insert a preimage into the store, the payment hash should be a 32 bytes hash result of the preimage after `HashAlgorithm` is applied.
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256);

    /// Remove a preimage from the store.
    fn remove_preimage(&self, payment_hash: &Hash256);

    /// Get a preimage from the store.
    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256>;
}

/// Error returned when recovering the separately persisted preimage for an invoice.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum EnsureInvoicePreimageError {
    /// The invoice or its status does not exist.
    #[error("Invoice not found")]
    InvoiceNotFound,
    /// The supplied preimage does not produce the invoice payment hash.
    #[error("Invoice preimage hash mismatch")]
    HashMismatch,
    /// A different preimage is already persisted for the invoice.
    #[error("Invoice has a conflicting stored preimage")]
    ConflictingPreimage,
    /// The invoice has reached a terminal status that cannot be repaired.
    #[error("Invoice status does not allow preimage recovery: {0}")]
    InvoiceNotUsable(CkbInvoiceStatus),
    /// A paid invoice has no persisted preimage and must not be modified retroactively.
    #[error("Paid invoice is missing its stored preimage")]
    PaidInvoiceMissingPreimage,
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

#[derive(Error, Debug, PartialEq, Eq)]
pub enum CancelInvoiceError {
    #[error("invoice not found")]
    InvoiceNotFound,
    #[error("invoice can not be canceled, current status: Cancelled")]
    InvoiceAlreadyCancelled,
    #[error("invoice can not be canceled, current status: Paid")]
    InvoiceAlreadyPaid,
    #[error("invoice can not be canceled because payment preimage already exists")]
    PaymentPreimageAlreadyExists,
    #[error("{0}")]
    InternalError(String),
}
