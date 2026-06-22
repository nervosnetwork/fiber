use crate::cch::CchMessage;
use crate::rpc::utils::RpcResultExt;
use jsonrpsee::{proc_macros::rpc, types::ErrorObjectOwned, PendingSubscriptionSink};
use ractor::{call_t, ActorRef};

pub use fiber_json_types::{
    CchNewOrderResponse, CchOrderResponse, GetCchOrderParams, ReceiveBTCParams, SendBTCParams,
    SubmitSwapProposalResponseParams, SwapProposal,
};
/// RPC module for cross chain hub demonstration.
#[rpc(server)]
trait CchRpc {
    /// Creates a CCH order for a BTC Lightning payee.
    ///
    /// For a fixed-rate Fiber asset the counterparty invoice is minted inline
    /// and the response carries the created order (already `Pending`). For a
    /// non-fixed-rate asset no order is created yet: the response carries the
    /// swap proposal the hub is broadcasting to operators while it awaits a
    /// decision. Poll `get_cch_order` until the order appears (`Pending`); if
    /// the operator rejects the proposal or it times out, no order is ever
    /// created and `get_cch_order` reports it as not found.
    ///
    /// The result is an externally-tagged `CchNewOrderResponse` JSON enum:
    ///
    /// * `{"Order": <CchOrderResponse>}` — fixed-rate asset: the order was
    ///   created and is already `Pending`. Contains `incoming_invoice` and
    ///   `payment_hash` for the Fiber leg.
    /// * `{"PendingProposal": <SwapProposal>}` — non-fixed-rate asset: no
    ///   order exists yet; the hub is awaiting an operator decision.
    #[method(name = "send_btc")]
    async fn send_btc(
        &self,
        params: SendBTCParams,
    ) -> Result<CchNewOrderResponse, ErrorObjectOwned>;

    /// Creates a CCH order for a CKB Fiber payee.
    ///
    /// For a fixed-rate Fiber asset the counterparty invoice is minted inline
    /// and the response carries the created order (already `Pending`). For a
    /// non-fixed-rate asset no order is created yet: the response carries the
    /// swap proposal the hub is broadcasting to operators while it awaits a
    /// decision. Poll `get_cch_order` until the order appears (`Pending`); if
    /// the operator rejects the proposal or it times out, no order is ever
    /// created and `get_cch_order` reports it as not found.
    ///
    /// The result is an externally-tagged `CchNewOrderResponse` JSON enum:
    ///
    /// * `{"Order": <CchOrderResponse>}` — fixed-rate asset: the order was
    ///   created and is already `Pending`. Contains `incoming_invoice` and
    ///   `payment_hash` for the BTC leg.
    /// * `{"PendingProposal": <SwapProposal>}` — non-fixed-rate asset: no
    ///   order exists yet; the hub is awaiting an operator decision.
    #[method(name = "receive_btc")]
    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchNewOrderResponse, ErrorObjectOwned>;

    /// Get a CCH order by payment hash.
    #[method(name = "get_cch_order")]
    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Subscribe to swap proposals that the hub needs the operator to
    /// approve. Each notification carries a `proposal_id` that the operator
    /// must echo back via [`submit_swap_proposal_response`] before the
    /// configured timeout, otherwise the proposal is auto-rejected.
    ///
    /// (Spec §5 calls for inline subscription replies; jsonrpsee
    /// subscriptions are unidirectional, so we use a separate normal RPC
    /// method on the same connection.)
    #[subscription(
        name = "subscribe_swap_proposals" => "swap_proposal",
        unsubscribe = "unsubscribe_swap_proposals",
        item = SwapProposal
    )]
    async fn subscribe_swap_proposals(&self) -> jsonrpsee::core::SubscriptionResult;

    /// Submit an operator decision for a previously emitted swap proposal.
    /// Any authenticated operator may resolve any pending proposal — the
    /// acceptor does not require that the response arrive on the same
    /// WebSocket session that received the proposal notification. Operators
    /// can list outstanding proposals/orders via the orders RPC and submit
    /// decisions over any session.
    ///
    /// On accept the hub mints the counterparty invoice and creates the order
    /// as `Pending`; on reject it drops the pending proposal without creating
    /// an order. The call returns `Ok` once the decision is applied — the swap
    /// client observes the resulting state by polling `get_cch_order`.
    #[method(name = "submit_swap_proposal_response")]
    async fn submit_swap_proposal_response(
        &self,
        params: SubmitSwapProposalResponseParams,
    ) -> Result<(), ErrorObjectOwned>;
}

pub struct CchRpcServerImpl {
    cch_actor: ActorRef<CchMessage>,
}

impl CchRpcServerImpl {
    pub fn new(cch_actor: ActorRef<CchMessage>) -> Self {
        CchRpcServerImpl { cch_actor }
    }
}

const TIMEOUT: u64 = 1000;

#[async_trait::async_trait]
impl CchRpcServer for CchRpcServerImpl {
    /// Send BTC to a address.
    async fn send_btc(
        &self,
        params: SendBTCParams,
    ) -> Result<CchNewOrderResponse, ErrorObjectOwned> {
        // <Self as CchRpcServerImpl>::send_btc(self, params).await
        self.send_btc(params).await
    }

    /// Receive BTC from a payment hash.
    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchNewOrderResponse, ErrorObjectOwned> {
        self.receive_btc(params).await
    }

    /// Get receive BTC order by payment hash.
    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        self.get_cch_order(params).await
    }

    async fn subscribe_swap_proposals(
        &self,
        pending: PendingSubscriptionSink,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = pending.accept().await?;
        let _ = self
            .cch_actor
            .send_message(CchMessage::SubscribeSwapProposals(sink));
        Ok(())
    }

    async fn submit_swap_proposal_response(
        &self,
        params: SubmitSwapProposalResponseParams,
    ) -> Result<(), ErrorObjectOwned> {
        let result = call_t!(
            self.cch_actor,
            CchMessage::SubmitSwapProposalResponse,
            TIMEOUT,
            params.into()
        )
        .rpc_err()?;
        result.map_err(Into::into)
    }
}

// #[async_trait::async_trait(?Send)]
impl CchRpcServerImpl {
    async fn send_btc(
        &self,
        params: SendBTCParams,
    ) -> Result<CchNewOrderResponse, ErrorObjectOwned> {
        let currency = params.currency.into();
        // The proposal path does not block: the hub persists a pending proposal
        // and returns immediately, so a normal short mailbox timeout suffices.
        let result = call_t!(
            self.cch_actor,
            CchMessage::SendBTC,
            TIMEOUT,
            crate::cch::SendBTC {
                btc_pay_req: params.btc_pay_req,
                currency,
                fiber_type_script: params.fiber_type_script,
            }
        )
        .rpc_err()?;

        result.map(CchNewOrderResponse::from).map_err(Into::into)
    }

    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchNewOrderResponse, ErrorObjectOwned> {
        let result = call_t!(
            self.cch_actor,
            CchMessage::ReceiveBTC,
            TIMEOUT,
            crate::cch::ReceiveBTC {
                fiber_pay_req: params.fiber_pay_req,
            }
        )
        .rpc_err()?;

        result.map(CchNewOrderResponse::from).map_err(Into::into)
    }

    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.into();
        let result = call_t!(
            self.cch_actor,
            CchMessage::GetCchOrder,
            TIMEOUT,
            payment_hash
        )
        .rpc_err()?;

        result.map(CchOrderResponse::from).map_err(Into::into)
    }
}
