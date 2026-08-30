//! JSON-RPC surface for liquidity quote and swap operations.

use std::time::Duration;

use fiber_json_types::{
    AddLiquidityAssetParams, GetSwapParams, ImportLiquidityQuoteParams, LiquidityAssetInfo,
    LiquidityProviderStatus, LiquidityQuoteEnvelope,
    LiquiditySwapRecord as JsonLiquiditySwapRecord, LiquiditySwapResponse,
    ListLiquidityAssetsResponse, ListLiquidityChainTransactionsParams,
    ListLiquidityChainTransactionsResponse, ListSwapsParams, ListSwapsResponse, LoopInParams,
    LoopOutParams, ProviderAcceptLoopInParams, ProviderAcceptLoopOutParams,
    ProviderQuoteLoopOutParams, QuoteLoopInParams, QuoteLoopOutParams,
    SetLiquidityProviderModeParams, UpdateLiquidityAssetParams,
};
use fiber_types::LiquiditySwapState;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use ractor::{call, ActorRef, RpcReplyPort};
use serde::Serialize;

use crate::liquidity::actor::LiquidityActorMessage;
use crate::liquidity::store::{
    LiquidityStore, LiquiditySwapFilter, LiquiditySwapKind,
    LiquiditySwapRecord as StoreLiquiditySwapRecord,
};
use crate::log_and_error;
use crate::rpc::utils::{rpc_error, RpcResultExt};

#[cfg(not(test))]
const LIQUIDITY_RPC_ACTOR_CALL_TIMEOUT_MS: u64 = 30_000;
#[cfg(test)]
const LIQUIDITY_RPC_ACTOR_CALL_TIMEOUT_MS: u64 = 10;

/// RPC module for liquidity management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait LiquidityRpc {
    /// Request a Loop Out quote from a provider.
    #[method(name = "quote_loop_out")]
    async fn quote_loop_out(
        &self,
        params: QuoteLoopOutParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned>;

    /// Validate and persist complete quote terms received from an independent provider node.
    ///
    /// The quote must be unexpired and its provider and routing fees must not exceed the supplied
    /// caps. The complete canonical envelope, including the final asset and lock scripts, is
    /// returned after persistence.
    #[method(name = "import_liquidity_quote")]
    async fn import_liquidity_quote(
        &self,
        params: ImportLiquidityQuoteParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned>;

    /// Execute a Loop Out swap after quote acceptance.
    #[method(name = "loop_out")]
    async fn loop_out(
        &self,
        params: LoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned>;

    /// Request a Loop In quote from a provider.
    #[method(name = "quote_loop_in")]
    async fn quote_loop_in(
        &self,
        params: QuoteLoopInParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned>;

    /// Execute a Loop In swap after quote acceptance.
    #[method(name = "loop_in")]
    async fn loop_in(
        &self,
        params: LoopInParams,
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
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned>;

    /// Provider-side accept endpoint for a Loop Out quote.
    #[method(name = "provider_accept_loop_out")]
    async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned>;

    /// Provider-side accept endpoint for an observed Loop In lock.
    #[method(name = "provider_accept_loop_in")]
    async fn provider_accept_loop_in(
        &self,
        params: ProviderAcceptLoopInParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned>;

    /// Add a provider asset registry entry.
    #[method(name = "add_liquidity_asset")]
    async fn add_liquidity_asset(
        &self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned>;

    /// Update a provider asset registry entry.
    #[method(name = "update_liquidity_asset")]
    async fn update_liquidity_asset(
        &self,
        params: UpdateLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned>;

    /// Disable a provider asset registry entry.
    #[method(name = "disable_liquidity_asset")]
    async fn disable_liquidity_asset(
        &self,
        asset_id: String,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned>;

    /// List configured provider assets.
    #[method(name = "list_liquidity_assets")]
    async fn list_liquidity_assets(&self) -> Result<ListLiquidityAssetsResponse, ErrorObjectOwned>;

    /// Return persisted chain transactions for a liquidity swap.
    #[method(name = "list_liquidity_chain_transactions")]
    async fn list_liquidity_chain_transactions(
        &self,
        params: ListLiquidityChainTransactionsParams,
    ) -> Result<ListLiquidityChainTransactionsResponse, ErrorObjectOwned>;

    /// Return provider status.
    #[method(name = "get_liquidity_provider_status")]
    async fn get_liquidity_provider_status(
        &self,
    ) -> Result<LiquidityProviderStatus, ErrorObjectOwned>;

    /// Enable or disable liquidity provider mode.
    ///
    /// When disabled, all provider quote and accept endpoints are rejected until provider mode is
    /// enabled again. Recovery of already accepted swaps is unaffected.
    #[method(name = "set_liquidity_provider_mode")]
    async fn set_liquidity_provider_mode(
        &self,
        params: SetLiquidityProviderModeParams,
    ) -> Result<LiquidityProviderStatus, ErrorObjectOwned>;
}

/// Server implementation for the liquidity RPC module.
pub struct LiquidityRpcServerImpl<S> {
    store: S,
    actor: Option<ActorRef<LiquidityActorMessage>>,
}

/// Return the JSON-RPC method names exposed by the liquidity module.
pub fn liquidity_rpc_method_names() -> Vec<&'static str> {
    vec![
        "quote_loop_out",
        "import_liquidity_quote",
        "loop_out",
        "quote_loop_in",
        "loop_in",
        "get_swap",
        "list_swaps",
        "provider_quote_loop_out",
        "provider_accept_loop_out",
        "provider_accept_loop_in",
        "add_liquidity_asset",
        "update_liquidity_asset",
        "disable_liquidity_asset",
        "list_liquidity_assets",
        "list_liquidity_chain_transactions",
        "get_liquidity_provider_status",
        "set_liquidity_provider_mode",
    ]
}

impl<S> LiquidityRpcServerImpl<S> {
    /// Construct a liquidity RPC server backed by `store`.
    pub fn new(store: S, actor: Option<ActorRef<LiquidityActorMessage>>) -> Self {
        Self { store, actor }
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
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        self.quote_loop_out(params).await
    }

    async fn import_liquidity_quote(
        &self,
        params: ImportLiquidityQuoteParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        self.import_liquidity_quote(params).await
    }

    async fn loop_out(
        &self,
        params: LoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        self.loop_out(params).await
    }

    async fn quote_loop_in(
        &self,
        params: QuoteLoopInParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        self.quote_loop_in(params).await
    }

    async fn loop_in(
        &self,
        params: LoopInParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        self.loop_in(params).await
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
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        self.provider_quote_loop_out(params).await
    }

    async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        self.provider_accept_loop_out(params).await
    }

    async fn provider_accept_loop_in(
        &self,
        params: ProviderAcceptLoopInParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        self.provider_accept_loop_in(params).await
    }

    async fn add_liquidity_asset(
        &self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        self.add_liquidity_asset(params).await
    }

    async fn update_liquidity_asset(
        &self,
        params: UpdateLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        self.update_liquidity_asset(params).await
    }

    async fn disable_liquidity_asset(
        &self,
        asset_id: String,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        self.disable_liquidity_asset(asset_id).await
    }

    async fn list_liquidity_assets(&self) -> Result<ListLiquidityAssetsResponse, ErrorObjectOwned> {
        self.list_liquidity_assets().await
    }

    async fn list_liquidity_chain_transactions(
        &self,
        params: ListLiquidityChainTransactionsParams,
    ) -> Result<ListLiquidityChainTransactionsResponse, ErrorObjectOwned> {
        self.list_liquidity_chain_transactions(params).await
    }

    async fn get_liquidity_provider_status(
        &self,
    ) -> Result<LiquidityProviderStatus, ErrorObjectOwned> {
        self.get_liquidity_provider_status().await
    }

    async fn set_liquidity_provider_mode(
        &self,
        params: SetLiquidityProviderModeParams,
    ) -> Result<LiquidityProviderStatus, ErrorObjectOwned> {
        self.set_liquidity_provider_mode(params).await
    }
}

impl<S> LiquidityRpcServerImpl<S>
where
    S: LiquidityStore + Send + Sync + 'static,
{
    /// Request a Loop Out quote from a provider.
    pub async fn quote_loop_out(
        &self,
        params: QuoteLoopOutParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::QuoteLoopOut(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Validate and persist complete quote terms received from an independent provider node.
    pub async fn import_liquidity_quote(
        &self,
        params: ImportLiquidityQuoteParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::ImportLiquidityQuote(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Execute a Loop Out swap after quote acceptance.
    pub async fn loop_out(
        &self,
        params: LoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::LoopOut(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Request a Loop In quote from a provider.
    pub async fn quote_loop_in(
        &self,
        params: QuoteLoopInParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::QuoteLoopIn(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Execute a Loop In swap after quote acceptance.
    pub async fn loop_in(
        &self,
        params: LoopInParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::LoopIn(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Return one persisted liquidity swap.
    pub async fn get_swap(
        &self,
        params: GetSwapParams,
    ) -> Result<Option<JsonLiquiditySwapRecord>, ErrorObjectOwned> {
        self.store
            .get_liquidity_swap(&params.swap_id.into())
            .map(|swap| swap.map(json_liquidity_swap_record_from_store))
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
                .map(json_liquidity_swap_record_from_store)
                .collect(),
            next_cursor: page.next_cursor,
        })
    }

    /// Provider-side quote endpoint for a Loop Out request.
    pub async fn provider_quote_loop_out(
        &self,
        params: ProviderQuoteLoopOutParams,
    ) -> Result<LiquidityQuoteEnvelope, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::ProviderQuoteLoopOut(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Provider-side accept endpoint for a Loop Out quote.
    pub async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::ProviderAcceptLoopOut(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Provider-side accept endpoint for an observed Loop In lock.
    pub async fn provider_accept_loop_in(
        &self,
        params: ProviderAcceptLoopInParams,
    ) -> Result<LiquiditySwapResponse, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::ProviderAcceptLoopIn(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Add a provider asset registry entry.
    pub async fn add_liquidity_asset(
        &self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::AddLiquidityAsset(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Update a provider asset registry entry.
    pub async fn update_liquidity_asset(
        &self,
        params: UpdateLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::UpdateLiquidityAsset(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Disable a provider asset registry entry.
    pub async fn disable_liquidity_asset(
        &self,
        asset_id: String,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = asset_id.clone();
        let message = move |reply| LiquidityActorMessage::DisableLiquidityAsset(asset_id, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// List configured provider assets.
    pub async fn list_liquidity_assets(
        &self,
    ) -> Result<ListLiquidityAssetsResponse, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = "list_liquidity_assets";
        let message = move |reply| LiquidityActorMessage::ListLiquidityAssets(reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Return persisted chain transactions for a liquidity swap.
    pub async fn list_liquidity_chain_transactions(
        &self,
        params: ListLiquidityChainTransactionsParams,
    ) -> Result<ListLiquidityChainTransactionsResponse, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message =
            move |reply| LiquidityActorMessage::ListLiquidityChainTransactions(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Return provider status.
    pub async fn get_liquidity_provider_status(
        &self,
    ) -> Result<LiquidityProviderStatus, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = "get_liquidity_provider_status";
        let message = move |reply| LiquidityActorMessage::GetLiquidityProviderStatus(reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }

    /// Enable or disable liquidity provider mode.
    pub async fn set_liquidity_provider_mode(
        &self,
        params: SetLiquidityProviderModeParams,
    ) -> Result<LiquidityProviderStatus, ErrorObjectOwned> {
        let actor = self
            .actor
            .as_ref()
            .ok_or_else(|| rpc_error("liquidity actor is not available"))?;
        let log_params = params.clone();
        let message = move |reply| LiquidityActorMessage::SetLiquidityProviderMode(params, reply);

        call_liquidity_actor(actor.clone(), message, &log_params).await
    }
}

async fn call_liquidity_actor<T, E, F, P>(
    actor: ActorRef<LiquidityActorMessage>,
    message: F,
    params: &P,
) -> Result<T, ErrorObjectOwned>
where
    T: Send + 'static,
    E: ToString + Send + 'static,
    F: FnOnce(RpcReplyPort<Result<T, E>>) -> LiquidityActorMessage + Send + 'static,
    P: Serialize,
{
    match tokio::time::timeout(
        Duration::from_millis(LIQUIDITY_RPC_ACTOR_CALL_TIMEOUT_MS),
        async move { call!(actor, message) },
    )
    .await
    {
        Ok(Ok(result)) => match result {
            Ok(response) => Ok(response),
            Err(error) => log_and_error!(params, error.to_string()),
        },
        Ok(Err(error)) => log_and_error!(params, error.to_string()),
        Err(_) => log_and_error!(params, "liquidity actor call timed out"),
    }
}

fn json_liquidity_swap_record_from_store(
    record: StoreLiquiditySwapRecord,
) -> JsonLiquiditySwapRecord {
    JsonLiquiditySwapRecord {
        swap_id: record.swap_id.into(),
        swap_kind: json_liquidity_swap_kind(record.swap_kind),
        state: liquidity_swap_state_to_string(record.state),
        asset_id: record.asset_id,
        amount: record.amount,
        payment_hash: record.payment_hash.into(),
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

fn json_liquidity_swap_kind(kind: LiquiditySwapKind) -> fiber_json_types::LiquiditySwapKind {
    match kind {
        LiquiditySwapKind::LoopOut => fiber_json_types::LiquiditySwapKind::LoopOut,
        LiquiditySwapKind::LoopIn => fiber_json_types::LiquiditySwapKind::LoopIn,
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::future::pending;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use fiber_json_types::{
        GetSwapParams, Hash256 as JsonHash256, ImportLiquidityQuoteParams,
        LiquidityChainTransaction, LiquidityChainTransactionRole, LiquidityQuoteEnvelope,
        ListLiquidityChainTransactionsParams, ListLiquidityChainTransactionsResponse,
        ListSwapsParams, LoopInParams, QuoteLoopInParams,
    };
    use fiber_types::{Hash256, LiquidityAsset, LiquiditySwapState};
    use ractor::{Actor, ActorProcessingErr, ActorRef};

    use super::*;
    use crate::liquidity::actor::LiquidityActorMessage;
    use crate::liquidity::store::{
        LiquidityStateTransition, LiquidityStoreError, LiquiditySwapPage, LiquiditySwapRole,
        LiquiditySwapUpdate,
    };

    #[derive(Default)]
    struct MockLiquidityStore {
        get_swap_result: Mutex<Option<StoreLiquiditySwapRecord>>,
        get_swap_id: Mutex<Option<Hash256>>,
        list_swaps_result: Mutex<LiquiditySwapPage>,
        list_filter: Mutex<Option<LiquiditySwapFilter>>,
    }

    impl MockLiquidityStore {
        fn with_swap(swap: StoreLiquiditySwapRecord) -> Self {
            Self {
                get_swap_result: Mutex::new(Some(swap)),
                ..Default::default()
            }
        }

        fn with_page(page: LiquiditySwapPage) -> Self {
            Self {
                list_swaps_result: Mutex::new(page),
                ..Default::default()
            }
        }

        fn recorded_get_swap_id(&self) -> Option<Hash256> {
            *self.get_swap_id.lock().expect("get swap id lock")
        }

        fn recorded_list_filter(&self) -> Option<LiquiditySwapFilter> {
            self.list_filter.lock().expect("list filter lock").clone()
        }
    }

    impl LiquidityStore for MockLiquidityStore {
        fn insert_loop_out_quote(
            &self,
            _quote: crate::liquidity::types::LoopOutQuoteTerms,
            _created_at: u64,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn get_loop_out_quote(
            &self,
            _quote_id: &Hash256,
        ) -> Result<Option<crate::liquidity::types::LoopOutQuoteTerms>, LiquidityStoreError>
        {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn insert_liquidity_swap(
            &self,
            _swap: StoreLiquiditySwapRecord,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn get_liquidity_swap(
            &self,
            swap_id: &Hash256,
        ) -> Result<Option<StoreLiquiditySwapRecord>, LiquidityStoreError> {
            *self.get_swap_id.lock().expect("get swap id lock") = Some(*swap_id);
            Ok(self
                .get_swap_result
                .lock()
                .expect("get result lock")
                .clone())
        }

        fn list_liquidity_swaps(
            &self,
            filter: LiquiditySwapFilter,
        ) -> Result<LiquiditySwapPage, LiquidityStoreError> {
            *self.list_filter.lock().expect("list filter lock") = Some(filter);
            Ok(self
                .list_swaps_result
                .lock()
                .expect("list result lock")
                .clone())
        }

        fn list_liquidity_swaps_by_states(
            &self,
            states: &[LiquiditySwapState],
            swap_kind: LiquiditySwapKind,
        ) -> Result<Vec<StoreLiquiditySwapRecord>, LiquidityStoreError> {
            Ok(self
                .list_swaps_result
                .lock()
                .expect("list result lock")
                .swaps
                .iter()
                .filter(|swap| swap.swap_kind == swap_kind && states.contains(&swap.state))
                .cloned()
                .collect())
        }

        fn update_liquidity_swap_state(
            &self,
            _swap_id: &Hash256,
            _transition: LiquidityStateTransition,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn update_liquidity_swap(
            &self,
            _swap_id: &Hash256,
            _update: LiquiditySwapUpdate,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn clear_liquidity_swap_failure_reason(
            &self,
            _swap_id: &Hash256,
            _expected_reason: &str,
            _updated_at: u64,
        ) -> Result<bool, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn insert_liquidity_chain_tx(
            &self,
            _record: fiber_types::LiquidityChainTxRecord,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn get_liquidity_chain_tx(
            &self,
            _swap_id: &Hash256,
            _role: fiber_types::LiquidityChainTxRole,
        ) -> Result<Option<fiber_types::LiquidityChainTxRecord>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn update_liquidity_chain_tx_status(
            &self,
            _swap_id: &Hash256,
            _role: fiber_types::LiquidityChainTxRole,
            _status: fiber_types::LiquidityChainTxStatus,
            _failure_reason: Option<String>,
            _updated_at: u64,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn list_liquidity_chain_txs_by_status(
            &self,
            _statuses: &[fiber_types::LiquidityChainTxStatus],
        ) -> Result<Vec<fiber_types::LiquidityChainTxRecord>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn list_liquidity_chain_txs_by_swap(
            &self,
            _swap_id: &Hash256,
        ) -> Result<Vec<fiber_types::LiquidityChainTxRecord>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn insert_liquidity_chain_tx_signed_tx(
            &self,
            _swap_id: &Hash256,
            _role: fiber_types::LiquidityChainTxRole,
            _tx: ckb_types::packed::Transaction,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn get_liquidity_chain_tx_signed_tx(
            &self,
            _swap_id: &Hash256,
            _role: fiber_types::LiquidityChainTxRole,
        ) -> Result<Option<ckb_types::packed::Transaction>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn upsert_liquidity_asset(
            &self,
            _asset: LiquidityAsset,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn get_liquidity_asset(
            &self,
            _asset_id: &str,
        ) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn set_provider_mode(&self, _enabled: bool) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("not implemented".to_string()))
        }

        fn get_provider_mode(&self) -> Result<bool, LiquidityStoreError> {
            Ok(false)
        }
    }

    fn liquidity_rpc_swap() -> StoreLiquiditySwapRecord {
        StoreLiquiditySwapRecord {
            swap_id: [1u8; 32].into(),
            quote_id: [2u8; 32].into(),
            role: LiquiditySwapRole::Client,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: "ckb".to_string(),
            state: LiquiditySwapState::PaymentSettled,
            payment_hash: [3u8; 32].into(),
            payment_preimage: Some([4u8; 32].into()),
            amount: 123,
            onchain_outpoint: None,
            payout_deadline: Some(456),
            refund_after_lock_time: 789,
            expires_at: 1000,
            failure_reason: None,
            created_at: 11,
            updated_at: 22,
        }
    }

    struct LiquidityRpcMock;

    #[async_trait::async_trait]
    impl Actor for LiquidityRpcMock {
        type Msg = LiquidityActorMessage;
        type State = Arc<Mutex<Vec<&'static str>>>;
        type Arguments = Arc<Mutex<Vec<&'static str>>>;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            events: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(events)
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            events: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            match message {
                LiquidityActorMessage::QuoteLoopOut(_params, reply) => {
                    events.lock().expect("events lock").push("quote_loop_out");
                    let _ = reply.send(Ok(liquidity_quote_envelope()));
                }
                LiquidityActorMessage::LoopOut(_params, reply) => {
                    events.lock().expect("events lock").push("loop_out");
                    let _ = reply.send(Ok(liquidity_swap_response()));
                }
                LiquidityActorMessage::QuoteLoopIn(_params, reply) => {
                    events.lock().expect("events lock").push("quote_loop_in");
                    let _ = reply.send(Ok(liquidity_quote_envelope()));
                }
                LiquidityActorMessage::ImportLiquidityQuote(params, reply) => {
                    assert_eq!(
                        serde_json::to_value(&params).expect("serialize received import params"),
                        serde_json::to_value(import_liquidity_quote_params())
                            .expect("serialize expected import params")
                    );
                    events
                        .lock()
                        .expect("events lock")
                        .push("import_liquidity_quote");
                    let _ = reply.send(Ok(params.quote));
                }
                LiquidityActorMessage::LoopIn(_params, reply) => {
                    events.lock().expect("events lock").push("loop_in");
                    let _ = reply.send(Ok(liquidity_swap_response()));
                }
                LiquidityActorMessage::ProviderQuoteLoopOut(_params, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("provider_quote_loop_out");
                    let _ = reply.send(Ok(liquidity_quote_envelope()));
                }
                LiquidityActorMessage::ProviderAcceptLoopOut(_params, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("provider_accept_loop_out");
                    let _ = reply.send(Ok(liquidity_swap_response()));
                }
                LiquidityActorMessage::ProviderAcceptLoopIn(_params, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("provider_accept_loop_in");
                    let _ = reply.send(Ok(liquidity_swap_response()));
                }
                LiquidityActorMessage::AddLiquidityAsset(_, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("add_liquidity_asset");
                    let _ = reply.send(Ok(liquidity_asset_info()));
                }
                LiquidityActorMessage::UpdateLiquidityAsset(_, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("update_liquidity_asset");
                    let _ = reply.send(Ok(liquidity_asset_info()));
                }
                LiquidityActorMessage::DisableLiquidityAsset(_, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("disable_liquidity_asset");
                    let _ = reply.send(Ok(liquidity_asset_info()));
                }
                LiquidityActorMessage::ListLiquidityAssets(reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("list_liquidity_assets");
                    let _ = reply.send(Ok(liquidity_list_assets_response()));
                }
                LiquidityActorMessage::GetLiquidityProviderStatus(reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("get_liquidity_provider_status");
                    let _ = reply.send(Ok(liquidity_provider_status()));
                }
                LiquidityActorMessage::SetLiquidityProviderMode(params, reply) => {
                    assert_eq!(
                        serde_json::to_value(&params).expect("serialize received mode params"),
                        serde_json::to_value(set_liquidity_provider_mode_params())
                            .expect("serialize expected mode params")
                    );
                    events
                        .lock()
                        .expect("events lock")
                        .push("set_liquidity_provider_mode");
                    let mut status = liquidity_provider_status();
                    status.enabled = params.enabled;
                    let _ = reply.send(Ok(status));
                }
                LiquidityActorMessage::ListLiquidityChainTransactions(_params, reply) => {
                    events
                        .lock()
                        .expect("events lock")
                        .push("list_liquidity_chain_transactions");
                    let _ = reply.send(Ok(liquidity_chain_transactions_response()));
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct StalledLiquidityRpcMock;

    #[async_trait::async_trait]
    impl Actor for StalledLiquidityRpcMock {
        type Msg = LiquidityActorMessage;
        type State = ();
        type Arguments = ();

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            _args: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(())
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            _state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            match message {
                LiquidityActorMessage::QuoteLoopIn(_, _)
                | LiquidityActorMessage::ImportLiquidityQuote(_, _)
                | LiquidityActorMessage::LoopIn(_, _)
                | LiquidityActorMessage::SetLiquidityProviderMode(_, _)
                | LiquidityActorMessage::ListLiquidityChainTransactions(_, _) => {
                    pending::<()>().await;
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct SpawnedLiquidityRpcMock {
        ref_: ActorRef<LiquidityActorMessage>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl SpawnedLiquidityRpcMock {
        fn take_events(&self) -> Vec<&'static str> {
            std::mem::take(&mut *self.events.lock().expect("events lock"))
        }
    }

    impl Drop for SpawnedLiquidityRpcMock {
        fn drop(&mut self) {
            self.ref_.stop(None);
        }
    }

    async fn spawn_liquidity_rpc_mock() -> SpawnedLiquidityRpcMock {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ref_, _handle) = ractor::Actor::spawn(None, LiquidityRpcMock, events.clone())
            .await
            .expect("spawn liquidity rpc mock");

        SpawnedLiquidityRpcMock { ref_, events }
    }

    async fn spawn_stalled_liquidity_rpc_mock() -> ActorRef<LiquidityActorMessage> {
        let (ref_, _handle) = ractor::Actor::spawn(None, StalledLiquidityRpcMock, ())
            .await
            .expect("spawn stalled liquidity rpc mock");

        ref_
    }

    struct ClosedReplyLiquidityRpcMock;

    #[async_trait::async_trait]
    impl Actor for ClosedReplyLiquidityRpcMock {
        type Msg = LiquidityActorMessage;
        type State = ();
        type Arguments = ();

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            _args: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(())
        }

        async fn handle(
            &self,
            myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            _state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            if let LiquidityActorMessage::ImportLiquidityQuote(_, reply) = message {
                drop(reply);
                myself.stop(None);
            }
            Ok(())
        }
    }

    async fn spawn_closed_reply_liquidity_rpc_mock() -> ActorRef<LiquidityActorMessage> {
        let (ref_, _handle) = ractor::Actor::spawn(None, ClosedReplyLiquidityRpcMock, ())
            .await
            .expect("spawn closed reply liquidity rpc mock");
        ref_
    }

    fn quote_loop_out_params() -> QuoteLoopOutParams {
        QuoteLoopOutParams {
            provider: "provider".to_string(),
            asset_id: "ckb".to_string(),
            amount: 100,
            claimant_lock: "0x".to_string(),
            max_provider_fee: 10,
            max_routing_fee: 5,
            expires_after_seconds: 60,
        }
    }

    fn loop_out_params() -> LoopOutParams {
        LoopOutParams {
            quote_id: JsonHash256([1u8; 32]),
            max_provider_fee: 10,
            max_routing_fee: 5,
            payout_outpoint: None,
        }
    }

    fn quote_loop_in_params() -> QuoteLoopInParams {
        QuoteLoopInParams {
            provider: "provider".to_string(),
            asset_id: "ckb".to_string(),
            amount: 100,
            client_invoice: "invoice".to_string(),
            refund_lock: "0x".to_string(),
            max_provider_fee: 10,
            max_routing_fee: 5,
            expires_after_seconds: 60,
        }
    }

    fn loop_in_params() -> LoopInParams {
        LoopInParams {
            quote_id: JsonHash256([1u8; 32]),
            funding_tx: "0xfunding".to_string(),
        }
    }

    fn provider_quote_loop_out_params() -> ProviderQuoteLoopOutParams {
        ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: 100,
            claimant_lock: "0x".to_string(),
            max_provider_fee: 10,
            max_routing_fee: 5,
            expires_after_seconds: 60,
        }
    }

    fn provider_accept_loop_out_params() -> ProviderAcceptLoopOutParams {
        ProviderAcceptLoopOutParams {
            quote_id: JsonHash256([1u8; 32]),
        }
    }

    fn liquidity_quote_envelope() -> LiquidityQuoteEnvelope {
        LiquidityQuoteEnvelope {
            quote_id: JsonHash256([1u8; 32]),
            swap_kind: fiber_json_types::LiquiditySwapKind::LoopOut,
            provider_pubkey: fiber_json_types::Pubkey([2u8; 33]),
            asset: liquidity_asset_info(),
            amount: 100,
            provider_fee: 10,
            routing_fee_limit: 5,
            onchain_fee_estimate_ckb: 1,
            capacity_requirement_ckb: 61,
            payment_hash: JsonHash256([3u8; 32]),
            expires_at: 1000,
            payout_deadline: 2000,
            refund_after_lock_time: 3000,
            claimant_lock: "0xclaimant".to_string(),
            refund_lock: "0xrefund".to_string(),
            client_invoice: None,
        }
    }

    fn import_liquidity_quote_params() -> ImportLiquidityQuoteParams {
        ImportLiquidityQuoteParams {
            quote: liquidity_quote_envelope(),
            max_provider_fee: 10,
            max_routing_fee: 5,
        }
    }

    fn provider_accept_loop_in_params() -> ProviderAcceptLoopInParams {
        ProviderAcceptLoopInParams {
            quote_id: JsonHash256([1u8; 32]),
            lock_tx_hash: JsonHash256([9u8; 32]),
            lock_output_index: 0,
        }
    }

    fn liquidity_swap_response() -> LiquiditySwapResponse {
        LiquiditySwapResponse {
            swap_id: JsonHash256([1u8; 32]),
            state: "payment_settled".to_string(),
            payment_hash: JsonHash256([3u8; 32]),
            payout_outpoint: None,
            created_at: 11,
        }
    }

    fn liquidity_asset_info() -> fiber_json_types::LiquidityAssetInfo {
        fiber_json_types::LiquidityAssetInfo {
            asset_id: "ckb".to_string(),
            kind: fiber_json_types::LiquidityAssetKind::Ckb,
            udt_type_script: None,
            min_amount: 1,
            max_amount: 100,
            available_capacity: 1000,
            base_fee: 2,
            proportional_fee_ppm: 30,
            enabled: true,
        }
    }

    fn liquidity_list_assets_response() -> ListLiquidityAssetsResponse {
        ListLiquidityAssetsResponse {
            assets: vec![liquidity_asset_info()],
        }
    }

    fn liquidity_provider_status() -> LiquidityProviderStatus {
        LiquidityProviderStatus {
            enabled: false,
            enabled_asset_count: 0,
            active_swaps: 0,
        }
    }

    fn set_liquidity_provider_mode_params() -> SetLiquidityProviderModeParams {
        SetLiquidityProviderModeParams { enabled: true }
    }

    fn add_liquidity_asset_params() -> AddLiquidityAssetParams {
        AddLiquidityAssetParams {
            asset: liquidity_asset_info(),
        }
    }

    fn update_liquidity_asset_params() -> UpdateLiquidityAssetParams {
        UpdateLiquidityAssetParams {
            asset: liquidity_asset_info(),
        }
    }

    fn assert_not_placeholder_unavailable(error: Option<ErrorObjectOwned>) {
        if let Some(error) = error {
            let legacy_placeholder = [
                "unavailable until the liquidity actor RPC boundary is ",
                "wired",
            ]
            .concat();

            assert!(!error.to_string().contains(&legacy_placeholder));
        }
    }

    #[test]
    fn liquidity_rpc_methods_are_registered_by_name() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None).into_rpc();
        let methods: HashSet<_> = rpc.method_names().collect();
        let expected: HashSet<_> = liquidity_rpc_method_names().into_iter().collect();

        assert_eq!(methods, expected);
        assert!(methods.contains("import_liquidity_quote"));
        assert!(methods.contains("list_liquidity_chain_transactions"));
    }

    #[tokio::test]
    async fn import_liquidity_quote_rpc_delegates_exact_params_and_returns_exact_envelope() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));
        let params = import_liquidity_quote_params();
        let expected = serde_json::to_value(&params.quote).expect("serialize expected envelope");

        let response = rpc
            .import_liquidity_quote(params)
            .await
            .expect("import liquidity quote");

        assert_eq!(
            serde_json::to_value(response).expect("serialize response envelope"),
            expected
        );
        assert_eq!(actor.take_events(), vec!["import_liquidity_quote"]);
    }

    #[tokio::test]
    async fn import_liquidity_quote_rpc_reports_actor_unavailable() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .import_liquidity_quote(import_liquidity_quote_params())
            .await
            .expect_err("missing actor");

        assert!(error.message().contains("liquidity actor is not available"));
    }

    #[tokio::test]
    async fn import_liquidity_quote_rpc_times_out_when_actor_stalls() {
        let actor = spawn_stalled_liquidity_rpc_mock().await;
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.clone()));

        let error = rpc
            .import_liquidity_quote(import_liquidity_quote_params())
            .await
            .expect_err("stalled actor");

        assert!(error.message().contains("liquidity actor call timed out"));
        actor.stop(None);
    }

    #[tokio::test]
    async fn import_liquidity_quote_rpc_reports_closed_actor_reply() {
        let actor = spawn_closed_reply_liquidity_rpc_mock().await;
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor));

        let error = rpc
            .import_liquidity_quote(import_liquidity_quote_params())
            .await
            .expect_err("closed actor reply");

        assert_eq!(
            error.message(),
            "Messaging failed because channel is closed"
        );
    }

    #[tokio::test]
    async fn liquidity_rpc_get_swap_converts_store_record_to_json_dto() {
        let store = MockLiquidityStore::with_swap(liquidity_rpc_swap());
        let rpc = LiquidityRpcServerImpl::new(store, None);

        let response = rpc
            .get_swap(GetSwapParams {
                swap_id: JsonHash256([1u8; 32]),
            })
            .await
            .expect("get swap")
            .expect("swap");

        assert_eq!(rpc.store.recorded_get_swap_id(), Some([1u8; 32].into()));
        assert_eq!(response.swap_id, JsonHash256([1u8; 32]));
        assert_eq!(
            response.swap_kind,
            fiber_json_types::LiquiditySwapKind::LoopOut
        );
        assert_eq!(response.state, "payment_settled");
        assert_eq!(response.asset_id, "ckb");
        assert_eq!(response.amount, 123);
        assert_eq!(response.payment_hash, JsonHash256([3u8; 32]));
        assert_eq!(response.created_at, 11);
        assert_eq!(response.updated_at, 22);
    }

    #[tokio::test]
    async fn liquidity_rpc_get_swap_maps_loop_in_record_to_json_dto() {
        let mut swap = liquidity_rpc_swap();
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        let store = MockLiquidityStore::with_swap(swap);
        let rpc = LiquidityRpcServerImpl::new(store, None);

        let response = rpc
            .get_swap(GetSwapParams {
                swap_id: JsonHash256([1u8; 32]),
            })
            .await
            .expect("get swap")
            .expect("swap");

        assert_eq!(
            response.swap_kind,
            fiber_json_types::LiquiditySwapKind::LoopIn
        );
    }

    #[tokio::test]
    async fn liquidity_rpc_list_swaps_forwards_filter_and_returns_next_cursor() {
        let store = MockLiquidityStore::with_page(LiquiditySwapPage {
            swaps: vec![liquidity_rpc_swap()],
            next_cursor: Some("next".to_string()),
        });
        let rpc = LiquidityRpcServerImpl::new(store, None);

        let response = rpc
            .list_swaps(ListSwapsParams {
                state: Some("payment_settled".to_string()),
                asset_id: Some("ckb".to_string()),
                limit: Some(10),
                cursor: Some("cursor".to_string()),
            })
            .await
            .expect("list swaps");

        assert_eq!(response.swaps.len(), 1);
        assert_eq!(response.next_cursor, Some("next".to_string()));
        assert_eq!(
            rpc.store.recorded_list_filter(),
            Some(LiquiditySwapFilter {
                state: Some(LiquiditySwapState::PaymentSettled),
                asset_id: Some("ckb".to_string()),
                limit: Some(10),
                cursor: Some("cursor".to_string()),
            })
        );
    }

    #[tokio::test]
    async fn liquidity_rpc_list_swaps_maps_loop_in_records_to_json_dto() {
        let mut swap = liquidity_rpc_swap();
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        let store = MockLiquidityStore::with_page(LiquiditySwapPage {
            swaps: vec![swap],
            next_cursor: None,
        });
        let rpc = LiquidityRpcServerImpl::new(store, None);

        let response = rpc
            .list_swaps(ListSwapsParams {
                state: None,
                asset_id: None,
                limit: None,
                cursor: None,
            })
            .await
            .expect("list swaps");

        assert_eq!(response.swaps.len(), 1);
        assert_eq!(
            response.swaps[0].swap_kind,
            fiber_json_types::LiquiditySwapKind::LoopIn
        );
    }

    #[tokio::test]
    async fn liquidity_rpc_list_swaps_rejects_invalid_state_before_store_call() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .list_swaps(ListSwapsParams {
                state: Some("not_a_state".to_string()),
                asset_id: Some("ckb".to_string()),
                limit: Some(10),
                cursor: Some("cursor".to_string()),
            })
            .await
            .expect_err("invalid state");

        assert!(error.message().contains("invalid liquidity swap state"));
        assert_eq!(rpc.store.recorded_list_filter(), None);
    }

    #[tokio::test]
    async fn loop_out_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc.loop_out(loop_out_params()).await.expect("loop out");

        assert_eq!(response.swap_id, JsonHash256([1u8; 32]));
        assert_eq!(actor.take_events(), vec!["loop_out"]);
    }

    #[tokio::test]
    async fn quote_loop_in_rpc_delegates_to_actor_when_runtime_available() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .quote_loop_in(quote_loop_in_params())
            .await
            .expect("quote loop in");

        assert_eq!(response.quote_id, JsonHash256([1u8; 32]));
        assert_eq!(actor.take_events(), vec!["quote_loop_in"]);
    }

    #[tokio::test]
    async fn provider_quote_loop_out_rpc_returns_complete_envelope() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .provider_quote_loop_out(provider_quote_loop_out_params())
            .await
            .expect("provider quote loop out");

        assert_eq!(
            serde_json::to_value(response).expect("serialize response envelope"),
            serde_json::to_value(liquidity_quote_envelope()).expect("serialize expected envelope")
        );
        assert_eq!(actor.take_events(), vec!["provider_quote_loop_out"]);
    }

    #[tokio::test]
    async fn quote_loop_in_rpc_returns_complete_envelope() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .quote_loop_in(quote_loop_in_params())
            .await
            .expect("quote loop in");

        assert_eq!(
            serde_json::to_value(response).expect("serialize response envelope"),
            serde_json::to_value(liquidity_quote_envelope()).expect("serialize expected envelope")
        );
        assert_eq!(actor.take_events(), vec!["quote_loop_in"]);
    }

    #[tokio::test]
    async fn loop_in_rpc_delegates_to_actor_when_runtime_available() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc.loop_in(loop_in_params()).await.expect("loop in");

        assert_eq!(response.swap_id, JsonHash256([1u8; 32]));
        assert_eq!(actor.take_events(), vec!["loop_in"]);
    }

    #[tokio::test]
    async fn quote_loop_in_rpc_times_out_when_actor_stalls() {
        let actor = spawn_stalled_liquidity_rpc_mock().await;
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor));

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            rpc.quote_loop_in(quote_loop_in_params()),
        )
        .await
        .expect("rpc call should time out internally");
        let error = result.expect_err("stalled actor");

        assert!(error.message().contains("liquidity actor call timed out"));
    }

    #[tokio::test]
    async fn loop_in_rpc_times_out_when_actor_stalls() {
        let actor = spawn_stalled_liquidity_rpc_mock().await;
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor));

        let result =
            tokio::time::timeout(Duration::from_millis(100), rpc.loop_in(loop_in_params()))
                .await
                .expect("rpc call should time out internally");
        let error = result.expect_err("stalled actor");

        assert!(error.message().contains("liquidity actor call timed out"));
    }

    #[tokio::test]
    async fn liquidity_mutation_rpcs_no_longer_return_placeholder_unavailable_errors() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        assert_not_placeholder_unavailable(rpc.quote_loop_out(quote_loop_out_params()).await.err());
        assert_not_placeholder_unavailable(rpc.loop_out(loop_out_params()).await.err());
        assert_not_placeholder_unavailable(rpc.quote_loop_in(quote_loop_in_params()).await.err());
        assert_not_placeholder_unavailable(rpc.loop_in(loop_in_params()).await.err());
        assert_not_placeholder_unavailable(
            rpc.provider_quote_loop_out(provider_quote_loop_out_params())
                .await
                .err(),
        );
        assert_not_placeholder_unavailable(
            rpc.provider_accept_loop_out(provider_accept_loop_out_params())
                .await
                .err(),
        );
        assert_not_placeholder_unavailable(
            rpc.provider_accept_loop_in(provider_accept_loop_in_params())
                .await
                .err(),
        );

        assert_eq!(
            actor.take_events(),
            vec![
                "quote_loop_out",
                "loop_out",
                "quote_loop_in",
                "loop_in",
                "provider_quote_loop_out",
                "provider_accept_loop_out",
                "provider_accept_loop_in",
            ]
        );
    }

    #[tokio::test]
    async fn liquidity_mutation_rpcs_report_actor_unavailable_when_missing() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .loop_out(loop_out_params())
            .await
            .expect_err("missing actor");

        assert!(error.message().contains("liquidity actor is not available"));
    }

    #[tokio::test]
    async fn loop_in_rpc_fails_closed_without_liquidity_actor() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .loop_in(loop_in_params())
            .await
            .expect_err("missing actor");

        assert!(error.message().contains("liquidity actor is not available"));
    }

    #[tokio::test]
    async fn add_liquidity_asset_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .add_liquidity_asset(add_liquidity_asset_params())
            .await
            .expect("add liquidity asset");

        assert_eq!(response.asset_id, "ckb");
        assert_eq!(actor.take_events(), vec!["add_liquidity_asset"]);
    }

    #[tokio::test]
    async fn update_liquidity_asset_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .update_liquidity_asset(update_liquidity_asset_params())
            .await
            .expect("update liquidity asset");

        assert_eq!(response.asset_id, "ckb");
        assert_eq!(actor.take_events(), vec!["update_liquidity_asset"]);
    }

    #[tokio::test]
    async fn disable_liquidity_asset_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .disable_liquidity_asset("ckb".to_string())
            .await
            .expect("disable liquidity asset");

        assert_eq!(response.asset_id, "ckb");
        assert_eq!(actor.take_events(), vec!["disable_liquidity_asset"]);
    }

    #[tokio::test]
    async fn list_liquidity_assets_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .list_liquidity_assets()
            .await
            .expect("list liquidity assets");

        assert_eq!(response.assets.len(), 1);
        assert_eq!(actor.take_events(), vec!["list_liquidity_assets"]);
    }

    #[tokio::test]
    async fn get_liquidity_provider_status_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .get_liquidity_provider_status()
            .await
            .expect("get liquidity provider status");

        assert_eq!(response.active_swaps, 0);
        assert_eq!(actor.take_events(), vec!["get_liquidity_provider_status"]);
    }

    #[tokio::test]
    async fn set_liquidity_provider_mode_rpc_delegates_exact_params_and_returns_status() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .set_liquidity_provider_mode(set_liquidity_provider_mode_params())
            .await
            .expect("set liquidity provider mode");

        assert!(response.enabled);
        assert_eq!(actor.take_events(), vec!["set_liquidity_provider_mode"]);
    }

    #[tokio::test]
    async fn set_liquidity_provider_mode_rpc_reports_actor_unavailable() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .set_liquidity_provider_mode(set_liquidity_provider_mode_params())
            .await
            .expect_err("missing actor");

        assert!(error.message().contains("liquidity actor is not available"));
    }

    #[tokio::test]
    async fn set_liquidity_provider_mode_rpc_times_out_when_actor_stalls() {
        let actor = spawn_stalled_liquidity_rpc_mock().await;
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor));

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            rpc.set_liquidity_provider_mode(set_liquidity_provider_mode_params()),
        )
        .await
        .expect("rpc call should time out internally");
        let error = result.expect_err("stalled actor");

        assert!(error.message().contains("liquidity actor call timed out"));
    }

    #[tokio::test]
    async fn asset_management_rpcs_report_actor_unavailable_when_missing() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .add_liquidity_asset(add_liquidity_asset_params())
            .await
            .expect_err("missing actor");
        assert!(error.message().contains("liquidity actor is not available"));

        let error = rpc
            .list_liquidity_assets()
            .await
            .expect_err("missing actor");
        assert!(error.message().contains("liquidity actor is not available"));

        let error = rpc
            .get_liquidity_provider_status()
            .await
            .expect_err("missing actor");
        assert!(error.message().contains("liquidity actor is not available"));
    }

    #[tokio::test]
    async fn liquidity_read_rpcs_use_store_without_actor() {
        let store = MockLiquidityStore::with_page(LiquiditySwapPage {
            swaps: vec![liquidity_rpc_swap()],
            next_cursor: None,
        });
        let rpc = LiquidityRpcServerImpl::new(store, None);

        let swap = rpc
            .get_swap(GetSwapParams {
                swap_id: JsonHash256([1u8; 32]),
            })
            .await
            .expect("get swap");
        let swaps = rpc
            .list_swaps(ListSwapsParams {
                state: None,
                asset_id: None,
                limit: None,
                cursor: None,
            })
            .await
            .expect("list swaps");

        assert!(swap.is_none());
        assert_eq!(swaps.swaps.len(), 1);
    }

    fn list_liquidity_chain_transactions_params() -> ListLiquidityChainTransactionsParams {
        ListLiquidityChainTransactionsParams {
            swap_id: JsonHash256([1u8; 32]),
        }
    }

    fn liquidity_chain_transactions_response() -> ListLiquidityChainTransactionsResponse {
        ListLiquidityChainTransactionsResponse {
            transactions: vec![LiquidityChainTransaction {
                role: LiquidityChainTransactionRole::LoopInLock,
                tx_hash: JsonHash256([2u8; 32]),
                outpoint: None,
                status: "broadcast".to_string(),
                failure_reason: None,
                created_at: 11,
                updated_at: 12,
            }],
        }
    }

    #[tokio::test]
    async fn list_liquidity_chain_transactions_rpc_delegates_to_actor() {
        let actor = spawn_liquidity_rpc_mock().await;
        let rpc =
            LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

        let response = rpc
            .list_liquidity_chain_transactions(list_liquidity_chain_transactions_params())
            .await
            .expect("list liquidity chain transactions");

        assert_eq!(response.transactions.len(), 1);
        assert_eq!(
            response.transactions[0].role,
            LiquidityChainTransactionRole::LoopInLock
        );
        assert_eq!(
            actor.take_events(),
            vec!["list_liquidity_chain_transactions"]
        );
    }

    #[tokio::test]
    async fn list_liquidity_chain_transactions_rpc_reports_actor_unavailable() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), None);

        let error = rpc
            .list_liquidity_chain_transactions(list_liquidity_chain_transactions_params())
            .await
            .expect_err("missing actor");

        assert!(error.message().contains("liquidity actor is not available"));
    }

    #[tokio::test]
    async fn list_liquidity_chain_transactions_rpc_times_out_when_actor_stalls() {
        let actor = spawn_stalled_liquidity_rpc_mock().await;
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.clone()));

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            rpc.list_liquidity_chain_transactions(list_liquidity_chain_transactions_params()),
        )
        .await
        .expect("rpc call should time out internally");
        let error = result.expect_err("stalled actor");

        assert!(error.message().contains("liquidity actor call timed out"));
        actor.stop(None);
    }
}
