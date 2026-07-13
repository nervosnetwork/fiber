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

    /// Broadcast the claim transaction for a paid Loop Out swap.
    fn broadcast_claim(&mut self, request: LoopOutClaimRequest) -> Result<(), Self::Error>;
}

/// Chain claim request for a client Loop Out payout.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LoopOutClaimRequest {
    /// Local swap identifier being claimed.
    pub swap_id: Hash256,
    /// Persisted payment preimage required to unlock the claim path.
    pub payment_preimage: Hash256,
}

/// Restart recovery action planned for a persisted Loop Out swap state.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RecoveryAction {
    /// Watch the provider payout lock until it is confirmed or expires.
    WatchPayout,
    /// Resume the client payment after the payout lock is confirmed.
    ResumePayment,
    /// Reload the in-flight payment and reconcile its latest result.
    ReloadPayment,
    /// Broadcast the client claim transaction using the persisted preimage.
    BroadcastClaim,
    /// Watch the claim transaction until it is confirmed.
    WatchClaim,
    /// Refund the provider payout lock after the refund path is available.
    RefundProviderPayout,
}

/// Return the restart recovery action for a persisted Loop Out swap `state`.
pub fn recovery_action_for_loop_out_state(state: LiquiditySwapState) -> Option<RecoveryAction> {
    use LiquiditySwapState::*;

    match state {
        PayoutPending => Some(RecoveryAction::WatchPayout),
        PayoutLocked => Some(RecoveryAction::ResumePayment),
        PaymentInFlight => Some(RecoveryAction::ReloadPayment),
        PaymentSettled => Some(RecoveryAction::BroadcastClaim),
        ClaimPending => Some(RecoveryAction::WatchClaim),
        RefundPending => Some(RecoveryAction::RefundProviderPayout),
        Created | Quoted | OnchainLockPending | OnchainLocked | Success | Failed | Refunded => None,
    }
}

/// Payment boundary required by the client Loop Out execution workflow.
pub trait LoopOutPaymentAdapter {
    /// Adapter-specific error returned by payment operations.
    type Error;

    /// Send the Fiber payment for a Loop Out swap and return the settled payment preimage.
    fn send_loop_out_payment(
        &mut self,
        request: crate::liquidity::payment::LoopOutPaymentRequest,
    ) -> Result<Hash256, Self::Error>;
}

/// Create the client-side Loop Out record and persist quote acceptance before side effects.
pub fn create_client_loop_out<S>(
    store: &S,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
) -> Result<Hash256, LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    if quote.expires_at <= now_ms {
        return Err(LiquidityLoopOutError::QuoteExpired);
    }

    let swap_id = quote.quote_id;
    store
        .insert_liquidity_swap(loop_out_record(&quote, LiquiditySwapRole::Client, now_ms))
        .map_err(map_store_error)?;
    transition_swap(store, &swap_id, LiquiditySwapState::Quoted, now_ms)?;
    transition_swap(store, &swap_id, LiquiditySwapState::PayoutPending, now_ms)?;

    Ok(swap_id)
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

/// Mark the client-side Loop Out payout lock as observed before starting payment.
pub fn mark_client_payout_locked<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PayoutLocked, now_ms)
}

/// Send the client Fiber payment after payout lock persistence and persist the preimage.
pub fn send_client_loop_out_payment<S, P>(
    store: &S,
    payment: &mut P,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
) -> Result<Hash256, LiquidityLoopOutError>
where
    S: LiquidityStore,
    P: LoopOutPaymentAdapter,
    P::Error: Display,
{
    let swap_id = quote.quote_id;
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
        })?;
    ensure_client_can_start_payment(swap.state)?;
    let request = crate::liquidity::payment::LoopOutPaymentRequest::new(
        quote.payment_hash,
        quote.amount,
        quote.provider_fee,
        quote.routing_fee_limit,
    )?;

    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)?;
    let preimage = payment
        .send_loop_out_payment(request)
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?;
    store
        .update_liquidity_swap(
            &swap_id,
            LiquiditySwapUpdate {
                payment_preimage: Some(preimage),
                updated_at: now_ms,
                ..Default::default()
            },
        )
        .map_err(map_store_error)?;
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentSettled, now_ms)?;

    Ok(preimage)
}

/// Claim the on-chain Loop Out payout after payment settlement.
pub fn claim_client_loop_out<S, C>(
    store: &S,
    chain: &mut C,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
    C: LoopOutChainAdapter,
    C::Error: Display,
{
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
        })?;
    if !client_can_claim(swap.state) {
        return Err(LiquidityLoopOutError::InvalidStateTransition {
            from: swap.state,
            to: LiquiditySwapState::ClaimPending,
        });
    }
    let payment_preimage = match swap.payment_preimage {
        Some(payment_preimage) => payment_preimage,
        None => {
            return Err(LiquidityLoopOutError::InvalidStateTransition {
                from: swap.state,
                to: LiquiditySwapState::ClaimPending,
            })
        }
    };

    if swap.state == LiquiditySwapState::PaymentSettled {
        transition_swap(store, &swap_id, LiquiditySwapState::ClaimPending, now_ms)?;
    }
    chain
        .broadcast_claim(LoopOutClaimRequest {
            swap_id,
            payment_preimage,
        })
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    transition_swap(store, &swap_id, LiquiditySwapState::Success, now_ms)
}

fn loop_out_record(
    quote: &LoopOutQuoteTerms,
    role: LiquiditySwapRole,
    now_ms: u64,
) -> LiquiditySwapRecord {
    LiquiditySwapRecord {
        swap_id: quote.quote_id,
        quote_id: quote.quote_id,
        role,
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
    }
}

fn transition_swap<S>(
    store: &S,
    swap_id: &Hash256,
    state: LiquiditySwapState,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    store
        .update_liquidity_swap_state(
            swap_id,
            LiquidityStateTransition {
                state,
                updated_at: now_ms,
                reason: None,
            },
        )
        .map_err(map_store_error)
}

fn map_store_error(error: LiquidityStoreError) -> LiquidityLoopOutError {
    match error {
        LiquidityStoreError::InvalidStateTransition { from, to } => {
            LiquidityLoopOutError::InvalidStateTransition { from, to }
        }
        error => LiquidityLoopOutError::Store(error.to_string()),
    }
}

/// Return whether the client may start the Fiber payment from `state`.
pub fn client_can_start_payment(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PayoutLocked
}

/// Return whether the client may claim or retry claiming the payout lock from `state`.
pub fn client_can_claim(state: LiquiditySwapState) -> bool {
    matches!(
        state,
        LiquiditySwapState::PaymentSettled | LiquiditySwapState::ClaimPending
    )
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

    use crate::liquidity::store::{
        LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
        LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapUpdate,
    };

    use super::*;

    #[derive(Default)]
    struct TestLiquidityStore {
        swaps: RefCell<HashMap<Hash256, LiquiditySwapRecord>>,
        events: Rc<RefCell<Vec<&'static str>>>,
        label: Option<&'static str>,
    }

    impl TestLiquidityStore {
        fn new(events: Rc<RefCell<Vec<&'static str>>>, label: &'static str) -> Self {
            Self {
                swaps: RefCell::new(HashMap::new()),
                events,
                label: Some(label),
            }
        }

        fn events(&self) -> Rc<RefCell<Vec<&'static str>>> {
            self.events.clone()
        }

        fn insert_event(&self, role: LiquiditySwapRole) -> &'static str {
            match (self.label, role) {
                (Some("client"), LiquiditySwapRole::Client) => "client_insert_created",
                (Some("provider"), LiquiditySwapRole::Provider) => "provider_insert_created",
                _ => "insert_swap",
            }
        }

        fn transition_event(
            &self,
            role: LiquiditySwapRole,
            state: LiquiditySwapState,
        ) -> &'static str {
            match (self.label, role, state) {
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapState::Quoted) => {
                    "client_transition_quoted"
                }
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapState::PayoutPending) => {
                    "client_transition_payout_pending"
                }
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapState::PayoutLocked) => {
                    "client_transition_payout_locked"
                }
                (
                    Some("client"),
                    LiquiditySwapRole::Client,
                    LiquiditySwapState::PaymentInFlight,
                ) => "client_transition_payment_in_flight",
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapState::PaymentSettled) => {
                    "client_transition_payment_settled"
                }
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapState::ClaimPending) => {
                    "client_transition_claim_pending"
                }
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapState::Success) => {
                    "client_transition_success"
                }
                (Some("provider"), LiquiditySwapRole::Provider, LiquiditySwapState::Quoted) => {
                    "provider_transition_quoted"
                }
                (
                    Some("provider"),
                    LiquiditySwapRole::Provider,
                    LiquiditySwapState::PayoutPending,
                ) => "provider_transition_payout_pending",
                (_, _, LiquiditySwapState::Quoted) => "transition_quoted",
                (_, _, LiquiditySwapState::PayoutPending) => "transition_payout_pending",
                _ => "transition_other",
            }
        }

        fn update_event(
            &self,
            role: LiquiditySwapRole,
            update: &LiquiditySwapUpdate,
        ) -> &'static str {
            if update.payment_preimage.is_some() {
                return match (self.label, role) {
                    (Some("client"), LiquiditySwapRole::Client) => "client_persist_preimage",
                    _ => "persist_preimage",
                };
            }

            match (self.label, role) {
                (Some("provider"), LiquiditySwapRole::Provider) => "provider_persist_outpoint",
                _ => "persist_outpoint",
            }
        }
    }

    impl LiquidityStore for TestLiquidityStore {
        fn insert_liquidity_swap(
            &self,
            swap: LiquiditySwapRecord,
        ) -> Result<(), LiquidityStoreError> {
            self.events.borrow_mut().push(self.insert_event(swap.role));
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
            let mut swaps = self.swaps.borrow_mut();
            let swap = swaps
                .get_mut(swap_id)
                .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
            if !swap.state.can_transition_to(transition.state) {
                return Err(LiquidityStoreError::InvalidStateTransition {
                    from: swap.state,
                    to: transition.state,
                });
            }
            self.events
                .borrow_mut()
                .push(self.transition_event(swap.role, transition.state));
            swap.state = transition.state;
            swap.updated_at = transition.updated_at;
            if swap.state == LiquiditySwapState::Failed {
                swap.failure_reason = transition.reason;
            }
            Ok(())
        }

        fn update_liquidity_swap(
            &self,
            swap_id: &Hash256,
            update: LiquiditySwapUpdate,
        ) -> Result<(), LiquidityStoreError> {
            let mut swaps = self.swaps.borrow_mut();
            let swap = swaps
                .get_mut(swap_id)
                .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
            self.events
                .borrow_mut()
                .push(self.update_event(swap.role, &update));
            if update.onchain_outpoint.is_some() {
                swap.onchain_outpoint = update.onchain_outpoint;
            }
            if update.payment_preimage.is_some() {
                swap.payment_preimage = update.payment_preimage;
            }
            if update.failure_reason.is_some() {
                swap.failure_reason = update.failure_reason;
            }
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
        label: Option<&'static str>,
        fail_next_claim: bool,
        claim_preimages: Vec<Hash256>,
    }

    impl TestLiquidityChain {
        fn new(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
                label: None,
                fail_next_claim: false,
                claim_preimages: Vec::new(),
            }
        }

        fn new_with_label(events: Rc<RefCell<Vec<&'static str>>>, label: &'static str) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
                label: Some(label),
                fail_next_claim: false,
                claim_preimages: Vec::new(),
            }
        }

        fn fail_next_claim(&mut self) {
            self.fail_next_claim = true;
        }
    }

    impl LoopOutChainAdapter for TestLiquidityChain {
        type Error = String;

        fn broadcast_payout_lock(
            &mut self,
            _quote: &LoopOutQuoteTerms,
        ) -> Result<OutPoint, Self::Error> {
            let event = match self.label {
                Some("chain") => "chain_broadcast_payout",
                _ => "broadcast_payout",
            };
            self.events.borrow_mut().push(event);
            Ok(self.outpoint.clone())
        }

        fn broadcast_claim(&mut self, request: LoopOutClaimRequest) -> Result<(), Self::Error> {
            let event = match self.label {
                Some("chain") => "chain_broadcast_claim",
                _ => "broadcast_claim",
            };
            self.events.borrow_mut().push(event);
            self.claim_preimages.push(request.payment_preimage);
            if self.fail_next_claim {
                self.fail_next_claim = false;
                return Err("claim failed".to_string());
            }
            Ok(())
        }
    }

    struct TestLoopOutPayment {
        events: Rc<RefCell<Vec<&'static str>>>,
        preimage: Hash256,
    }

    impl TestLoopOutPayment {
        fn new(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                events,
                preimage: [4u8; 32].into(),
            }
        }
    }

    impl LoopOutPaymentAdapter for TestLoopOutPayment {
        type Error = String;

        fn send_loop_out_payment(
            &mut self,
            _request: crate::liquidity::payment::LoopOutPaymentRequest,
        ) -> Result<Hash256, Self::Error> {
            self.events.borrow_mut().push("payment_send");
            Ok(self.preimage)
        }
    }

    struct LoopOutActorTestHarness {
        events: Rc<RefCell<Vec<&'static str>>>,
        client_store: TestLiquidityStore,
        provider_store: TestLiquidityStore,
        chain: TestLiquidityChain,
        payment: TestLoopOutPayment,
    }

    impl LoopOutActorTestHarness {
        fn new_with_real_orchestrator() -> Self {
            let events = Rc::new(RefCell::new(Vec::new()));
            Self {
                events: events.clone(),
                client_store: TestLiquidityStore::new(events.clone(), "client"),
                provider_store: TestLiquidityStore::new(events.clone(), "provider"),
                chain: TestLiquidityChain::new_with_label(events.clone(), "chain"),
                payment: TestLoopOutPayment::new(events),
            }
        }

        fn run_happy_path(&mut self) {
            let now_ms = 1_000;
            let quote = test_loop_out_quote(now_ms + 60_000);

            create_client_loop_out(&self.client_store, quote.clone(), now_ms).unwrap();
            accept_provider_loop_out(&self.provider_store, &mut self.chain, quote.clone(), now_ms)
                .unwrap();
            mark_client_payout_locked(&self.client_store, quote.quote_id, now_ms + 1).unwrap();
            send_client_loop_out_payment(
                &self.client_store,
                &mut self.payment,
                quote.clone(),
                now_ms + 2,
            )
            .unwrap();
            claim_client_loop_out(
                &self.client_store,
                &mut self.chain,
                quote.quote_id,
                now_ms + 3,
            )
            .unwrap();
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
    fn recovery_maps_non_terminal_loop_out_states_to_actions() {
        use fiber_types::LiquiditySwapState::*;

        assert_eq!(
            recovery_action_for_loop_out_state(PayoutPending),
            Some(RecoveryAction::WatchPayout)
        );
        assert_eq!(
            recovery_action_for_loop_out_state(PayoutLocked),
            Some(RecoveryAction::ResumePayment)
        );
        assert_eq!(
            recovery_action_for_loop_out_state(PaymentInFlight),
            Some(RecoveryAction::ReloadPayment)
        );
        assert_eq!(
            recovery_action_for_loop_out_state(PaymentSettled),
            Some(RecoveryAction::BroadcastClaim)
        );
        assert_eq!(
            recovery_action_for_loop_out_state(ClaimPending),
            Some(RecoveryAction::WatchClaim)
        );
        assert_eq!(recovery_action_for_loop_out_state(Success), None);
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

    #[test]
    fn loop_out_happy_path_orders_side_effects_after_persistence() {
        let mut harness = LoopOutActorTestHarness::new_with_real_orchestrator();

        harness.run_happy_path();

        assert_eq!(
            harness.events.borrow().as_slice(),
            vec![
                "client_insert_created",
                "client_transition_quoted",
                "client_transition_payout_pending",
                "provider_insert_created",
                "provider_transition_quoted",
                "provider_transition_payout_pending",
                "chain_broadcast_payout",
                "provider_persist_outpoint",
                "client_transition_payout_locked",
                "client_transition_payment_in_flight",
                "payment_send",
                "client_persist_preimage",
                "client_transition_payment_settled",
                "client_transition_claim_pending",
                "chain_broadcast_claim",
                "client_transition_success",
            ]
        );
        assert_eq!(harness.chain.claim_preimages, [[4u8; 32].into()]);
    }

    #[test]
    fn loop_out_client_claim_requires_persisted_preimage_before_broadcast() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms + 1).unwrap();
        transition_swap(
            &store,
            &quote.quote_id,
            LiquiditySwapState::PaymentInFlight,
            now_ms + 2,
        )
        .unwrap();
        transition_swap(
            &store,
            &quote.quote_id,
            LiquiditySwapState::PaymentSettled,
            now_ms + 3,
        )
        .unwrap();
        events.borrow_mut().clear();

        assert_eq!(
            claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4),
            Err(LiquidityLoopOutError::InvalidStateTransition {
                from: LiquiditySwapState::PaymentSettled,
                to: LiquiditySwapState::ClaimPending,
            })
        );

        assert!(events.borrow().is_empty());
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentSettled
        );
    }

    #[test]
    fn loop_out_client_claim_retries_after_transient_chain_failure() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms + 1).unwrap();
        transition_swap(
            &store,
            &quote.quote_id,
            LiquiditySwapState::PaymentInFlight,
            now_ms + 2,
        )
        .unwrap();
        store
            .update_liquidity_swap(
                &quote.quote_id,
                LiquiditySwapUpdate {
                    payment_preimage: Some([4u8; 32].into()),
                    updated_at: now_ms + 3,
                    ..Default::default()
                },
            )
            .unwrap();
        transition_swap(
            &store,
            &quote.quote_id,
            LiquiditySwapState::PaymentSettled,
            now_ms + 3,
        )
        .unwrap();
        events.borrow_mut().clear();
        chain.fail_next_claim();

        assert_eq!(
            claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4),
            Err(LiquidityLoopOutError::Chain("claim failed".to_string()))
        );
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::ClaimPending
        );

        claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 5).unwrap();

        assert_eq!(
            events.borrow().as_slice(),
            [
                "client_transition_claim_pending",
                "chain_broadcast_claim",
                "chain_broadcast_claim",
                "client_transition_success",
            ]
        );
        assert_eq!(
            chain.claim_preimages,
            vec![[4u8; 32].into(), [4u8; 32].into()]
        );
    }

    #[test]
    fn loop_out_payment_request_overflow_does_not_mark_payment_in_flight() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut payment = TestLoopOutPayment::new(events.clone());
        let now_ms = 1_000;
        let quote = LoopOutQuoteTerms {
            amount: u128::MAX,
            provider_fee: 1,
            ..test_loop_out_quote(now_ms + 60_000)
        };

        create_client_loop_out(&store, quote.clone(), now_ms).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms + 1).unwrap();
        events.borrow_mut().clear();

        assert_eq!(
            send_client_loop_out_payment(&store, &mut payment, quote.clone(), now_ms + 2),
            Err(LiquidityLoopOutError::GrossAmountOverflow)
        );

        assert!(events.borrow().is_empty());
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PayoutLocked
        );
    }

    #[test]
    fn loop_out_store_invalid_transition_maps_to_structured_error() {
        assert_eq!(
            map_store_error(LiquidityStoreError::InvalidStateTransition {
                from: LiquiditySwapState::Quoted,
                to: LiquiditySwapState::PayoutLocked,
            }),
            LiquidityLoopOutError::InvalidStateTransition {
                from: LiquiditySwapState::Quoted,
                to: LiquiditySwapState::PayoutLocked,
            }
        );
    }
}
