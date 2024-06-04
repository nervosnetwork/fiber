use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use tokio::sync::mpsc::{channel, Sender};
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use serde::{Deserialize, Serialize};
use crate::ckb::types::Hash256;
use crate::invoice::{Currency, InvoiceCommand};
use super::InvoiceCommandWithReply;

#[derive(Serialize, Deserialize)]
pub struct NewInvoiceParams {
    pub amount: u128,
    pub description: Option<String>,
    pub currency: Currency,
    pub payment_preimage: Hash256,
    pub expiry: Option<u64>,
    pub fallback_address: Option<String>,
    pub final_cltv: Option<u64>,
    pub final_htlc_timeout: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct ParseInvoiceParams {
    pub invoice: String,
}

#[rpc(server)]
pub trait InvoiceRpc {
    #[method(name = "new_invoice")]
    async fn new_invoice(&self, params: NewInvoiceParams) -> Result<String, ErrorObjectOwned>;

    #[method(name = "parse_invoice")]
    async fn parse_invoice(&self, params: ParseInvoiceParams) -> Result<String, ErrorObjectOwned>;
}

pub struct InvoiceRpcServerImpl {
    pub invoice_command_sender: Sender<InvoiceCommandWithReply>,
}

impl InvoiceRpcServerImpl {
    pub fn new(invoice_command_sender: Sender<InvoiceCommandWithReply>) -> Self {
        InvoiceRpcServerImpl {
            invoice_command_sender,
        }
    }
}

#[async_trait]
impl InvoiceRpcServer for InvoiceRpcServerImpl {
    async fn new_invoice(&self, params: NewInvoiceParams) -> Result<String, ErrorObjectOwned> {
        let command = InvoiceCommand::NewInvoice(crate::invoice::NewInvoiceParams {
            amount: params.amount,
            description: params.description.clone(),
            currency: params.currency,
            payment_hash: None,
            payment_preimage: Some(params.payment_preimage),
            expiry: params.expiry,
            fallback_address: params.fallback_address.clone(),
            final_cltv: params.final_cltv,
            final_htlc_timeout: params.final_htlc_timeout,
        });

        let (sender, mut receiver) = channel(1);
        let _ = self.invoice_command_sender.send((command, sender)).await;
        let result = receiver.recv().await.expect("channel should not be closed");
        match result {
            Ok(data) => Ok(data),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    async fn parse_invoice(&self, params: ParseInvoiceParams) -> Result<String, ErrorObjectOwned> {
        let command = InvoiceCommand::ParseInvoice(params.invoice.clone());
        let (sender, mut receiver) = channel(1);
        let _ = self.invoice_command_sender.send((command, sender)).await;
        let result = receiver.recv().await.expect("channel should not be closed");
        match result {
            Ok(data) => Ok(data),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }
}
