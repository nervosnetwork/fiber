use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    fn get_invoice_channel_info(&self, payment_hash: &Hash256) -> Vec<InvoiceChannelInfo>;
    // This function is used to add a channel (with the amount paid through this channel)
    // to the list of channels that were used to pay an invoice.
    fn add_invoice_channel_info(
        &self,
        payment_hash: &Hash256,
        invoice_channel_info: InvoiceChannelInfo,
    ) -> Result<Vec<InvoiceChannelInfo>, InvoiceError>;
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct InvoiceChannelInfo {
    pub channel_id: Hash256,
    pub amount: u128,
}

impl InvoiceChannelInfo {
    pub fn new(channel_id: Hash256, amount: u128) -> Self {
        Self { channel_id, amount }
    }
}

#[derive(Error, Debug)]
pub enum SettleInvoiceError {
    #[error("Invoice not found")]
    InvoiceNotFound,
    #[error("Hash mismatch")]
    HashMismatch,
    #[error("Internal error: {0}")]
    InternalError(String),
}

pub(crate) fn add_invoice<S: InvoiceStore>(
    store: &S,
    invoice: CkbInvoice,
    preimage: Option<Hash256>,
) -> Result<(), InvoiceError> {
    let hash = invoice.payment_hash();
    if store.get_invoice(hash).is_some() {
        return Err(InvoiceError::InvoiceAlreadyExists);
    }
    store.insert_invoice(invoice, preimage)
}
