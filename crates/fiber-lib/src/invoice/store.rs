use super::{CkbInvoiceStatus, InvoiceError};
use crate::{fiber::types::Hash256, invoice::CkbInvoice};

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

    /// Search for the stored preimage with the given payment hash prefix, should be the first 20 bytes of the payment hash.
    fn search_preimage(&self, payment_hash_prefix: &[u8]) -> Option<Hash256>;
}
