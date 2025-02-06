use super::{CkbInvoiceStatus, InvoiceError};
use crate::{fiber::types::Hash256, invoice::CkbInvoice};

pub trait InvoiceStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice>;
    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError>;
    fn get_invoice_preimage(&self, id: &Hash256) -> Option<Hash256>;
    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: CkbInvoiceStatus,
    ) -> Result<(), InvoiceError>;
    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus>;
    fn insert_payment_preimage(
        &self,
        payment_hash: Hash256,
        preimage: Hash256,
    ) -> Result<(), InvoiceError>;
    fn remove_payment_preimage(&self, payment_hash: &Hash256) -> Result<(), InvoiceError>;
}
