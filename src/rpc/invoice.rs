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
pub(crate) struct NewInvoiceResult {
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

#[rpc(server)]
trait InvoiceRpc {
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
                Ok(_) => Ok(NewInvoiceResult {
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
}
