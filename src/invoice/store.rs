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
    // A payment to an invoice is made by sending a TLC over some channels
    // (possibly multiple when atomic multi-path payment support is out).
    // This function returns all the channels that were used to pay an invoice.
    fn get_invoice_channels(&self, id: &Hash256) -> Vec<Hash256>;
    // This function is used to add a channel to the list of channels that were
    // used to pay an invoice.
    fn add_invoice_channel(
        &self,
        id: &Hash256,
        channel: &Hash256,
    ) -> Result<Vec<Hash256>, InvoiceError>;
}
