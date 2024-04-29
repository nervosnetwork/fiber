use serde::Deserialize;
use serde_with::serde_as;

#[serde_as]
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

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct NewInvoiceParams {
    pub amount: u64,
    pub description: Option<String>,
    pub currency: String,
    pub payment_hash: Option<String>,
    pub payment_preimage: Option<String>,
    pub expiry: Option<u64>,
    pub fallback_address: Option<String>,
    pub final_cltv: Option<u64>,
    pub final_htlc_timeout: Option<u64>,
}
