//! This module provides the RPC interface for creating, parsing, and retrieving invoices.
//!
//! We define CkbInvoice and its related types here only for the RPC interface.
//! For better separation of concerns, the actual invoice logic is implemented in the `invoice` module.
//!

use crate::fiber::config::{MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA};
use crate::fiber::{FiberActorCommand, FiberActorMessage, FiberActorRef, NetworkActorMessage};
use crate::invoice::{
    CkbInvoice as InternalCkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore,
};
use crate::rpc::utils::rpc_error;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "lsp")]
use crate::rpc::utils::RpcResultExt;
use crate::{gen_rand_sha256_hash, handle_actor_call, log_and_error, FiberConfig};
use fiber_types::{FeatureVector, Privkey};

#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::Extensions;
use ractor::{call, ActorRef};
use rand::Rng;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::time::Duration;
use tentacle::secio::SecioKeyPair;

pub use fiber_json_types::{
    Attribute, CkbInvoice, GetInvoiceParams, GetInvoiceResult, InvoiceData, InvoiceParams,
    InvoiceResult, NewInvoiceParams, ParseInvoiceParams, ParseInvoiceResult, SettleInvoiceParams,
    SettleInvoiceResult,
};

/// RPC module for invoice management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait InvoiceRpc {
    /// Generates a new invoice.
    #[method(name = "new_invoice", with_extensions)]
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
    #[method(name = "get_invoice", with_extensions)]
    async fn get_invoice(
        &self,
        params: GetInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    /// Cancels an invoice, only when invoice is in status `Open` can be canceled.
    #[method(name = "cancel_invoice", with_extensions)]
    async fn cancel_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    /// Settles an invoice by saving the preimage to this invoice.
    #[method(name = "settle_invoice", with_extensions)]
    async fn settle_invoice(
        &self,
        settle_invoice: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned>;
}

#[doc(hidden)]
pub struct InvoiceBackend<S> {
    store: S,
    fiber_actor: Option<FiberActorRef>,
    keypair: Option<(PublicKey, SecretKey)>,
    currency: Option<Currency>,
    node_features: Option<FeatureVector>,
    trampoline_route_hint: Option<PublicKey>,
    /// Hosted tenants must supply `payment_hash` or `payment_preimage` so the
    /// LSP does not generate and store a settle secret.
    require_client_payment_lock: bool,
}

impl<S> InvoiceBackend<S> {
    pub fn new(
        store: S,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        config: Option<FiberConfig>,
    ) -> Self {
        Self::new_fiber(
            store,
            network_actor.map(|actor| FiberActorRef::from_network(&actor)),
            config,
        )
    }

    pub(crate) fn new_fiber(
        store: S,
        fiber_actor: Option<FiberActorRef>,
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
            fiber_actor,
            keypair,
            currency,
            node_features,
            trampoline_route_hint: None,
            require_client_payment_lock: false,
        }
    }

    pub fn with_trampoline_route_hint(mut self, node_id: PublicKey) -> Self {
        self.trampoline_route_hint = Some(node_id);
        self
    }

    /// Require the caller to lock the invoice to a preimage they already know.
    pub fn with_require_client_payment_lock(mut self, required: bool) -> Self {
        self.require_client_payment_lock = required;
        self
    }
}

/// Registered invoice RPC module.
///
/// Holds only the resolver and the host config: the invoice backend (which owns
/// key material) is built from the request's scope, so a handler cannot reach the
/// public node's invoice keys without resolving the caller first.
pub struct InvoiceRpcServerImpl<S> {
    backend: crate::rpc_scope::BackendResolver<S>,
    config: Option<FiberConfig>,
}

impl<S: crate::rpc_scope::RpcBackendStore> InvoiceRpcServerImpl<S> {
    pub fn new(
        store: S,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        config: Option<FiberConfig>,
    ) -> Self {
        Self {
            backend: crate::rpc_scope::BackendResolver::new(
                network_actor.map(|actor| FiberActorRef::from_network(&actor)),
                store,
                None,
            ),
            config,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_lsp_actor(mut self, lsp_actor: crate::rpc::tenant::LspActorHandle) -> Self {
        self.backend = self.backend.with_lsp_actor(lsp_actor);
        self
    }

    /// Resolve the caller's tenant and build the invoice backend for it.
    #[cfg(not(target_arch = "wasm32"))]
    async fn backend_for(
        &self,
        extensions: &Extensions,
    ) -> Result<InvoiceBackend<S>, ErrorObjectOwned> {
        Ok(self.for_scope(&self.backend.resolve(extensions).await?))
    }

    /// Resolve a query target that may name a tenant explicitly.
    #[cfg(not(target_arch = "wasm32"))]
    async fn backend_for_query(
        &self,
        extensions: &Extensions,
        requested_tenant: Option<&str>,
    ) -> Result<InvoiceBackend<S>, ErrorObjectOwned> {
        Ok(self.for_scope(
            &self
                .backend
                .resolve_query(extensions, requested_tenant)
                .await?,
        ))
    }

    /// Build the invoice backend for a resolved scope.
    fn for_scope(&self, scope: &crate::rpc_scope::ScopedBackend<S>) -> InvoiceBackend<S> {
        let config = scope
            .tenant()
            .map(|tenant| tenant.config.clone())
            .or_else(|| self.config.clone());
        let mut backend =
            InvoiceBackend::new_fiber(scope.store.clone(), scope.actor_opt().cloned(), config);
        if let Some(tenant) = scope.tenant() {
            backend = backend
                .with_trampoline_route_hint(tenant.public_node_id.into())
                .with_require_client_payment_lock(true);
        }
        backend
    }

    /// The public invoice backend, for callers that hold no request (wasm, tests).
    ///
    /// Not available to handlers: they must resolve the request's tenant first.
    /// The public invoice backend. Handlers use it only after `resolve` reported
    /// that no tenant is involved.
    fn public_backend(&self) -> InvoiceBackend<S> {
        self.for_scope(&self.backend.public_scope())
    }

    /// Handle exposed to request-less callers (wasm bindings, tests).
    #[cfg(any(test, target_arch = "wasm32"))]
    pub fn public_handler(&self) -> InvoiceBackend<S> {
        self.public_backend()
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> InvoiceRpcServer for InvoiceRpcServerImpl<S>
where
    S: InvoiceStore + crate::rpc_scope::RpcBackendStore + Send + Sync + 'static,
{
    /// Generates a new invoice.
    async fn new_invoice(
        &self,
        extensions: &Extensions,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
        let scope = self.backend.resolve(extensions).await?;
        let backend = self.for_scope(&scope);
        if let Some(tenant) = scope.tenant() {
            if params.allow_mpp == Some(true) {
                return Err(rpc_error("hosted tenant invoices do not support MPP"));
            }
            let tenant_id = tenant.tenant_id.clone();
            let result = backend.new_invoice(params).await?;
            let invoice = result
                .invoice_address
                .parse::<InternalCkbInvoice>()
                .map_err(|error| {
                    rpc_error(format!("failed to parse generated hosted invoice: {error}"))
                })?;
            #[cfg(feature = "lsp")]
            {
                let lsp_actor = self
                    .backend
                    .lsp()
                    .as_ref()
                    .ok_or_else(|| rpc_error("hosted LSP service is not enabled"))?;
                call!(lsp_actor, |reply| {
                    crate::lsp::LspServiceMessage::RegisterInvoice {
                        tenant_id,
                        invoice,
                        buffer_duration_ms: None,
                        reply,
                    }
                })
                .rpc_err()?
                .rpc_err()?;
            }
            #[cfg(not(feature = "lsp"))]
            {
                let _ = (tenant_id, invoice);
            }
            return Ok(result);
        }
        self.public_backend().new_invoice(params).await
    }

    /// Parses a encoded invoice.
    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned> {
        self.public_backend().parse_invoice(params).await
    }

    /// Retrieves an invoice.
    async fn get_invoice(
        &self,
        extensions: &Extensions,
        params: GetInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let backend = self
            .backend_for_query(extensions, params.tenant_id.as_deref())
            .await?;
        let params = InvoiceParams {
            payment_hash: params.payment_hash,
        };
        backend.get_invoice(params).await
    }

    /// Cancels an invoice, only when invoice is in status `Open` can be canceled.
    async fn cancel_invoice(
        &self,
        extensions: &Extensions,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        self.backend_for(extensions)
            .await?
            .cancel_invoice(payment_hash)
            .await
    }

    /// Settles an invoice by saving the preimage to this invoice.
    async fn settle_invoice(
        &self,
        extensions: &Extensions,
        settle_invoice: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned> {
        self.backend_for(extensions)
            .await?
            .settle_invoice(settle_invoice)
            .await
    }
}

impl<S> InvoiceBackend<S>
where
    S: InvoiceStore + Send + Sync + 'static,
{
    pub async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
        let error = |msg: &str| Err(rpc_error(msg.to_string()));

        let params_currency = params.currency.into();

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
        let preimage_hash = params.payment_preimage.map(fiber_types::Hash256::from);
        let payment_hash = params.payment_hash.map(fiber_types::Hash256::from);

        // If both preimage and hash are absent, a random preimage is generated
        // unless this is a hosted tenant invoice. Hosted invoices must be
        // locked to a client-owned secret so the LSP cannot settle them.
        let preimage_opt = match (preimage_hash, payment_hash) {
            (Some(preimage), _) => Some(preimage),
            (None, None) if self.require_client_payment_lock => {
                return error(
                    "hosted invoices require payment_hash or payment_preimage from the client",
                );
            }
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
                let mut rng = rand::rng();
                let payment_secret: [u8; 32] = rng.random();
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

        if let Some(node_id) = self.trampoline_route_hint {
            invoice_builder = invoice_builder.trampoline_route_hint(node_id);
        }

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
            invoice_builder = invoice_builder.hash_algorithm(hash_algorithm.into());
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
                        invoice: invoice.clone().into(),
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
        let result = InternalCkbInvoice::from_str_allowing_unsigned(&params.invoice);
        match result {
            Ok(invoice) => Ok(ParseInvoiceResult {
                invoice: invoice.into(),
            }),
            Err(e) => Err(rpc_error(e.to_string())),
        }
    }

    pub async fn get_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.into();
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
                    status: status.into(),
                })
            }
            None => Err(rpc_error("invoice not found")),
        }
    }

    pub async fn cancel_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let fiber_actor = self
            .fiber_actor
            .as_ref()
            .ok_or_else(|| rpc_error("network actor not initialized"))?;

        let payment_hash = params.payment_hash.into();
        match self.store.get_invoice(&payment_hash) {
            Some(invoice) => {
                let message = move |rpc_reply| -> FiberActorMessage {
                    FiberActorMessage::new_command(FiberActorCommand::CancelInvoice(
                        payment_hash,
                        rpc_reply,
                    ))
                };
                handle_actor_call!(fiber_actor, message, params)?;

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
                    status: status.into(),
                })
            }
            None => Err(rpc_error("invoice not found")),
        }
    }

    pub async fn settle_invoice(
        &self,
        params: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned> {
        let fiber_actor = self
            .fiber_actor
            .as_ref()
            .ok_or_else(|| rpc_error("network actor not initialized"))?;

        let payment_hash = params.payment_hash.into();
        let payment_preimage = params.payment_preimage.into();

        let message = move |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::SettleInvoice(
                payment_hash,
                payment_preimage,
                rpc_reply,
            ))
        };

        handle_actor_call!(fiber_actor, message, params).map(|_| SettleInvoiceResult {})
    }
}
