//! Liquidity actor boundary messages and Loop Out client guard helpers.

use std::fmt::Display;

use ckb_types::packed::OutPoint;
use fiber_types::{Hash256, LiquiditySwapState};

use crate::liquidity::store::{
    LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapKind,
    LiquiditySwapRecord, LiquiditySwapRole, LiquiditySwapUpdate,
};
use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};

/// Messages accepted by the liquidity actor boundary.
#[derive(Debug)]
pub enum LiquidityActorMessage {
    /// Request a provider quote for a Loop Out swap.
    QuoteLoopOut(QuoteLoopOutCommand),
    /// Accept a previously issued Loop Out quote.
    AcceptLoopOut(AcceptLoopOutCommand),
    /// Resume orchestration for a persisted Loop Out swap.
    ResumeLoopOut(Hash256),
}

/// Command payload for requesting a Loop Out quote.
#[derive(Debug, Clone)]
pub struct QuoteLoopOutCommand {
    /// Asset identifier to quote.
    pub asset_id: String,
    /// Net amount the client wants to receive on-chain.
    pub amount: u128,
    /// Maximum provider fee the client accepts.
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee the client accepts.
    pub max_routing_fee: u128,
    /// Relative quote expiration requested by the client.
    pub expires_after_seconds: u64,
}

impl QuoteLoopOutCommand {
    /// Return the absolute requested quote expiration timestamp in milliseconds.
    pub fn requested_expiry_ms(&self, now_ms: u64) -> Option<u64> {
        now_ms.checked_add(self.expires_after_seconds.checked_mul(1000)?)
    }
}

/// Command payload for accepting a Loop Out quote.
#[derive(Debug, Clone)]
pub struct AcceptLoopOutCommand {
    /// Provider quote terms being accepted.
    pub quote: LoopOutQuoteTerms,
}

/// Chain boundary required by the provider Loop Out accept workflow.
pub trait LoopOutChainAdapter {
    /// Adapter-specific error returned by chain operations.
    type Error;

    /// Broadcast the payout lock transaction for the accepted quote.
    fn broadcast_payout_lock(&mut self, quote: &LoopOutQuoteTerms)
        -> Result<OutPoint, Self::Error>;
}

/// Accept a provider Loop Out quote and persist restart-safe state before chain broadcast.
pub fn accept_provider_loop_out<S, C>(
    store: &S,
    chain: &mut C,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
) -> Result<Hash256, LiquidityLoopOutError>
where
    S: LiquidityStore,
    C: LoopOutChainAdapter,
    C::Error: Display,
{
    if quote.expires_at <= now_ms {
        return Err(LiquidityLoopOutError::QuoteExpired);
    }

    let swap_id = quote.quote_id;
    store
        .insert_liquidity_swap(LiquiditySwapRecord {
            swap_id,
            quote_id: quote.quote_id,
            role: LiquiditySwapRole::Provider,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: quote.asset.asset_id.clone(),
            state: LiquiditySwapState::Created,
            payment_hash: quote.payment_hash,
            payment_preimage: None,
            amount: quote.amount,
            onchain_outpoint: None,
            payout_deadline: Some(quote.payout_deadline),
            refund_after_lock_time: quote.refund_after_lock_time,
            expires_at: quote.expires_at,
            failure_reason: None,
            created_at: now_ms,
            updated_at: now_ms,
        })
        .map_err(map_store_error)?;

    store
        .update_liquidity_swap_state(
            &swap_id,
            LiquidityStateTransition {
                state: LiquiditySwapState::Quoted,
                updated_at: now_ms,
                reason: None,
            },
        )
        .map_err(map_store_error)?;
    store
        .update_liquidity_swap_state(
            &swap_id,
            LiquidityStateTransition {
                state: LiquiditySwapState::PayoutPending,
                updated_at: now_ms,
                reason: None,
            },
        )
        .map_err(map_store_error)?;

    let onchain_outpoint = chain
        .broadcast_payout_lock(&quote)
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    store
        .update_liquidity_swap(
            &swap_id,
            LiquiditySwapUpdate {
                onchain_outpoint: Some(onchain_outpoint),
                updated_at: now_ms,
                ..Default::default()
            },
        )
        .map_err(map_store_error)?;

    Ok(swap_id)
}

fn map_store_error(error: LiquidityStoreError) -> LiquidityLoopOutError {
    LiquidityLoopOutError::Store(error.to_string())
}

/// Return whether the client may start the Fiber payment from `state`.
pub fn client_can_start_payment(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PayoutLocked
}

/// Return whether the client may claim the payout lock from `state`.
pub fn client_can_claim(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PaymentSettled
}

/// Ensure the client may start the Fiber payment from `state`.
pub fn ensure_client_can_start_payment(
    state: LiquiditySwapState,
) -> Result<(), LiquidityLoopOutError> {
    if client_can_start_payment(state) {
        Ok(())
    } else {
        Err(LiquidityLoopOutError::InvalidStateTransition {
            from: state,
            to: LiquiditySwapState::PaymentInFlight,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use ckb_types::{packed::Byte32, packed::OutPoint, prelude::*};
    use fiber_types::{Hash256, LiquidityAsset, LiquidityAssetKind, LiquiditySwapState, Pubkey};
    use secp256k1::{SecretKey, SECP256K1};

    use super::*;
    use crate::liquidity::store::{
        LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
        LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapUpdate,
    };

    #[derive(Default)]
    struct TestLiquidityStore {
        swaps: RefCell<HashMap<Hash256, LiquiditySwapRecord>>,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl TestLiquidityStore {
        fn events(&self) -> Rc<RefCell<Vec<&'static str>>> {
            self.events.clone()
        }
    }

    impl LiquidityStore for TestLiquidityStore {
        fn insert_liquidity_swap(
            &self,
            swap: LiquiditySwapRecord,
        ) -> Result<(), LiquidityStoreError> {
            self.events.borrow_mut().push("insert_swap");
            self.swaps.borrow_mut().insert(swap.swap_id, swap);
            Ok(())
        }

        fn get_liquidity_swap(
            &self,
            swap_id: &Hash256,
        ) -> Result<Option<LiquiditySwapRecord>, LiquidityStoreError> {
            Ok(self.swaps.borrow().get(swap_id).cloned())
        }

        fn list_liquidity_swaps(
            &self,
            _filter: LiquiditySwapFilter,
        ) -> Result<LiquiditySwapPage, LiquidityStoreError> {
            Ok(LiquiditySwapPage::default())
        }

        fn update_liquidity_swap_state(
            &self,
            swap_id: &Hash256,
            transition: LiquidityStateTransition,
        ) -> Result<(), LiquidityStoreError> {
            let event = match transition.state {
                LiquiditySwapState::Quoted => "transition_quoted",
                LiquiditySwapState::PayoutPending => "transition_payout_pending",
                _ => "transition_other",
            };
            self.events.borrow_mut().push(event);
            let mut swaps = self.swaps.borrow_mut();
            let swap = swaps
                .get_mut(swap_id)
                .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
            swap.state = transition.state;
            swap.updated_at = transition.updated_at;
            swap.failure_reason = transition.reason;
            Ok(())
        }

        fn update_liquidity_swap(
            &self,
            swap_id: &Hash256,
            update: LiquiditySwapUpdate,
        ) -> Result<(), LiquidityStoreError> {
            self.events.borrow_mut().push("persist_outpoint");
            let mut swaps = self.swaps.borrow_mut();
            let swap = swaps
                .get_mut(swap_id)
                .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
            swap.onchain_outpoint = update.onchain_outpoint;
            swap.updated_at = update.updated_at;
            Ok(())
        }

        fn upsert_liquidity_asset(
            &self,
            _asset: LiquidityAsset,
        ) -> Result<(), LiquidityStoreError> {
            Ok(())
        }

        fn get_liquidity_asset(
            &self,
            _asset_id: &str,
        ) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
            Ok(None)
        }

        fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
            Ok(Vec::new())
        }
    }

    struct TestLiquidityChain {
        events: Rc<RefCell<Vec<&'static str>>>,
        outpoint: OutPoint,
    }

    impl TestLiquidityChain {
        fn new(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
            }
        }
    }

    impl LoopOutChainAdapter for TestLiquidityChain {
        type Error = String;

        fn broadcast_payout_lock(
            &mut self,
            _quote: &LoopOutQuoteTerms,
        ) -> Result<OutPoint, Self::Error> {
            self.events.borrow_mut().push("broadcast_payout");
            Ok(self.outpoint.clone())
        }
    }

    fn test_loop_out_quote(expires_at: u64) -> LoopOutQuoteTerms {
        let sk = SecretKey::from_slice(&[42; 32]).unwrap();
        LoopOutQuoteTerms {
            quote_id: [1u8; 32].into(),
            provider: Pubkey::from(sk.public_key(SECP256K1)),
            asset: LiquidityAsset {
                asset_id: "ckb".to_string(),
                kind: LiquidityAssetKind::Ckb,
                udt_type_script: None,
                min_amount: 1,
                max_amount: 1_000,
                available_capacity: 1_000,
                base_fee: 1,
                proportional_fee_ppm: 0,
                enabled: true,
            },
            amount: 100,
            provider_fee: 1,
            routing_fee_limit: 1,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 10_000,
            payment_hash: [2u8; 32].into(),
            expires_at,
            payout_deadline: expires_at + 10_000,
            refund_after_lock_time: expires_at + 20_000,
            claimant_lock: Default::default(),
            refund_lock: Default::default(),
        }
    }

    #[test]
    fn client_payment_requires_payout_locked_state() {
        assert!(client_can_start_payment(LiquiditySwapState::PayoutLocked));
        assert!(!client_can_start_payment(LiquiditySwapState::Quoted));
    }

    #[test]
    fn client_claim_requires_payment_settled_state() {
        assert!(client_can_claim(LiquiditySwapState::PaymentSettled));
        assert!(!client_can_claim(LiquiditySwapState::PaymentInFlight));
    }

    #[test]
    fn ensure_client_payment_reports_invalid_state_transition() {
        assert!(ensure_client_can_start_payment(LiquiditySwapState::PayoutLocked).is_ok());
        assert_eq!(
            ensure_client_can_start_payment(LiquiditySwapState::Quoted),
            Err(LiquidityLoopOutError::InvalidStateTransition {
                from: LiquiditySwapState::Quoted,
                to: LiquiditySwapState::PaymentInFlight,
            })
        );
    }

    #[test]
    fn quote_loop_out_command_requested_expiry_ms_checks_overflow() {
        let command = QuoteLoopOutCommand {
            asset_id: "ckb".to_string(),
            amount: 100,
            max_provider_fee: 1,
            max_routing_fee: 1,
            expires_after_seconds: 60,
        };

        assert_eq!(command.requested_expiry_ms(1_000), Some(61_000));

        let overflowing_command = QuoteLoopOutCommand {
            expires_after_seconds: u64::MAX,
            ..command
        };
        assert_eq!(overflowing_command.requested_expiry_ms(1_000), None);
    }

    #[test]
    fn provider_accept_persists_payout_pending_before_chain_broadcast() {
        let store = TestLiquidityStore::default();
        let mut chain = TestLiquidityChain::new(store.events());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        accept_provider_loop_out(&store, &mut chain, quote, now_ms).unwrap();

        assert_eq!(
            store.events.borrow().as_slice(),
            [
                "insert_swap",
                "transition_quoted",
                "transition_payout_pending",
                "broadcast_payout",
                "persist_outpoint",
            ]
        );
    }

    #[test]
    fn provider_accept_loop_out_rejects_expired_quote_before_order_creation() {
        let store = TestLiquidityStore::default();
        let mut chain = TestLiquidityChain::new(store.events());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms);

        assert_eq!(
            accept_provider_loop_out(&store, &mut chain, quote, now_ms),
            Err(LiquidityLoopOutError::QuoteExpired)
        );
        assert!(store.events.borrow().is_empty());
        assert!(store.swaps.borrow().is_empty());
    }
}
