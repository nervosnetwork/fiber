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
    pub btc_pay_req: String,
}
