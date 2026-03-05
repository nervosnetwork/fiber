//! This module provides the RPC interface for creating, parsing, and retrieving invoices.
//!
//! We define CkbInvoice and its related types here only for the RPC interface.
//! For better separation of concerns, the actual invoice logic is implemented in the `invoice` module.
//!
use crate::fiber::config::{MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{
    CkbInvoice as InternalCkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore,
};
use crate::rpc::utils::{rpc_error, rpc_error_no_data, RpcResultExt};
use crate::{gen_rand_sha256_hash, handle_actor_call, log_and_error, FiberConfig};
use fiber_json_types::{CkbInvoice as JsonCkbInvoice, CkbInvoiceStatus as JsonCkbInvoiceStatus};
use fiber_types::{FeatureVector, Privkey};

#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use ractor::{call, ActorRef};
use rand::Rng;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::time::Duration;
use tentacle::secio::SecioKeyPair;

pub use fiber_json_types::{
    Attribute, CkbInvoice, GetInvoiceResult, InvoiceData, InvoiceParams, InvoiceResult,
    NewInvoiceParams, ParseInvoiceParams, ParseInvoiceResult, SettleInvoiceParams,
    SettleInvoiceResult,
};

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
            let currency = config.currency();

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
        let error = |msg: &str| Err(rpc_error(msg.to_string(), params.clone()));

        // Convert the JSON currency to internal currency for comparison & building
        let params_currency = fiber_types::Currency::from(&params.currency);

        if let Some(currency) = self.currency {
            if currency != params_currency {
                return error(&format!(
                    "Currency must be {:?} with the chain network",
                    currency
                ));
            }
        }
        let mut invoice_builder = InvoiceBuilder::new(params_currency).amount(Some(params.amount));

        // Convert payment_preimage and payment_hash from JSON types -> internal Hash256
        let preimage_hash = params
            .payment_preimage
            .as_ref()
            .map(fiber_types::Hash256::from);
        let payment_hash = params.payment_hash.as_ref().map(fiber_types::Hash256::from);

        // If both preimage and hash are absent, a random preimage is generated.
        let preimage_opt = match (preimage_hash, payment_hash) {
            (Some(preimage), _) => Some(preimage),
            (None, None) => Some(gen_rand_sha256_hash()),
            _ => None,
        };

        if let Some(preimage) = preimage_opt {
            invoice_builder = invoice_builder.payment_preimage(preimage);
        }
        if let Some(hash) = payment_hash {
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
        if let Some(hash_algorithm) = params.hash_algorithm.as_ref() {
            invoice_builder =
                invoice_builder.hash_algorithm(fiber_types::HashAlgorithm::from(hash_algorithm));
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
                        invoice: JsonCkbInvoice::from(&invoice),
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
                invoice: JsonCkbInvoice::from(&invoice),
            }),
            Err(e) => Err(rpc_error(e.to_string(), params)),
        }
    }

    pub async fn get_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = fiber_types::Hash256::from(&params.payment_hash);
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
                    invoice: JsonCkbInvoice::from(&invoice),
                    status: JsonCkbInvoiceStatus::from(&status),
                })
            }
            None => Err(rpc_error("invoice not found", params)),
        }
    }

    pub async fn cancel_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = fiber_types::Hash256::from(&params.payment_hash);
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
                        return Err(rpc_error(
                            format!("invoice can not be canceled, current status: {}", status),
                            params,
                        ));
                    }
                    _ => CkbInvoiceStatus::Cancelled,
                };
                self.store
                    .update_invoice_status(&payment_hash, new_status)
                    .rpc_err(&params)?;
                if let Some(network_actor) = &self.network_actor {
                    let _ = network_actor.send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SettleHoldTlcSet(payment_hash),
                    ));
                }
                Ok(GetInvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice: JsonCkbInvoice::from(&invoice),
                    status: JsonCkbInvoiceStatus::from(&new_status),
                })
            }
            None => Err(rpc_error("invoice not found", params)),
        }
    }

    pub async fn settle_invoice(
        &self,
        params: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned> {
        let network_actor = self
            .network_actor
            .as_ref()
            .ok_or_else(|| rpc_error_no_data("network actor not initialized"))?;

        let payment_hash = fiber_types::Hash256::from(&params.payment_hash);
        let payment_preimage = fiber_types::Hash256::from(&params.payment_preimage);

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
