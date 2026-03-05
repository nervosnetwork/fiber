use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::network::BuildRouterCommand;
use crate::fiber::payment::SendPaymentWithRouterCommand;
use crate::fiber::{
    channel::ChannelActorStateStore, payment::SendPaymentCommand, NetworkActorCommand,
    NetworkActorMessage,
};
use crate::rpc::utils::RpcResultExt;
use crate::{handle_actor_call, log_and_error};
use fiber_json_types::serde_utils::Hash256 as JsonHash256;
use fiber_json_types::{
    PaymentCustomRecords as JsonPaymentCustomRecords, PaymentStatus as JsonPaymentStatus,
    RouterHop as JsonRouterHop, SessionRoute as JsonSessionRoute,
};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;

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
    #[method(name = "send_payment")]
    async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Retrieves a payment.
    #[method(name = "get_payment")]
    async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Builds a router with a list of pubkeys and required channels.
    #[method(name = "build_router")]
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
    #[method(name = "send_payment_with_router")]
    async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Lists all payments, optionally filtered by status.
    #[method(name = "list_payments")]
    async fn list_payments(
        &self,
        params: ListPaymentsParams,
    ) -> Result<ListPaymentsResult, ErrorObjectOwned>;
}

pub struct PaymentRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    store: S,
}

impl<S> PaymentRpcServerImpl<S> {
    pub fn new(actor: ActorRef<NetworkActorMessage>, store: S) -> Self {
        PaymentRpcServerImpl { actor, store }
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> PaymentRpcServer for PaymentRpcServerImpl<S>
where
    S: ChannelActorStateStore + NetworkGraphStateStore + Send + Sync + 'static,
{
    /// Sends a payment to a peer.
    async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.send_payment(params).await
    }

    /// Retrieves a payment.
    async fn get_payment(
        &self,
        params: GetPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.get_payment(params).await
    }

    /// Builds a router with a list of pubkeys and required channels.
    async fn build_router(
        &self,
        params: BuildRouterParams,
    ) -> Result<BuildPaymentRouterResult, ErrorObjectOwned> {
        self.build_router(params).await
    }

    /// Sends a payment to a peer with specified router.
    /// This method differs from SendPayment in that it allows users to specify a full route manually.
    /// This can be used for things like channel rebalancing.
    async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.send_payment_with_router(params).await
    }

    /// Lists all payments, optionally filtered by status.
    async fn list_payments(
        &self,
        params: ListPaymentsParams,
    ) -> Result<ListPaymentsResult, ErrorObjectOwned> {
        self.list_payments(params).await
    }
}

/// Helper to convert an internal SendPaymentResponse to the JSON result type.
fn send_payment_response_to_json(
    response: &crate::fiber::network::SendPaymentResponse,
) -> GetPaymentCommandResult {
    GetPaymentCommandResult {
        payment_hash: JsonHash256::from(&response.payment_hash),
        status: JsonPaymentStatus::from(&response.status),
        created_at: response.created_at,
        last_updated_at: response.last_updated_at,
        failed_error: response.failed_error.clone(),
        fee: response.fee,
        custom_records: response
            .custom_records
            .as_ref()
            .map(JsonPaymentCustomRecords::from),
        #[cfg(debug_assertions)]
        routers: response
            .routers
            .iter()
            .map(JsonSessionRoute::from)
            .collect(),
    }
}

impl<S> PaymentRpcServerImpl<S>
where
    S: ChannelActorStateStore + NetworkGraphStateStore + Send + Sync + 'static,
{
    pub async fn send_payment(
        &self,
        params: SendPaymentCommandParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let target_pubkey = params
            .target_pubkey
            .as_ref()
            .map(fiber_types::Pubkey::try_from)
            .transpose()
            .rpc_err(&params)?;
        let payment_hash = params.payment_hash.as_ref().map(fiber_types::Hash256::from);
        let trampoline_hops = params
            .trampoline_hops
            .as_ref()
            .map(|hops| {
                hops.iter()
                    .map(fiber_types::Pubkey::try_from)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .rpc_err(&params)?;
        let custom_records = params
            .custom_records
            .as_ref()
            .map(fiber_types::PaymentCustomRecords::from);
        let hop_hints = params
            .hop_hints
            .as_ref()
            .map(|hints| {
                hints
                    .iter()
                    .map(fiber_types::HopHint::try_from)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .rpc_err(&params)?;

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
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
        let payment_hash = fiber_types::Hash256::from(&params.payment_hash);
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
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
            .map(fiber_types::HopRequire::try_from)
            .collect::<Result<Vec<_>, _>>()
            .rpc_err(&params)?;

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::BuildPaymentRouter(
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
                .iter()
                .map(JsonRouterHop::from)
                .collect(),
        })
    }

    pub async fn send_payment_with_router(
        &self,
        params: SendPaymentWithRouterParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.as_ref().map(fiber_types::Hash256::from);
        let router: Vec<_> = params
            .router
            .iter()
            .map(fiber_types::RouterHop::try_from)
            .collect::<Result<Vec<_>, _>>()
            .rpc_err(&params)?;
        let custom_records = params
            .custom_records
            .as_ref()
            .map(fiber_types::PaymentCustomRecords::from);

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPaymentWithRouter(
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
        let default_limit: u64 = 15;
        let limit = params.limit.unwrap_or(default_limit) as usize;

        let after = params.after.as_ref().map(fiber_types::Hash256::from);
        let status = params.status.as_ref().map(fiber_types::PaymentStatus::from);

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
