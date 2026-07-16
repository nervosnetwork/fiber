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

/// Return the JSON-RPC method names exposed by the liquidity module.
pub fn liquidity_rpc_method_names() -> Vec<&'static str> {
    vec![
        "quote_loop_out",
        "loop_out",
        "get_swap",
        "list_swaps",
        "provider_quote_loop_out",
        "provider_accept_loop_out",
    ]
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
    use std::sync::Mutex;

    use fiber_json_types::{GetSwapParams, Hash256 as JsonHash256, ListSwapsParams};
    use fiber_types::{Hash256, LiquidityAsset, LiquiditySwapState};

    use super::*;
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

    #[test]
    fn liquidity_rpc_methods_are_registered_by_name() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default()).into_rpc();
        let methods: HashSet<_> = rpc.method_names().collect();
        let expected: HashSet<_> = liquidity_rpc_method_names().into_iter().collect();

        assert_eq!(methods, expected);
    }

    #[tokio::test]
    async fn liquidity_rpc_get_swap_converts_store_record_to_json_dto() {
        let store = MockLiquidityStore::with_swap(liquidity_rpc_swap());
        let rpc = LiquidityRpcServerImpl::new(store);

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
    async fn liquidity_rpc_list_swaps_forwards_filter_and_returns_next_cursor() {
        let store = MockLiquidityStore::with_page(LiquiditySwapPage {
            swaps: vec![liquidity_rpc_swap()],
            next_cursor: Some("next".to_string()),
        });
        let rpc = LiquidityRpcServerImpl::new(store);

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
    async fn liquidity_rpc_list_swaps_rejects_invalid_state_before_store_call() {
        let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default());

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
}
