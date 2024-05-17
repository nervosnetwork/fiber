use crate::{ckb::types::Hash256, invoice::CkbInvoice};
use std::collections::HashMap;

use super::InvoiceError;

// TODO: persist generated invoices
#[derive(Default)]
pub struct InvoicesDb {
    invoices: HashMap<Hash256, CkbInvoice>,
}

impl InvoicesDb {
    pub fn insert_invoice(&mut self, invoice: CkbInvoice) -> Result<(), InvoiceError> {
        if self.invoices.contains_key(&invoice.payment_hash()) {
            return Err(InvoiceError::DuplicatedInvoice(
                invoice.payment_hash().to_string(),
            ));
        }
        self.invoices.insert(*invoice.payment_hash(), invoice);
        Ok(())
    }
}
