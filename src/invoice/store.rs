use crate::{fiber::types::Hash256, invoice::CkbInvoice};

use super::InvoiceError;

pub trait InvoiceStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice>;
    fn insert_invoice(&self, invoice: CkbInvoice) -> Result<(), InvoiceError>;
}
