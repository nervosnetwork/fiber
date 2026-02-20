//! This module provides the RPC interface for creating, parsing, and retrieving invoices.
//!
//! We define CkbInvoice and its related types here only for the RPC interface.
//! For better separation of concerns, the actual invoice logic is implemented in the `invoice` module.
//!
use crate::fiber::config::{MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA};
use crate::fiber::features::FeatureVector;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::serde_utils::{duration_hex, U128Hex, U64Hex};
use crate::fiber::types::{Hash256, Privkey};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{
    Attribute as InternalAttribute, CkbInvoice as InternalCkbInvoice, CkbInvoiceStatus, CkbScript,
    Currency, InvoiceBuilder, InvoiceData as InternalInvoiceData, InvoiceSignature, InvoiceStore,
};
use crate::{gen_rand_sha256_hash, handle_actor_call, log_and_error, FiberConfig};

use ckb_jsonrpc_types::Script;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use ractor::{call, ActorRef};
use rand::Rng;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
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
    /// This attribute is deprecated since v0.6.0, The final tlc time out, in milliseconds
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
#[derive(Serialize, Deserialize, Default, Clone)]
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
    /// Whether allow payment to use trampoline routing
    pub allow_trampoline_routing: Option<bool>,
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

#[derive(Serialize, Deserialize, Debug)]
pub struct SettleInvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
    /// The payment preimage of the invoice.
    pub payment_preimage: Hash256,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SettleInvoiceResult {}

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

    /// Settles an invoice by saving the preimage to this invoice.
    #[method(name = "settle_invoice")]
    async fn settle_invoice(
        &self,
        settle_invoice: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned>;
}

pub struct InvoiceRpcServerImpl<S> {
    store: S,
    network_actor: Option<ActorRef<NetworkActorMessage>>,
    keypair: Option<(PublicKey, SecretKey)>,
    currency: Option<Currency>,
    node_features: Option<FeatureVector>,
}

impl<S> InvoiceRpcServerImpl<S> {
    pub fn new(
        store: S,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        config: Option<FiberConfig>,
    ) -> Self {
        let (keypair, currency, node_features) = if let Some(config) = config {
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

            (
                Some(keypair),
                Some(currency),
                Some(config.gen_node_features()),
            )
        } else {
            (None, None, None)
        };
        Self {
            store,
            network_actor,
            keypair,
            currency,
            node_features,
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

    /// Settles an invoice by saving the preimage to this invoice.
    async fn settle_invoice(
        &self,
        settle_invoice: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned> {
        self.settle_invoice(settle_invoice).await
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
        let error = |msg: &str| {
            Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                msg.to_string(),
                Some(params.clone()),
            ))
        };

        if let Some(currency) = self.currency {
            if currency != params.currency {
                return error(&format!(
                    "Currency must be {:?} with the chain network",
                    currency
                ));
            }
        }
        let mut invoice_builder = InvoiceBuilder::new(params.currency).amount(Some(params.amount));

        // If both preimage and hash are absent, a random preimage is generated.
        let preimage_opt = match (params.payment_preimage, params.payment_hash) {
            (Some(preimage), _) => Some(preimage),
            (None, None) => Some(gen_rand_sha256_hash()),
            _ => None,
        };

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
                if !self
                    .node_features
                    .as_ref()
                    .is_some_and(|f| f.supports_basic_mpp())
                {
                    return error("Node does not support MPP, please enable MPP feature");
                }
                let mut rng = rand::thread_rng();
                let payment_secret: [u8; 32] = rng.gen();
                invoice_builder = invoice_builder.payment_secret(payment_secret.into());
            }
        };
        if let Some(allow_trampoline_routing) = params.allow_trampoline_routing {
            invoice_builder = invoice_builder.allow_trampoline_routing(allow_trampoline_routing);
            if allow_trampoline_routing
                && !self
                    .node_features
                    .as_ref()
                    .is_some_and(|f| f.supports_trampoline_routing())
            {
                return error("Node does not support trampoline routing, please enable trampoline routing feature");
            }
        };

        let final_expiry_delta = params.final_expiry_delta.unwrap_or(MIN_TLC_EXPIRY_DELTA);
        if final_expiry_delta < MIN_TLC_EXPIRY_DELTA {
            return error(&format!(
                "final_expiry_delta must be greater than or equal to {}",
                MIN_TLC_EXPIRY_DELTA
            ));
        }
        if final_expiry_delta > MAX_PAYMENT_TLC_EXPIRY_LIMIT {
            return error(&format!(
                "final_expiry_delta must be less than or equal to {}",
                MAX_PAYMENT_TLC_EXPIRY_LIMIT
            ));
        }
        invoice_builder = invoice_builder.final_expiry_delta(final_expiry_delta);

        if let Some(udt_type_script) = &params.udt_type_script {
            invoice_builder = invoice_builder.udt_type_script(udt_type_script.clone().into());
        };
        if let Some(hash_algorithm) = params.hash_algorithm {
            invoice_builder = invoice_builder.hash_algorithm(hash_algorithm);
        };

        let invoice = if let Some((public_key, secret_key)) = &self.keypair {
            invoice_builder = invoice_builder.payee_pub_key(*public_key);
            invoice_builder
                .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, secret_key))
        } else {
            invoice_builder.build()
        };

        match invoice {
            Ok(invoice) => {
                if self.store.get_invoice(invoice.payment_hash()).is_some() {
                    return error("invoice already exists");
                }
                match self.store.insert_invoice(invoice.clone(), preimage_opt) {
                    Ok(_) => Ok(InvoiceResult {
                        invoice_address: invoice.to_string(),
                        invoice: invoice.into(),
                    }),
                    Err(e) => error(&e.to_string()),
                }
            }
            Err(e) => error(&e.to_string()),
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

    pub async fn settle_invoice(
        &self,
        params: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned> {
        let network_actor = self.network_actor.as_ref().ok_or(ErrorObjectOwned::owned(
            CALL_EXECUTION_FAILED_CODE,
            "network actor not initialized".to_string(),
            Option::<()>::None,
        ))?;

        let SettleInvoiceParams {
            payment_hash,
            payment_preimage,
        } = params;

        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                payment_hash,
                payment_preimage,
                rpc_reply,
            ))
        };

        handle_actor_call!(network_actor, message, params).map(|_| SettleInvoiceResult {})
    }
}
