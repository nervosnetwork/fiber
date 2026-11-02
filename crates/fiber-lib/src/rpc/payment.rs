use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::network::BuildRouterCommand;
use crate::fiber::payment::SendPaymentWithRouterCommand;
use crate::fiber::{
    channel::ChannelActorStateStore, payment::SendPaymentCommand, FiberActorCommand,
    FiberActorMessage, FiberActorRef, NetworkActorMessage,
};
use crate::rpc::scoped::{BackendResolver, RpcBackendStore, ScopedBackend};
use crate::rpc::utils::{rpc_error, RpcResultExt};
use crate::{handle_actor_call, log_and_error};
#[cfg(debug_assertions)]
use fiber_json_types::SessionRoute as JsonSessionRoute;
use fiber_json_types::{
    PaymentCustomRecords as JsonPaymentCustomRecords, RouterHop as JsonRouterHop,
};

#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::Extensions;

use ractor::{call, ActorRef};

pub use fiber_json_types::{
    BuildPaymentRouterResult, BuildRouterParams, GetPaymentCommandParams, GetPaymentCommandResult,
    HopHint, ListPaymentsParams, ListPaymentsResult, PaymentCustomRecords,
    SendPaymentCommandParams, SendPaymentWithRouterParams,
};

/// RPC module for channel management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait PaymentRpc {
    /// Sends a payment to a peer.
    #[method(name = "send_payment", with_extensions)]
    async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Retrieves a payment.
    #[method(name = "get_payment", with_extensions)]
    async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Builds a router with a list of pubkeys and required channels.
    #[method(name = "build_router", with_extensions)]
    async fn build_router(
        &self,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned>;

    /// Sends a payment to a peer with specified router.
    /// This method differs from SendPayment in that it allows users to specify a full route manually.
    ///
    /// A typical use case is **channel rebalancing**: you can construct a circular route
    /// (your node -> intermediate nodes -> your node) to shift liquidity between your channels.
    ///
    /// To rebalance, follow these steps:
    ///
    /// 1. Call `build_router` with `hops_info` defining the circular route you want,
    ///    e.g. your_node -> peer_A -> peer_B -> your_node.
    /// 2. Call `send_payment_with_router` with the returned `router_hops` and `keysend: true`.
    ///
    /// Only routing fees are deducted; your total balance across channels remains the same.
    #[method(name = "send_payment_with_router", with_extensions)]
    async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Lists all payments, optionally filtered by status.
    #[method(name = "list_payments", with_extensions)]
    async fn list_payments(
        &self,
        params: ListPaymentsParams,
    ) -> Result<ListPaymentsResult, ErrorObjectOwned>;
}

pub struct PaymentRpcServerImpl<S> {
    backend: BackendResolver<S>,
}

impl<S: RpcBackendStore> PaymentRpcServerImpl<S> {
    pub fn new(actor: ActorRef<NetworkActorMessage>, store: S) -> Self {
        Self::new_fiber(FiberActorRef::from_network(&actor), store)
    }

    pub(crate) fn new_fiber(actor: FiberActorRef, store: S) -> Self {
        PaymentRpcServerImpl {
            backend: BackendResolver::new(actor, store, None),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_lsp_actor(mut self, lsp_actor: crate::rpc::tenant::LspActorHandle) -> Self {
        self.backend = self.backend.with_lsp_actor(lsp_actor);
        self
    }

    /// The public backend, for callers that hold no request (wasm bindings).
    pub fn public_scope(&self) -> ScopedBackend<S> {
        self.backend.public_scope()
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> PaymentRpcServer for PaymentRpcServerImpl<S>
where
    S: ChannelActorStateStore + NetworkGraphStateStore + RpcBackendStore + Send + Sync + 'static,
{
    /// Sends a payment to a peer.
    async fn send_payment(
        &self,
        extensions: &Extensions,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.backend
            .resolve(extensions)
            .await?
            .send_payment(params)
            .await
    }

    /// Retrieves a payment.
    async fn get_payment(
        &self,
        extensions: &Extensions,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let backend = self
            .backend
            .resolve_query(extensions, params.tenant_id.as_deref())
            .await?;
        let params = GetPaymentCommandParams {
            payment_hash: params.payment_hash,
            tenant_id: None,
        };
        backend.get_payment(params).await
    }

    /// Builds a router with a list of pubkeys and required channels.
    async fn build_router(
        &self,
        extensions: &Extensions,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned> {
        self.backend
            .resolve(extensions)
            .await?
            .build_router(params)
            .await
    }

    /// Sends a payment to a peer with specified router.
    /// This method differs from SendPayment in that it allows users to specify a full route manually.
    /// This can be used for things like channel rebalancing.
    async fn send_payment_with_router(
        &self,
        extensions: &Extensions,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.backend
            .resolve(extensions)
            .await?
            .send_payment_with_router(params)
            .await
    }

    /// Lists all payments, optionally filtered by status.
    async fn list_payments(
        &self,
        extensions: &Extensions,
        params: ListPaymentsParams,
    ) -> Result<ListPaymentsResult, ErrorObjectOwned> {
        self.backend
            .resolve(extensions)
            .await?
            .list_payments(params)
            .await
    }
}

/// Helper to convert an internal SendPaymentResponse to the JSON result type.
fn send_payment_response_to_json(
    response: &crate::fiber::network::SendPaymentResponse,
) -> GetPaymentCommandResult {
    GetPaymentCommandResult {
        payment_hash: response.payment_hash.into(),
        payment_preimage: response.payment_preimage.map(Into::into),
        status: response.status.into(),
        created_at: response.created_at,
        last_updated_at: response.last_updated_at,
        failed_error: response.failed_error.clone(),
        fee: response.fee,
        custom_records: response
            .custom_records
            .as_ref()
            .cloned()
            .map(JsonPaymentCustomRecords::from),
        #[cfg(debug_assertions)]
        routers: response
            .routers
            .iter()
            .cloned()
            .map(JsonSessionRoute::from)
            .collect(),
    }
}

impl<S> ScopedBackend<S>
where
    S: ChannelActorStateStore + NetworkGraphStateStore + Send + Sync + 'static,
{
    pub async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let target_pubkey = params
            .target_pubkey
            .map(fiber_types::Pubkey::try_from)
            .transpose()
            .rpc_err()?;
        let payment_hash = params.payment_hash.map(fiber_types::Hash256::from);
        let trampoline_hops = params
            .trampoline_hops
            .as_ref()
            .map(|hops| {
                hops.iter()
                    .copied()
                    .map(fiber_types::Pubkey::try_from)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .rpc_err()?;
        let custom_records = params
            .custom_records
            .clone()
            .map(fiber_types::PaymentCustomRecords::from);
        let hop_hints = params
            .hop_hints
            .as_ref()
            .map(|hints| {
                hints
                    .iter()
                    .cloned()
                    .map(fiber_types::HopHint::try_from)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .rpc_err()?;

        let message = |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::SendPayment(
                SendPaymentCommand {
                    target_pubkey,
                    amount: params.amount,
                    payment_hash,
                    final_tlc_expiry_delta: params.final_tlc_expiry_delta,
                    tlc_expiry_limit: params.tlc_expiry_limit,
                    invoice: params.invoice.clone(),
                    timeout: params.timeout,
                    max_fee_amount: params.max_fee_amount,
                    max_fee_rate: params.max_fee_rate,
                    max_parts: params.max_parts,
                    trampoline_hops,
                    keysend: params.keysend,
                    udt_type_script: params.udt_type_script.clone().map(|s| s.into()),
                    allow_self_payment: params.allow_self_payment.unwrap_or(false),
                    custom_records,
                    hop_hints,
                    dry_run: params.dry_run.unwrap_or(false),
                },
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params)
            .map(|response| send_payment_response_to_json(&response))
    }

    pub async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        if params.tenant_id.is_some() {
            return Err(rpc_error(
                "tenant_id is only supported by the native get_payment RPC",
            ));
        }
        let payment_hash = params.payment_hash.into();
        let message = |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::GetPayment(payment_hash, rpc_reply))
        };
        handle_actor_call!(self.actor, message, params)
            .map(|response| send_payment_response_to_json(&response))
    }

    pub async fn build_router(
        &self,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned> {
        let hops_info: Vec<_> = params
            .hops_info
            .iter()
            .cloned()
            .map(fiber_types::HopRequire::try_from)
            .collect::<Result<Vec<_>, _>>()
            .rpc_err()?;

        let message = |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::BuildPaymentRouter(
                BuildRouterCommand {
                    amount: params.amount,
                    hops_info,
                    udt_type_script: params.udt_type_script.clone().map(|x| x.into()),
                    final_tlc_expiry_delta: params.final_tlc_expiry_delta,
                },
                rpc_reply,
            ))
        };

        handle_actor_call!(self.actor, message, params).map(|response| BuildPaymentRouterResult {
            router_hops: response
                .router_hops
                .into_iter()
                .map(JsonRouterHop::from)
                .collect(),
        })
    }

    pub async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.map(fiber_types::Hash256::from);
        let router: Vec<_> = params
            .router
            .iter()
            .cloned()
            .map(fiber_types::RouterHop::try_from)
            .collect::<Result<Vec<_>, _>>()
            .rpc_err()?;
        let custom_records = params
            .custom_records
            .clone()
            .map(fiber_types::PaymentCustomRecords::from);

        let message = |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::SendPaymentWithRouter(
                SendPaymentWithRouterCommand {
                    payment_hash,
                    router,
                    invoice: params.invoice.clone(),
                    keysend: params.keysend,
                    udt_type_script: params.udt_type_script.clone().map(|s| s.into()),
                    custom_records,
                    dry_run: params.dry_run.unwrap_or(false),
                },
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params)
            .map(|response| send_payment_response_to_json(&response))
    }

    pub async fn list_payments(
        &self,
        params: ListPaymentsParams,
    ) -> Result<ListPaymentsResult, ErrorObjectOwned> {
        const MAX_LIST_PAYMENTS_LIMIT: u64 = 500;
        let default_limit: u64 = 15;
        let limit = std::cmp::min(
            params.limit.unwrap_or(default_limit),
            MAX_LIST_PAYMENTS_LIMIT,
        ) as usize;

        let after = params.after.map(fiber_types::Hash256::from);
        let status = params.status.map(fiber_types::PaymentStatus::from);

        let sessions = self
            .store
            .get_payment_sessions_with_limit(limit, after, status);

        let payments: Vec<GetPaymentCommandResult> = sessions
            .into_iter()
            .map(|session| {
                let response: crate::fiber::network::SendPaymentResponse = session.into();
                send_payment_response_to_json(&response)
            })
            .collect();

        let last_cursor = payments.last().map(|p| p.payment_hash);

        Ok(ListPaymentsResult {
            payments,
            last_cursor,
        })
    }
}
