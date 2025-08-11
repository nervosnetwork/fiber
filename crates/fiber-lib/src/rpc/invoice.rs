//! This module provides the RPC interface for creating, parsing, and retrieving invoices.
//!
//! We define CkbInvoice and its related types here only for the RPC interface.
//! For better separation of concerns, the actual invoice logic is implemented in the `invoice` module.
//!
use crate::fiber::config::{MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::serde_utils::{duration_hex, U128Hex, U64Hex};
use crate::fiber::types::{Hash256, Privkey};
use crate::invoice::{
    Attribute as InternalAttribute, CkbInvoice as InternalCkbInvoice, CkbInvoiceStatus, CkbScript,
    Currency, InvoiceBuilder, InvoiceData as InternalInvoiceData, InvoiceSignature, InvoiceStore,
};

use crate::{gen_rand_sha256_hash, FiberConfig};
use ckb_jsonrpc_types::Script;
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};

#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use rand::Rng;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::time::Duration;
use tentacle::secio::SecioKeyPair;

/// The attributes of the invoice
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribute {
    #[serde(with = "U64Hex")]
    /// The final tlc time out, in milliseconds
    FinalHtlcTimeout(u64),
    #[serde(with = "U64Hex")]
    /// The final tlc minimum expiry delta, in milliseconds, default is 1 day
    FinalHtlcMinimumExpiryDelta(u64),
    #[serde(with = "duration_hex")]
    /// The expiry time of the invoice, in seconds
    ExpiryTime(Duration),
    /// The description of the invoice
    Description(String),
    /// The fallback address of the invoice
    FallbackAddr(String),
    /// The udt type script of the invoice
    UdtScript(CkbScript),
    /// The payee public key of the invoice
    PayeePublicKey(PublicKey),
    /// The hash algorithm of the invoice
    HashAlgorithm(HashAlgorithm),
    /// The feature flags of the invoice
    Feature(Vec<String>),
    /// The payment secret of the invoice
    PaymentSecret(Hash256),
}

/// The metadata of the invoice
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvoiceData {
    /// The timestamp of the invoice
    #[serde_as(as = "U128Hex")]
    pub timestamp: u128,
    /// The payment hash of the invoice
    pub payment_hash: Hash256,
    /// The attributes of the invoice, e.g. description, expiry time, etc.
    pub attrs: Vec<Attribute>,
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using [`CkbInvoiceBuilder`]
///  2. using `str::parse::<CkbInvoice>(&str)` (see [`CkbInvoice::from_str`])
///
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbInvoice {
    /// The currency of the invoice
    pub currency: Currency,
    #[serde_as(as = "Option<U128Hex>")]
    /// The amount of the invoice
    pub amount: Option<u128>,
    /// The signature of the invoice
    pub signature: Option<InvoiceSignature>,
    /// The invoice data, including the payment hash, timestamp and other attributes
    pub data: InvoiceData,
}

impl From<InternalAttribute> for Attribute {
    fn from(attr: InternalAttribute) -> Self {
        match attr {
            InternalAttribute::FinalHtlcTimeout(timeout) => Attribute::FinalHtlcTimeout(timeout),
            InternalAttribute::FinalHtlcMinimumExpiryDelta(delta) => {
                Attribute::FinalHtlcMinimumExpiryDelta(delta)
            }
            InternalAttribute::ExpiryTime(duration) => Attribute::ExpiryTime(duration),
            InternalAttribute::Description(desc) => Attribute::Description(desc),
            InternalAttribute::FallbackAddr(addr) => Attribute::FallbackAddr(addr),
            InternalAttribute::UdtScript(script) => Attribute::UdtScript(script),
            InternalAttribute::PayeePublicKey(pubkey) => Attribute::PayeePublicKey(pubkey),
            InternalAttribute::HashAlgorithm(alg) => Attribute::HashAlgorithm(alg),
            InternalAttribute::Feature(feature) => {
                Attribute::Feature(feature.enabled_features_names())
            }
            InternalAttribute::PaymentSecret(secret) => Attribute::PaymentSecret(secret),
        }
    }
}

impl From<InternalInvoiceData> for InvoiceData {
    fn from(data: InternalInvoiceData) -> Self {
        InvoiceData {
            timestamp: data.timestamp,
            payment_hash: data.payment_hash,
            attrs: data.attrs.into_iter().map(|a| a.into()).collect(),
        }
    }
}

impl From<InternalCkbInvoice> for CkbInvoice {
    fn from(inv: InternalCkbInvoice) -> Self {
        CkbInvoice {
            currency: inv.currency,
            amount: inv.amount,
            signature: inv.signature,
            data: inv.data.into(),
        }
    }
}

/// The parameter struct for generating a new invoice.
#[serde_as]
#[derive(Serialize, Deserialize, Default)]
pub struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The description of the invoice.
    pub description: Option<String>,
    /// The currency of the invoice.
    pub currency: Currency,
    /// The preimage to settle an incoming TLC payable to this invoice. If preimage is set, hash must be absent. If both preimage and hash are absent, a random preimage is generated.
    pub payment_preimage: Option<Hash256>,
    /// The hash of the preimage. If hash is set, preimage must be absent. This condition indicates a 'hold invoice' for which the tlc must be accepted and held until the preimage becomes known.
    pub payment_hash: Option<Hash256>,
    /// The expiry time of the invoice, in seconds.
    #[serde_as(as = "Option<U64Hex>")]
    pub expiry: Option<u64>,
    /// The fallback address of the invoice.
    pub fallback_address: Option<String>,
    /// The final HTLC timeout of the invoice, in milliseconds.
    /// Minimal value is 16 hours, and maximal value is 14 days.
    #[serde_as(as = "Option<U64Hex>")]
    pub final_expiry_delta: Option<u64>,
    /// The UDT type script of the invoice.
    pub udt_type_script: Option<Script>,
    /// The hash algorithm of the invoice.
    pub hash_algorithm: Option<HashAlgorithm>,
    /// Whether allow payment to use MPP
    pub allow_mpp: Option<bool>,
    /// Whether use atomic mpp, if use atomic mpp there will be no preimage generated.
    pub atomic_mpp: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct InvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize)]
pub struct ParseInvoiceParams {
    /// The encoded invoice address.
    pub invoice: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ParseInvoiceResult {
    /// The invoice.
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
}

/// The status of the invoice.
#[derive(Clone, Serialize, Deserialize)]
pub struct GetInvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
    /// The invoice status
    pub status: CkbInvoiceStatus,
}

/// RPC module for invoice management.
#[cfg(not(target_arch = "wasm32"))]
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
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    /// Cancels an invoice, only when invoice is in status `Open` can be canceled.
    #[method(name = "cancel_invoice")]
    async fn cancel_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;
}

pub struct InvoiceRpcServerImpl<S> {
    store: S,
    keypair: Option<(PublicKey, SecretKey)>,
    currency: Option<Currency>,
}

impl<S> InvoiceRpcServerImpl<S> {
    pub fn new(store: S, config: Option<FiberConfig>) -> Self {
        let config = config.map(|config| {
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

            // restrict currency to be the same as network
            let currency = match config.chain.as_str() {
                "mainnet" => Currency::Fibb,
                "testnet" => Currency::Fibt,
                _ => Currency::Fibd,
            };

            (keypair, currency)
        });
        Self {
            store,
            keypair: config.as_ref().map(|(kp, _)| *kp),
            currency: config.as_ref().map(|(_, currency)| *currency),
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> InvoiceRpcServer for InvoiceRpcServerImpl<S>
where
    S: InvoiceStore + Send + Sync + 'static,
{
    /// Generates a new invoice.
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
        self.new_invoice(params).await
    }

    /// Parses a encoded invoice.
    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned> {
        self.parse_invoice(params).await
    }

    /// Retrieves an invoice.
    async fn get_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        self.get_invoice(payment_hash).await
    }

    /// Cancels an invoice, only when invoice is in status `Open` can be canceled.
    async fn cancel_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        self.cancel_invoice(payment_hash).await
    }
}

impl<S> InvoiceRpcServerImpl<S>
where
    S: InvoiceStore + Send + Sync + 'static,
{
    pub async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
        if let Some(currency) = self.currency {
            if currency != params.currency {
                return Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    format!("Currency must be {:?} with the chain network", currency),
                    Some(params),
                ));
            }
        }

        let mut invoice_builder = InvoiceBuilder::new(params.currency).amount(Some(params.amount));

        // If both preimage and payment hash are absent, a random preimage is generated.
        let need_gen_preimage =
            params.payment_hash.is_none() && params.atomic_mpp.is_none_or(|atomic| !atomic);
        let preimage_opt = params
            .payment_preimage
            .or_else(|| need_gen_preimage.then(gen_rand_sha256_hash));

        if let Some(preimage) = preimage_opt {
            invoice_builder = invoice_builder.payment_preimage(preimage);
        }
        if let Some(hash) = params.payment_hash {
            invoice_builder = invoice_builder.payment_hash(hash);
        }

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

        if let Some(allow_mpp) = params.allow_mpp {
            invoice_builder = invoice_builder.allow_mpp(allow_mpp);
            if allow_mpp {
                let mut rng = rand::thread_rng();
                let payment_secret: [u8; 32] = rng.gen();
                invoice_builder = invoice_builder.payment_secret(payment_secret.into());
            }
        };

        if let Some(atomic_mpp) = params.atomic_mpp {
            invoice_builder = invoice_builder.allow_atomic_mpp(atomic_mpp);
        }

        if let Some(final_expiry_delta) = params.final_expiry_delta {
            if final_expiry_delta < MIN_TLC_EXPIRY_DELTA {
                return Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    format!(
                        "final_expiry_delta must be greater than or equal to {}",
                        MIN_TLC_EXPIRY_DELTA
                    ),
                    Some(params),
                ));
            }
            if final_expiry_delta > MAX_PAYMENT_TLC_EXPIRY_LIMIT {
                return Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    format!(
                        "final_expiry_delta must be less than or equal to {}",
                        MAX_PAYMENT_TLC_EXPIRY_LIMIT
                    ),
                    Some(params),
                ));
            }
            invoice_builder = invoice_builder.final_expiry_delta(final_expiry_delta);
        };
        if let Some(udt_type_script) = &params.udt_type_script {
            invoice_builder = invoice_builder.udt_type_script(udt_type_script.clone().into());
        };
        if let Some(hash_algorithm) = params.hash_algorithm {
            invoice_builder = invoice_builder.hash_algorithm(hash_algorithm);
        };

        let invoice = if let Some((public_key, secret_key)) = &self.keypair {
            invoice_builder = invoice_builder.payee_pub_key(*public_key);
            invoice_builder
                .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, secret_key))
        } else {
            invoice_builder.build()
        };

        match invoice {
            Ok(invoice) => match self.store.insert_invoice(invoice.clone(), preimage_opt) {
                Ok(_) => Ok(InvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice: invoice.into(),
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

    pub async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned> {
        let result: Result<InternalCkbInvoice, _> = params.invoice.parse();
        match result {
            Ok(invoice) => Ok(ParseInvoiceResult {
                invoice: invoice.into(),
            }),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    pub async fn get_invoice(
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
                    invoice: invoice.into(),
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

    pub async fn cancel_invoice(
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
                    invoice: invoice.into(),
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
