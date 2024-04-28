use crate::invoice::CkbInvoice;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum InvoiceDbError {
    #[error("Duplicated Invoice with the same payment hash: {0}")]
    DuplicatedInvoice(String),
}

// TODO: persist generated invoices
#[derive(Default)]
pub struct InvoicesDb {
    invoices: HashMap<String, CkbInvoice>,
}

impl InvoicesDb {
    pub async fn insert_invoice(&mut self, invoice: CkbInvoice) -> Result<(), InvoiceDbError> {
        if self.invoices.contains_key(&invoice.payment_hash_id()) {
            return Err(InvoiceDbError::DuplicatedInvoice(invoice.payment_hash_id()));
        }
        self.invoices.insert(invoice.payment_hash_id(), invoice);
        Ok(())
    }
}
