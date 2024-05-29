use crate::{ckb::types::Hash256, invoice::CkbInvoice};

pub trait InvoiceStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice>;
    fn insert_invoice(&self, invoice: CkbInvoice);
}
