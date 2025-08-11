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
}

/// Used for delegating the store trait
pub trait PreimageStoreDeref {
    type Target: PreimageStore;
    fn preimage_store_deref(&self) -> &Self::Target;
}

impl<T: PreimageStoreDeref> PreimageStore for T {
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256) {
        self.preimage_store_deref()
            .insert_preimage(payment_hash, preimage);
    }

    fn remove_preimage(&self, payment_hash: &Hash256) {
        self.preimage_store_deref().remove_preimage(payment_hash);
    }

    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        self.preimage_store_deref().get_preimage(payment_hash)
    }
}
