use crate::invoice::CkbInvoice;
use std::collections::HashMap;

use super::InvoiceError;

// TODO: persist generated invoices
#[derive(Default)]
pub struct InvoicesDb {
    invoices: HashMap<String, CkbInvoice>,
}

impl InvoicesDb {
    pub fn insert_invoice(&mut self, invoice: CkbInvoice) -> Result<(), InvoiceError> {
        if self.invoices.contains_key(&invoice.payment_hash_id()) {
            return Err(InvoiceError::DuplicatedInvoice(invoice.payment_hash_id()));
        }
        self.invoices.insert(invoice.payment_hash_id(), invoice);
        Ok(())
    }
}
