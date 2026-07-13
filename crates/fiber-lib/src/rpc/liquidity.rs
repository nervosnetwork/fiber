//! JSON-RPC surface for liquidity quote and swap operations.

use fiber_json_types::{
    GetSwapParams, LiquidityQuoteResponse, LiquiditySwapRecord as JsonLiquiditySwapRecord,
    LiquiditySwapResponse, ListSwapsParams, ListSwapsResponse, LoopOutParams,
    ProviderAcceptLoopOutParams, ProviderQuoteLoopOutParams, QuoteLoopOutParams,
};
use fiber_types::LiquiditySwapState;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;

use crate::liquidity::store::{
    LiquidityStore, LiquiditySwapFilter, LiquiditySwapKind,
    LiquiditySwapRecord as StoreLiquiditySwapRecord,
};
use crate::rpc::utils::{rpc_error, RpcResultExt};

/// RPC module for liquidity management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait LiquidityRpc {
    /// Request a Loop Out quote from a provider.
    #[method(name = "quote_loop_out")]
    async fn quote_loop_out(
        &self,
        params: QuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, ErrorObjectOwned>;

    /// Execute a Loop Out swap after quote acceptance.
    #[method(name = "loop_out")]
    async fn loop_out(
        &self,
        params: LoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned>;

    /// Return one persisted liquidity swap.
    #[method(name = "get_swap")]
    async fn get_swap(
        &self,
        params: GetSwapParams,
    ) -> Result<Option<JsonLiquiditySwapRecord>, ErrorObjectOwned>;

    /// Return persisted liquidity swaps.
    #[method(name = "list_swaps")]
    async fn list_swaps(
        &self,
        params: ListSwapsParams,
    ) -> Result<ListSwapsResponse, ErrorObjectOwned>;

    /// Provider-side quote endpoint for a Loop Out request.
    #[method(name = "provider_quote_loop_out")]
    async fn provider_quote_loop_out(
        &self,
        params: ProviderQuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, ErrorObjectOwned>;

    /// Provider-side accept endpoint for a Loop Out quote.
    #[method(name = "provider_accept_loop_out")]
    async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned>;
}

/// Server implementation for the liquidity RPC module.
pub struct LiquidityRpcServerImpl<S> {
    store: S,
}

impl<S> LiquidityRpcServerImpl<S> {
    /// Construct a liquidity RPC server backed by `store`.
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> LiquidityRpcServer for LiquidityRpcServerImpl<S>
where
    S: LiquidityStore + Send + Sync + 'static,
{
    async fn quote_loop_out(
        &self,
        params: QuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, ErrorObjectOwned> {
        self.quote_loop_out(params).await
    }

    async fn loop_out(
        &self,
        params: LoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        self.loop_out(params).await
    }

    async fn get_swap(
        &self,
        params: GetSwapParams,
    ) -> Result<Option<JsonLiquiditySwapRecord>, ErrorObjectOwned> {
        self.get_swap(params).await
    }

    async fn list_swaps(
        &self,
        params: ListSwapsParams,
    ) -> Result<ListSwapsResponse, ErrorObjectOwned> {
        self.list_swaps(params).await
    }

    async fn provider_quote_loop_out(
        &self,
        params: ProviderQuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, ErrorObjectOwned> {
        self.provider_quote_loop_out(params).await
    }

    async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        self.provider_accept_loop_out(params).await
    }
}

impl<S> LiquidityRpcServerImpl<S>
where
    S: LiquidityStore + Send + Sync + 'static,
{
    /// Request a Loop Out quote from a provider.
    pub async fn quote_loop_out(
        &self,
        _params: QuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, ErrorObjectOwned> {
        Err(rpc_error(
            "liquidity quote execution is unavailable until the liquidity actor RPC boundary is wired",
        ))
    }

    /// Execute a Loop Out swap after quote acceptance.
    pub async fn loop_out(
        &self,
        _params: LoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        Err(rpc_error(
            "liquidity swap execution is unavailable until the liquidity actor RPC boundary is wired",
        ))
    }

    /// Return one persisted liquidity swap.
    pub async fn get_swap(
        &self,
        params: GetSwapParams,
    ) -> Result<Option<JsonLiquiditySwapRecord>, ErrorObjectOwned> {
        self.store
            .get_liquidity_swap(&params.swap_id.into())
            .map(|swap| swap.map(JsonLiquiditySwapRecord::from))
            .rpc_err()
    }

    /// Return persisted liquidity swaps.
    pub async fn list_swaps(
        &self,
        params: ListSwapsParams,
    ) -> Result<ListSwapsResponse, ErrorObjectOwned> {
        let filter = LiquiditySwapFilter {
            state: params
                .state
                .as_deref()
                .map(parse_liquidity_swap_state)
                .transpose()
                .rpc_err()?,
            asset_id: params.asset_id,
            limit: params.limit,
            cursor: params.cursor,
        };

        let page = self.store.list_liquidity_swaps(filter).rpc_err()?;
        Ok(ListSwapsResponse {
            swaps: page
                .swaps
                .into_iter()
                .map(JsonLiquiditySwapRecord::from)
                .collect(),
            next_cursor: page.next_cursor,
        })
    }

    /// Provider-side quote endpoint for a Loop Out request.
    pub async fn provider_quote_loop_out(
        &self,
        _params: ProviderQuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, ErrorObjectOwned> {
        Err(rpc_error(
            "provider loop-out quoting is unavailable until the liquidity actor RPC boundary is wired",
        ))
    }

    /// Provider-side accept endpoint for a Loop Out quote.
    pub async fn provider_accept_loop_out(
        &self,
        _params: ProviderAcceptLoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        Err(rpc_error(
            "provider loop-out acceptance is unavailable until the liquidity actor RPC boundary is wired",
        ))
    }
}

impl From<StoreLiquiditySwapRecord> for JsonLiquiditySwapRecord {
    fn from(record: StoreLiquiditySwapRecord) -> Self {
        Self {
            swap_id: record.swap_id.into(),
            swap_kind: record.swap_kind.into(),
            state: liquidity_swap_state_to_string(record.state),
            asset_id: record.asset_id,
            amount: record.amount,
            payment_hash: record.payment_hash.into(),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

impl From<LiquiditySwapKind> for fiber_json_types::LiquiditySwapKind {
    fn from(kind: LiquiditySwapKind) -> Self {
        match kind {
            LiquiditySwapKind::LoopOut => Self::LoopOut,
            LiquiditySwapKind::LoopIn => Self::LoopIn,
        }
    }
}

fn parse_liquidity_swap_state(value: &str) -> Result<LiquiditySwapState, String> {
    match value {
        "created" | "Created" => Ok(LiquiditySwapState::Created),
        "quoted" | "Quoted" => Ok(LiquiditySwapState::Quoted),
        "onchain_lock_pending" | "OnchainLockPending" => Ok(LiquiditySwapState::OnchainLockPending),
        "onchain_locked" | "OnchainLocked" => Ok(LiquiditySwapState::OnchainLocked),
        "payout_pending" | "PayoutPending" => Ok(LiquiditySwapState::PayoutPending),
        "payout_locked" | "PayoutLocked" => Ok(LiquiditySwapState::PayoutLocked),
        "payment_in_flight" | "PaymentInFlight" => Ok(LiquiditySwapState::PaymentInFlight),
        "payment_settled" | "PaymentSettled" => Ok(LiquiditySwapState::PaymentSettled),
        "claim_pending" | "ClaimPending" => Ok(LiquiditySwapState::ClaimPending),
        "refund_pending" | "RefundPending" => Ok(LiquiditySwapState::RefundPending),
        "success" | "Success" => Ok(LiquiditySwapState::Success),
        "failed" | "Failed" => Ok(LiquiditySwapState::Failed),
        "refunded" | "Refunded" => Ok(LiquiditySwapState::Refunded),
        _ => Err(format!("invalid liquidity swap state: {value}")),
    }
}

fn liquidity_swap_state_to_string(state: LiquiditySwapState) -> String {
    match state {
        LiquiditySwapState::Created => "created",
        LiquiditySwapState::Quoted => "quoted",
        LiquiditySwapState::OnchainLockPending => "onchain_lock_pending",
        LiquiditySwapState::OnchainLocked => "onchain_locked",
        LiquiditySwapState::PayoutPending => "payout_pending",
        LiquiditySwapState::PayoutLocked => "payout_locked",
        LiquiditySwapState::PaymentInFlight => "payment_in_flight",
        LiquiditySwapState::PaymentSettled => "payment_settled",
        LiquiditySwapState::ClaimPending => "claim_pending",
        LiquiditySwapState::RefundPending => "refund_pending",
        LiquiditySwapState::Success => "success",
        LiquiditySwapState::Failed => "failed",
        LiquiditySwapState::Refunded => "refunded",
    }
    .to_string()
}
