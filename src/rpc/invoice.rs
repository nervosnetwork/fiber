use std::time::Duration;

use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
use crate::fiber::types::Hash256;
use crate::invoice::{CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use ckb_jsonrpc_types::Script;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use secp256k1::PublicKey as Publickey;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tentacle::secio::PublicKey;

#[serde_as]
#[derive(Serialize, Deserialize)]
pub(crate) struct NewInvoiceParams {
    #[serde_as(as = "U128Hex")]
    amount: u128,
    description: Option<String>,
    currency: Currency,
    payment_preimage: Hash256,
    #[serde_as(as = "Option<U64Hex>")]
    expiry: Option<u64>,
    fallback_address: Option<String>,
    #[serde_as(as = "Option<U64Hex>")]
    final_cltv: Option<u64>,
    #[serde_as(as = "Option<U64Hex>")]
    final_htlc_timeout: Option<u64>,
    udt_type_script: Option<Script>,
    hash_algorithm: Option<HashAlgorithm>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InvoiceResult {
    invoice_address: String,
    invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ParseInvoiceParams {
    invoice: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ParseInvoiceResult {
    invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InvoiceParams {
    payment_hash: Hash256,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct GetInvoiceResult {
    invoice_address: String,
    invoice: CkbInvoice,
    status: CkbInvoiceStatus,
}

#[rpc(server)]
trait InvoiceRpc {
    #[method(name = "new_invoice")]
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned>;

    #[method(name = "parse_invoice")]
    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned>;

    #[method(name = "get_invoice")]
    async fn get_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    #[method(name = "cancel_invoice")]
    async fn cancel_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;
}

pub(crate) struct InvoiceRpcServerImpl<S> {
    store: S,
    public_key: Option<PublicKey>,
}

impl<S> InvoiceRpcServerImpl<S> {
    pub(crate) fn new(store: S, public_key: Option<PublicKey>) -> Self {
        Self { store, public_key }
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
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
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
        if let Some(hash_algorithm) = params.hash_algorithm {
            invoice_builder = invoice_builder.hash_algorithm(hash_algorithm);
        };

        if let Some(public_key) = &self.public_key {
            invoice_builder = invoice_builder.payee_pub_key(
                Publickey::from_slice(public_key.inner_ref()).expect("public key must be valid"),
            );
        }

        match invoice_builder.build() {
            Ok(invoice) => match self
                .store
                .insert_invoice(invoice.clone(), Some(params.payment_preimage))
            {
                Ok(_) => Ok(InvoiceResult {
                    invoice_address: invoice.to_string(),
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

    async fn get_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash;
        match self.store.get_invoice(&payment_hash) {
            Some(invoice) => {
                let status = match self
                    .store
                    .get_invoice_status(&payment_hash)
                    .expect("no invoice status found")
                {
                    CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
                    status => status,
                };

                Ok(GetInvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice,
                    status,
                })
            }
            None => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "invoice not found".to_string(),
                Some(payment_hash),
            )),
        }
    }

    async fn cancel_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash;
        match self.store.get_invoice(&payment_hash) {
            Some(invoice) => {
                let status = match self
                    .store
                    .get_invoice_status(&payment_hash)
                    .expect("no invoice status found")
                {
                    CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
                    status => status,
                };

                let new_status = match status {
                    CkbInvoiceStatus::Paid | CkbInvoiceStatus::Cancelled => {
                        return Err(ErrorObjectOwned::owned(
                            CALL_EXECUTION_FAILED_CODE,
                            format!("invoice can not be canceled, current status: {}", status),
                            Some(payment_hash),
                        ));
                    }
                    _ => CkbInvoiceStatus::Cancelled,
                };
                self.store
                    .update_invoice_status(&payment_hash, new_status)
                    .map_err(|e| {
                        ErrorObjectOwned::owned(
                            CALL_EXECUTION_FAILED_CODE,
                            e.to_string(),
                            Some(payment_hash),
                        )
                    })?;
                Ok(GetInvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice,
                    status: new_status,
                })
            }
            None => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "invoice not found".to_string(),
                Some(payment_hash),
            )),
        }
    }
}
