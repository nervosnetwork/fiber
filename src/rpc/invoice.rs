use std::time::Duration;

use crate::ckb::serde_utils::{U128Hex, U64Hex};
use crate::ckb::types::Hash256;
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder, InvoiceStore};
use ckb_jsonrpc_types::Script;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct NewInvoiceParams {
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    pub description: Option<String>,
    pub currency: Currency,
    pub payment_preimage: Hash256,
    #[serde_as(as = "Option<U64Hex>")]
    pub expiry: Option<u64>,
    pub fallback_address: Option<String>,
    #[serde_as(as = "Option<U64Hex>")]
    pub final_cltv: Option<u64>,
    #[serde_as(as = "Option<U64Hex>")]
    pub final_htlc_timeout: Option<u64>,
    pub udt_type_script: Option<Script>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NewInvoiceResult {
    pub invoice_string: String,
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize)]
pub struct ParseInvoiceParams {
    pub invoice: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ParseInvoiceResult {
    pub invoice: CkbInvoice,
}

#[rpc(server)]
pub trait InvoiceRpc {
    #[method(name = "new_invoice")]
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<NewInvoiceResult, ErrorObjectOwned>;

    #[method(name = "parse_invoice")]
    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned>;
}

pub struct InvoiceRpcServerImpl<S> {
    pub store: S,
}

impl<S> InvoiceRpcServerImpl<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

#[async_trait]
impl<S> InvoiceRpcServer for InvoiceRpcServerImpl<S>
where
    S: InvoiceStore + Send + Sync + 'static,
{
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<NewInvoiceResult, ErrorObjectOwned> {
        let mut invoice_builder = InvoiceBuilder::new(params.currency)
            .amount(Some(params.amount))
            .payment_preimage(params.payment_preimage);
        if let Some(description) = params.description.clone() {
            invoice_builder = invoice_builder.description(description);
        };
        if let Some(expiry) = params.expiry {
            let duration: Duration = Duration::from_secs(expiry);
            invoice_builder = invoice_builder.expiry_time(duration);
        };
        if let Some(fallback_address) = params.fallback_address.clone() {
            invoice_builder = invoice_builder.fallback_address(fallback_address);
        };
        if let Some(final_cltv) = params.final_cltv {
            invoice_builder = invoice_builder.final_cltv(final_cltv);
        };
        if let Some(udt_type_script) = &params.udt_type_script {
            invoice_builder = invoice_builder.udt_type_script(udt_type_script.clone().into());
        };

        match invoice_builder.build() {
            Ok(invoice) => match self.store.insert_invoice(invoice.clone()) {
                Ok(_) => Ok(NewInvoiceResult {
                    invoice_string: invoice.to_string(),
                    invoice,
                }),
                Err(e) => {
                    return Err(ErrorObjectOwned::owned(
                        CALL_EXECUTION_FAILED_CODE,
                        e.to_string(),
                        Some(params),
                    ))
                }
            },
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned> {
        let result: Result<CkbInvoice, _> = params.invoice.parse();
        match result {
            Ok(invoice) => Ok(ParseInvoiceResult { invoice }),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }
}
