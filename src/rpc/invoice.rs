use crate::fiber::graph::{NetworkGraphStateStore, PaymentSessionStatus};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
use crate::fiber::types::{Hash256, Privkey};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder, InvoiceStore};
use crate::FiberConfig;
use ckb_jsonrpc_types::Script;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::time::Duration;
use tentacle::secio::SecioKeyPair;

#[serde_as]
#[derive(Serialize, Deserialize)]
pub(crate) struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    amount: u128,
    /// The description of the invoice.
    description: Option<String>,
    /// The currency of the invoice.
    currency: Currency,
    /// The payment preimage of the invoice.
    payment_preimage: Hash256,
    /// The expiry time of the invoice.
    #[serde_as(as = "Option<U64Hex>")]
    expiry: Option<u64>,
    /// The fallback address of the invoice.
    fallback_address: Option<String>,
    /// The final CLTV of the invoice.
    #[serde_as(as = "Option<U64Hex>")]
    final_cltv: Option<u64>,
    /// The final HTLC timeout of the invoice.
    #[serde_as(as = "Option<U64Hex>")]
    final_htlc_timeout: Option<u64>,
    /// The UDT type script of the invoice.
    udt_type_script: Option<Script>,
    /// The hash algorithm of the invoice.
    hash_algorithm: Option<HashAlgorithm>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InvoiceResult {
    /// The encoded invoice address.
    invoice_address: String,
    /// The invoice.
    invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ParseInvoiceParams {
    /// The encoded invoice address.
    invoice: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ParseInvoiceResult {
    /// The invoice.
    invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GetInvoiceParams {
    /// The payment hash of the invoice.
    payment_hash: Hash256,
}

#[derive(Clone, Serialize, Deserialize)]
enum InvoiceStatus {
    /// The invoice is unpaid.
    Unpaid,
    /// The invoice is in flight.
    Inflight,
    /// The invoice is paid, the payment is successful.
    Paid,
    /// The invoice is expired, can'b be used anymore.
    Expired,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct GetInvoiceResult {
    /// The encoded invoice address.
    invoice_address: String,
    /// The invoice.
    invoice: CkbInvoice,
    /// The invoice status.
    status: InvoiceStatus,
}

/// RPC module for invoice management.
#[rpc(server)]
trait InvoiceRpc {
    /// Generates a new invoice.
    #[method(name = "new_invoice")]
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned>;

    /// Parses a encoded invoice.
    #[method(name = "parse_invoice")]
    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned>;

    /// Retrieves an invoice.
    #[method(name = "get_invoice")]
    async fn get_invoice(
        &self,
        payment_hash: GetInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;
}

pub(crate) struct InvoiceRpcServerImpl<S> {
    store: S,
    keypair: Option<(PublicKey, SecretKey)>,
}

impl<S> InvoiceRpcServerImpl<S> {
    pub(crate) fn new(store: S, config: Option<FiberConfig>) -> Self {
        let keypair = config.map(|config| {
            let kp = config
                .read_or_generate_secret_key()
                .expect("read or generate secret key");
            let private_key: Privkey = <[u8; 32]>::try_from(kp.as_ref())
                .expect("valid length for key")
                .into();
            let secio_kp = SecioKeyPair::from(kp);
            let keypair = (
                PublicKey::from_slice(secio_kp.public_key().inner_ref()).expect("valid public key"),
                private_key.into(),
            );
            keypair
        });
        Self { store, keypair }
    }
}

#[async_trait]
impl<S> InvoiceRpcServer for InvoiceRpcServerImpl<S>
where
    S: InvoiceStore + NetworkGraphStateStore + Send + Sync + 'static,
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

        let invoice = if let Some((public_key, secret_key)) = &self.keypair {
            invoice_builder = invoice_builder.payee_pub_key(public_key.clone());
            invoice_builder
                .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret_key))
        } else {
            invoice_builder.build()
        };

        match invoice {
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
        params: GetInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash;
        match self.store.get_invoice(&payment_hash) {
            Some(invoice) => {
                let invoice_status = if invoice.is_expired() {
                    InvoiceStatus::Expired
                } else {
                    InvoiceStatus::Unpaid
                };
                let payment_session = self.store.get_payment_session(payment_hash);
                let status = match payment_session {
                    Some(session) => match session.status {
                        PaymentSessionStatus::Inflight => InvoiceStatus::Inflight,
                        PaymentSessionStatus::Success => InvoiceStatus::Paid,
                        _ => invoice_status,
                    },
                    None => invoice_status,
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
}
