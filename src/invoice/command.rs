use super::invoice_impl::Currency;
use crate::fiber::types::Hash256;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub enum InvoiceCommand {
    NewInvoice(NewInvoiceParams),
    ParseInvoice(String),
}

impl InvoiceCommand {
    pub fn name(&self) -> &'static str {
        match self {
            InvoiceCommand::NewInvoice(_) => "NewInvoice",
            InvoiceCommand::ParseInvoice(_) => "ParseInvoice",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewInvoiceParams {
    pub amount: u128,
    pub description: Option<String>,
    pub currency: Currency,
    pub payment_hash: Option<Hash256>,
    pub payment_preimage: Option<Hash256>,
    pub expiry: Option<u64>,
    pub fallback_address: Option<String>,
    pub final_cltv: Option<u64>,
}
