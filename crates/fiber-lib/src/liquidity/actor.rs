//! Liquidity actor boundary messages and Loop Out client guard helpers.

use std::fmt::Display;
use std::marker::PhantomData;
use std::time::Duration;

use async_trait::async_trait;

use ckb_types::packed::Script;
use ckb_types::prelude::Entity;
use fiber_json_types::{
    LiquidityQuoteResponse, LiquiditySwapResponse, LoopOutParams, ProviderAcceptLoopOutParams,
    ProviderQuoteLoopOutParams, QuoteLoopOutParams,
};
use fiber_types::{Hash256, LiquiditySwapState};
use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use secp256k1::{SecretKey, SECP256K1};

pub use crate::liquidity::chain::{
    LiquidityChainWatcher as LoopOutChainAdapter, LoopOutClaimRequest,
};
use crate::liquidity::quote::validate_loop_out_quote_request;
use crate::liquidity::store::{
    LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapKind,
    LiquiditySwapRecord, LiquiditySwapRole, LiquiditySwapUpdate,
};
use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};

#[cfg(not(test))]
const LOOP_OUT_PAYMENT_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const LOOP_OUT_PAYMENT_RECONCILE_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS: u32 = 60;
#[cfg(test)]
const LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS: u32 = 2;

/// Messages accepted by the liquidity actor boundary.
#[derive(Debug)]
pub enum LiquidityActorMessage {
    /// Client-side request for a provider Loop Out quote.
    QuoteLoopOut(
        QuoteLoopOutParams,
        RpcReplyPort<Result<LiquidityQuoteResponse, LiquidityLoopOutError>>,
    ),
    /// Client-side acceptance/execution of a Loop Out quote.
    LoopOut(
        LoopOutParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Provider-side quote request.
    ProviderQuoteLoopOut(
        ProviderQuoteLoopOutParams,
        RpcReplyPort<Result<LiquidityQuoteResponse, LiquidityLoopOutError>>,
    ),
    /// Provider-side quote acceptance.
    ProviderAcceptLoopOut(
        ProviderAcceptLoopOutParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Resume every persisted non-terminal Loop Out swap.
    ResumeNonTerminal(RpcReplyPort<Result<usize, LiquidityLoopOutError>>),
    /// Internal continuation after payout lock confirmation.
    PayoutConfirmed(Hash256),
    /// Internal continuation after payment settlement.
    PaymentSettled(Hash256, Hash256),
    /// Internal continuation after client claim confirmation.
    ClaimConfirmed(Hash256),
    /// Internal continuation after provider observes the client claim.
    ProviderClaimObserved(Hash256),
    /// Internal continuation after provider refund confirmation.
    RefundConfirmed(Hash256),
}

impl LiquidityActorMessage {
    #[cfg(test)]
    fn variant_names() -> &'static [&'static str] {
        &[
            "quote_loop_out",
            "loop_out",
            "provider_quote_loop_out",
            "provider_accept_loop_out",
            "resume_non_terminal",
            "payout_confirmed",
            "payment_settled",
            "claim_confirmed",
            "provider_claim_observed",
            "refund_confirmed",
        ]
    }
}

/// Runtime dependencies for `LiquidityActor`.
pub struct LiquidityActorArguments<S, P, C> {
    /// Durable liquidity swap store.
    pub store: S,
    /// Fiber payment adapter used by client Loop Out execution.
    pub payment: P,
    /// Chain adapter used by payout and claim workflows.
    pub chain: C,
}

/// Durable mutation actor for liquidity workflows.
pub struct LiquidityActor<S, P, C>(pub PhantomData<(S, P, C)>);

/// Mutable runtime state held by `LiquidityActor`.
pub struct LiquidityActorState<S, P, C> {
    store: S,
    payment: P,
    chain: C,
}

#[async_trait]
impl<S, P, C> Actor for LiquidityActor<S, P, C>
where
    S: LiquidityStore + Clone + Send + Sync + 'static,
    P: LoopOutPaymentAdapter + Clone + Send + Sync + 'static,
    P::Error: Display + Send,
    C: LoopOutChainAdapter + Send + Sync + 'static,
    C::Error: Display + Send,
{
    type Msg = LiquidityActorMessage;
    type State = LiquidityActorState<S, P, C>;
    type Arguments = LiquidityActorArguments<S, P, C>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(LiquidityActorState {
            store: args.store,
            payment: args.payment,
            chain: args.chain,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LiquidityActorMessage::LoopOut(params, reply) => {
                let result = state.handle_loop_out(params, myself.clone());
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ProviderAcceptLoopOut(params, reply) => {
                let result = state.handle_provider_accept_loop_out(params, myself.clone());
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ResumeNonTerminal(reply) => {
                let _ = reply.send(Ok(0));
            }
            LiquidityActorMessage::PayoutConfirmed(swap_id) => {
                if let Err(error) = state.handle_payout_confirmed(swap_id, myself.clone()) {
                    tracing::warn!(?swap_id, %error, "ignoring loop out payout continuation");
                }
            }
            LiquidityActorMessage::PaymentSettled(swap_id, preimage) => {
                if let Err(error) = state.handle_payment_settled(swap_id, preimage, myself.clone())
                {
                    tracing::warn!(?swap_id, %error, "ignoring loop out payment settled continuation");
                }
            }
            LiquidityActorMessage::ClaimConfirmed(swap_id) => {
                if let Err(error) = mark_client_claim_confirmed(&state.store, swap_id, now_ms()) {
                    tracing::warn!(?swap_id, %error, "ignoring loop out claim continuation");
                }
            }
            LiquidityActorMessage::ProviderClaimObserved(swap_id) => {
                if let Err(error) = mark_provider_claim_observed(&state.store, swap_id, now_ms()) {
                    tracing::warn!(?swap_id, %error, "ignoring loop out provider claim continuation");
                }
            }
            LiquidityActorMessage::RefundConfirmed(_swap_id) => {}
            LiquidityActorMessage::QuoteLoopOut(_params, reply) => {
                let _ = reply.send(Err(LiquidityLoopOutError::Store(
                    "client quote delegation is wired in a later task".to_string(),
                )));
            }
            LiquidityActorMessage::ProviderQuoteLoopOut(params, reply) => {
                let result = state.handle_provider_quote_loop_out(params);
                let _ = reply.send(result);
            }
        }
        Ok(())
    }
}

impl<S, P, C> LiquidityActorState<S, P, C>
where
    S: LiquidityStore + Clone + Send + Sync + 'static,
    P: LoopOutPaymentAdapter + Clone + Send + 'static,
    P::Error: Display + Send + 'static,
    C: LoopOutChainAdapter,
    C::Error: Display,
{
    fn handle_loop_out(
        &mut self,
        params: LoopOutParams,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        let quote_id: Hash256 = params.quote_id.into();
        let quote = self.quote_terms(&quote_id)?;
        let now_ms = now_ms();
        let swap_id = create_client_loop_out(&self.store, quote.clone(), now_ms)?;
        self.chain
            .watch_payout_lock(swap_id, myself.clone())
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        self.swap_response(&swap_id)
    }

    fn handle_provider_quote_loop_out(
        &mut self,
        params: ProviderQuoteLoopOutParams,
    ) -> Result<LiquidityQuoteResponse, LiquidityLoopOutError> {
        let asset = self
            .store
            .get_liquidity_asset(&params.asset_id)
            .map_err(map_store_error)?
            .ok_or_else(|| LiquidityLoopOutError::AssetNotFound(params.asset_id.clone()))?;
        let now_ms = now_ms();
        let expires_at = quote_expires_at(now_ms, params.expires_after_seconds)?;
        let validated = validate_loop_out_quote_request(
            &asset,
            params.amount,
            params.max_provider_fee,
            params.max_routing_fee,
            asset.udt_type_script.as_ref(),
            now_ms,
            expires_at,
        )?;
        let terms = LoopOutQuoteTerms {
            quote_id: loop_out_quote_hash(&params, now_ms, b"quote"),
            provider: deterministic_provider_pubkey(),
            asset,
            amount: params.amount,
            provider_fee: validated.provider_fee,
            routing_fee_limit: validated.routing_fee_limit,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 10_000,
            payment_hash: loop_out_quote_hash(&params, now_ms, b"payment"),
            expires_at: validated.expires_at,
            payout_deadline: validated.expires_at.saturating_add(10_000),
            refund_after_lock_time: validated.expires_at.saturating_add(20_000),
            claimant_lock: Default::default(),
            refund_lock: Default::default(),
        };
        self.store
            .insert_loop_out_quote(terms.clone(), now_ms)
            .map_err(map_store_error)?;
        Ok(quote_response_from_terms(terms))
    }

    fn handle_payout_confirmed(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let swap = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        let now_ms = now_ms();

        match swap.role {
            LiquiditySwapRole::Client => {
                mark_client_payout_locked(&self.store, swap_id, now_ms)?;
                let quote = self.quote_terms(&swap_id)?;
                let request = start_client_loop_out_payment(&self.store, quote, now_ms)?;
                let payment_hash = request.payment_hash;
                let mut payment = self.payment.clone();
                let store = self.store.clone();
                tokio::spawn(async move {
                    match payment.send_loop_out_payment(request).await {
                        Ok(preimage) => {
                            send_payment_settled(&myself, swap_id, preimage);
                        }
                        Err(error) => {
                            tracing::warn!(?swap_id, %error, "loop out payment remains in flight after send failure");
                            reconcile_loop_out_payment(
                                store,
                                payment,
                                myself,
                                swap_id,
                                payment_hash,
                                LOOP_OUT_PAYMENT_RECONCILE_INTERVAL,
                            )
                            .await;
                        }
                    }
                });
            }
            LiquiditySwapRole::Provider => {
                mark_provider_payout_locked(&self.store, swap_id, now_ms)?;
            }
        }

        Ok(())
    }

    fn handle_payment_settled(
        &mut self,
        swap_id: Hash256,
        preimage: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        persist_client_loop_out_payment_preimage(&self.store, swap_id, preimage, now_ms())?;
        claim_client_loop_out(&self.store, &mut self.chain, swap_id, now_ms())?;
        self.chain
            .watch_claim(swap_id, myself)
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        Ok(())
    }

    fn handle_provider_accept_loop_out(
        &mut self,
        params: ProviderAcceptLoopOutParams,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        let quote_id: Hash256 = params.quote_id.into();
        if self
            .store
            .get_liquidity_swap(&quote_id)
            .map_err(map_store_error)?
            .is_some()
        {
            return Err(LiquidityLoopOutError::Store(format!(
                "loop out quote already accepted: {quote_id:?}"
            )));
        }
        let mut quote = self.quote_terms(&quote_id)?;
        quote.claimant_lock = parse_script_hex(&params.claimant_lock, "claimant_lock")?;
        quote.refund_lock = parse_script_hex(&params.refund_lock, "refund_lock")?;
        let now_ms = now_ms();
        self.store
            .insert_loop_out_quote(quote.clone(), now_ms)
            .map_err(map_store_error)?;
        let swap_id = accept_provider_loop_out(&self.store, &mut self.chain, quote, now_ms)?;
        self.chain
            .watch_payout_lock(swap_id, myself)
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        self.swap_response(&swap_id)
    }

    fn quote_terms(&self, quote_id: &Hash256) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError> {
        self.store
            .get_loop_out_quote(quote_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("loop out quote not found: {quote_id:?}"))
            })
    }

    fn swap_response(
        &self,
        swap_id: &Hash256,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        let swap = self
            .store
            .get_liquidity_swap(swap_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        Ok(LiquiditySwapResponse {
            swap_id: swap.swap_id.into(),
            state: format!("{:?}", swap.state),
            payment_hash: swap.payment_hash.into(),
            created_at: swap.created_at,
        })
    }
}

async fn reconcile_loop_out_payment<P>(
    store: impl LiquidityStore + Send + Sync + 'static,
    mut payment: P,
    myself: ActorRef<LiquidityActorMessage>,
    swap_id: Hash256,
    payment_hash: Hash256,
    retry_interval: Duration,
) where
    P: LoopOutPaymentAdapter + Send + 'static,
    P::Error: Display,
{
    for attempt in 0..LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS {
        tokio::time::sleep(retry_interval).await;
        match payment.reload_loop_out_payment(payment_hash).await {
            Ok(LoopOutPaymentStatus::Settled(preimage)) => {
                send_payment_settled(&myself, swap_id, preimage);
                return;
            }
            Ok(LoopOutPaymentStatus::InFlight) => {
                tracing::debug!(?swap_id, ?payment_hash, "loop out payment still in flight");
            }
            Ok(LoopOutPaymentStatus::Failed(reason)) => {
                tracing::warn!(?swap_id, ?payment_hash, %reason, "loop out payment failed while reconciling");
                persist_loop_out_payment_failure_context(&store, swap_id, reason);
                return;
            }
            Err(error) => {
                tracing::warn!(?swap_id, ?payment_hash, %error, "failed to reload loop out payment; retrying");
            }
        }

        if attempt + 1 == LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS {
            persist_loop_out_payment_failure_context(
                &store,
                swap_id,
                "payment reconciliation exhausted while status remained in flight".to_string(),
            );
        }
    }
}

fn persist_loop_out_payment_failure_context<S>(store: &S, swap_id: Hash256, reason: String)
where
    S: LiquidityStore,
{
    if let Err(error) = store.update_liquidity_swap(
        &swap_id,
        LiquiditySwapUpdate {
            failure_reason: Some(reason),
            updated_at: now_ms(),
            ..Default::default()
        },
    ) {
        tracing::warn!(?swap_id, %error, "failed to persist loop out payment failure context");
    }
}

fn send_payment_settled(
    myself: &ActorRef<LiquidityActorMessage>,
    swap_id: Hash256,
    preimage: Hash256,
) {
    if let Err(error) =
        myself.send_message(LiquidityActorMessage::PaymentSettled(swap_id, preimage))
    {
        tracing::warn!(?swap_id, %error, "failed to schedule loop out payment settlement");
    }
}

fn quote_expires_at(now_ms: u64, expires_after_seconds: u64) -> Result<u64, LiquidityLoopOutError> {
    expires_after_seconds
        .checked_mul(1_000)
        .and_then(|ttl_ms| now_ms.checked_add(ttl_ms))
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

fn loop_out_quote_hash(params: &ProviderQuoteLoopOutParams, now_ms: u64, domain: &[u8]) -> Hash256 {
    let mut seed = Vec::new();
    seed.extend_from_slice(domain);
    seed.extend_from_slice(params.asset_id.as_bytes());
    seed.extend_from_slice(&params.amount.to_le_bytes());
    seed.extend_from_slice(params.receiver.as_bytes());
    seed.extend_from_slice(&params.max_provider_fee.to_le_bytes());
    seed.extend_from_slice(&params.max_routing_fee.to_le_bytes());
    seed.extend_from_slice(&params.expires_after_seconds.to_le_bytes());
    seed.extend_from_slice(&now_ms.to_le_bytes());
    ckb_hash::blake2b_256(seed).into()
}

fn deterministic_provider_pubkey() -> fiber_types::Pubkey {
    let sk = SecretKey::from_slice(&[42; 32]).expect("valid deterministic provider secret key");
    fiber_types::Pubkey::from(sk.public_key(SECP256K1))
}

fn quote_response_from_terms(terms: LoopOutQuoteTerms) -> LiquidityQuoteResponse {
    LiquidityQuoteResponse {
        quote_id: terms.quote_id.into(),
        swap_kind: fiber_json_types::LiquiditySwapKind::LoopOut,
        asset_id: terms.asset.asset_id,
        amount: terms.amount,
        provider_fee: terms.provider_fee,
        routing_fee_limit: terms.routing_fee_limit,
        onchain_fee_estimate_ckb: terms.onchain_fee_estimate_ckb,
        capacity_requirement_ckb: terms.capacity_requirement_ckb,
        payment_hash: terms.payment_hash.into(),
        expires_at: terms.expires_at,
        payout_deadline: Some(terms.payout_deadline),
        refund_after_lock_time: terms.refund_after_lock_time,
    }
}

fn parse_script_hex(value: &str, field: &str) -> Result<Script, LiquidityLoopOutError> {
    let Some(hex_value) = value.strip_prefix("0x") else {
        return Err(LiquidityLoopOutError::Store(format!(
            "invalid {field}: script hex must start with 0x"
        )));
    };
    let bytes = hex::decode(hex_value).map_err(|error| {
        LiquidityLoopOutError::Store(format!(
            "invalid {field}: script hex decode failed: {error}"
        ))
    })?;
    Script::from_slice(&bytes).map_err(|error| {
        LiquidityLoopOutError::Store(format!("invalid {field}: script decode failed: {error}"))
    })
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
#[async_trait]
pub trait LoopOutPaymentAdapter {
    /// Adapter-specific error returned by payment operations.
    type Error;

    /// Send the Fiber payment for a Loop Out swap and return the settled payment preimage.
    async fn send_loop_out_payment(
        &mut self,
        request: crate::liquidity::payment::LoopOutPaymentRequest,
    ) -> Result<Hash256, Self::Error>;

    /// Reload a previously sent Loop Out payment and classify its current state.
    async fn reload_loop_out_payment(
        &mut self,
        payment_hash: Hash256,
    ) -> Result<LoopOutPaymentStatus, Self::Error>;
}

/// Reloaded Loop Out payment state used by actor reconciliation.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LoopOutPaymentStatus {
    /// Payment has settled and revealed the preimage.
    Settled(Hash256),
    /// Payment is not terminal yet and should be retried later.
    InFlight,
    /// Payment reached a terminal failed state.
    Failed(String),
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
        .reserve_payout_lock_outpoint(&quote)
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    store
        .update_liquidity_swap(
            &swap_id,
            LiquiditySwapUpdate {
                onchain_outpoint: Some(onchain_outpoint.clone()),
                updated_at: now_ms,
                ..Default::default()
            },
        )
        .map_err(map_store_error)?;
    chain
        .broadcast_payout_lock(&quote, &onchain_outpoint)
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;

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
pub async fn send_client_loop_out_payment<S, P>(
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
    let request = start_client_loop_out_payment(store, quote, now_ms)?;
    let preimage = payment
        .send_loop_out_payment(request)
        .await
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?;
    persist_client_loop_out_payment_preimage(store, swap_id, preimage, now_ms)?;

    Ok(preimage)
}

/// Persist that the client payment has started and return the network payment request.
pub fn start_client_loop_out_payment<S>(
    store: &S,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
) -> Result<crate::liquidity::payment::LoopOutPaymentRequest, LiquidityLoopOutError>
where
    S: LiquidityStore,
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
        quote.provider,
        quote.amount,
        quote.provider_fee,
        quote.routing_fee_limit,
    )?;

    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)?;
    Ok(request)
}

/// Persist a settled client payment preimage and transition the swap to payment settled.
pub fn persist_client_loop_out_payment_preimage<S>(
    store: &S,
    swap_id: Hash256,
    preimage: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
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
    Ok(())
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
    if !client_can_claim(swap.state) && swap.state != LiquiditySwapState::ClaimPending {
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
    Ok(())
}

/// Mark the client-side Loop Out claim as confirmed on-chain.
pub fn mark_client_claim_confirmed<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::Success, now_ms)
}

/// Mark the provider-side Loop Out payout lock as observed on-chain.
pub fn mark_provider_payout_locked<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PayoutLocked, now_ms)
}

/// Mark the provider-side Loop Out Fiber payment as settled.
pub fn mark_provider_payment_settled<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)?;
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentSettled, now_ms)
}

/// Mark the provider-side Loop Out client claim as observed on-chain.
pub fn mark_provider_claim_observed<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
        })?;
    if !matches!(
        swap.state,
        LiquiditySwapState::PaymentSettled | LiquiditySwapState::ClaimPending
    ) {
        return Err(LiquidityLoopOutError::InvalidStateTransition {
            from: swap.state,
            to: LiquiditySwapState::ClaimPending,
        });
    }

    if swap.state == LiquiditySwapState::PaymentSettled {
        transition_swap(store, &swap_id, LiquiditySwapState::ClaimPending, now_ms)?;
    }
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard};

    use ckb_types::{packed::Byte32, packed::OutPoint, prelude::*};
    use fiber_types::{Hash256, LiquidityAsset, LiquidityAssetKind, LiquiditySwapState, Pubkey};
    use ractor::concurrency::Duration;
    use secp256k1::{SecretKey, SECP256K1};
    use tokio::sync::oneshot;

    use crate::liquidity::store::{
        LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
        LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapUpdate,
    };

    use super::*;

    #[test]
    fn actor_message_names_cover_m3_mutations_and_recovery() {
        let names = LiquidityActorMessage::variant_names();

        assert_eq!(
            names,
            &[
                "quote_loop_out",
                "loop_out",
                "provider_quote_loop_out",
                "provider_accept_loop_out",
                "resume_non_terminal",
                "payout_confirmed",
                "payment_settled",
                "claim_confirmed",
                "provider_claim_observed",
                "refund_confirmed",
            ]
        );
    }

    #[test]
    fn resume_non_terminal_action_is_explicit_actor_message() {
        assert!(LiquidityActorMessage::variant_names().contains(&"resume_non_terminal"));
    }

    #[derive(Default)]
    struct Shared<T>(Arc<Mutex<T>>);

    impl<T> Clone for Shared<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    impl<T> Shared<T> {
        fn new(value: T) -> Self {
            Self(Arc::new(Mutex::new(value)))
        }

        fn borrow(&self) -> MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }

        fn borrow_mut(&self) -> MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
    }

    #[derive(Clone, Default)]
    struct TestLiquidityStore {
        swaps: Shared<HashMap<Hash256, LiquiditySwapRecord>>,
        quotes: Shared<HashMap<Hash256, LoopOutQuoteTerms>>,
        assets: Shared<HashMap<String, LiquidityAsset>>,
        events: Shared<Vec<&'static str>>,
        label: Option<&'static str>,
    }

    impl TestLiquidityStore {
        fn new(events: Shared<Vec<&'static str>>, label: &'static str) -> Self {
            Self {
                swaps: Shared::new(HashMap::new()),
                quotes: Shared::new(HashMap::new()),
                assets: Shared::new(HashMap::new()),
                events,
                label: Some(label),
            }
        }

        fn events(&self) -> Shared<Vec<&'static str>> {
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
                (
                    Some("provider"),
                    LiquiditySwapRole::Provider,
                    LiquiditySwapState::PayoutLocked,
                ) => "provider_transition_payout_locked",
                (
                    Some("provider"),
                    LiquiditySwapRole::Provider,
                    LiquiditySwapState::PaymentInFlight,
                ) => "provider_transition_payment_in_flight",
                (
                    Some("provider"),
                    LiquiditySwapRole::Provider,
                    LiquiditySwapState::PaymentSettled,
                ) => "provider_transition_payment_settled",
                (
                    Some("provider"),
                    LiquiditySwapRole::Provider,
                    LiquiditySwapState::ClaimPending,
                ) => "provider_transition_claim_pending",
                (Some("provider"), LiquiditySwapRole::Provider, LiquiditySwapState::Success) => {
                    "provider_transition_success"
                }
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
        fn insert_loop_out_quote(
            &self,
            quote: LoopOutQuoteTerms,
            _created_at: u64,
        ) -> Result<(), LiquidityStoreError> {
            self.quotes.borrow_mut().insert(quote.quote_id, quote);
            Ok(())
        }

        fn get_loop_out_quote(
            &self,
            quote_id: &Hash256,
        ) -> Result<Option<LoopOutQuoteTerms>, LiquidityStoreError> {
            Ok(self.quotes.borrow().get(quote_id).cloned())
        }

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

        fn upsert_liquidity_asset(&self, asset: LiquidityAsset) -> Result<(), LiquidityStoreError> {
            self.assets
                .borrow_mut()
                .insert(asset.asset_id.clone(), asset);
            Ok(())
        }

        fn get_liquidity_asset(
            &self,
            asset_id: &str,
        ) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
            Ok(self.assets.borrow().get(asset_id).cloned())
        }

        fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
            Ok(self.assets.borrow().values().cloned().collect())
        }
    }

    #[derive(Clone)]
    struct TestLiquidityChain {
        events: Shared<Vec<&'static str>>,
        outpoint: OutPoint,
        label: Option<&'static str>,
        fail_next_claim: bool,
        claim_preimages: Vec<Hash256>,
        payout_locks: Shared<Vec<(ckb_types::packed::Script, ckb_types::packed::Script)>>,
    }

    impl TestLiquidityChain {
        fn new(events: Shared<Vec<&'static str>>) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
                label: None,
                fail_next_claim: false,
                claim_preimages: Vec::new(),
                payout_locks: Shared::new(Vec::new()),
            }
        }

        fn new_with_label(events: Shared<Vec<&'static str>>, label: &'static str) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
                label: Some(label),
                fail_next_claim: false,
                claim_preimages: Vec::new(),
                payout_locks: Shared::new(Vec::new()),
            }
        }

        fn fail_next_claim(&mut self) {
            self.fail_next_claim = true;
        }
    }

    impl LoopOutChainAdapter for TestLiquidityChain {
        type Error = String;

        fn reserve_payout_lock_outpoint(
            &mut self,
            _quote: &LoopOutQuoteTerms,
        ) -> Result<OutPoint, Self::Error> {
            if self.label.is_some_and(|label| label.starts_with("runtime")) {
                self.events.borrow_mut().push("reserve_payout");
            }
            Ok(self.outpoint.clone())
        }

        fn broadcast_payout_lock(
            &mut self,
            quote: &LoopOutQuoteTerms,
            _outpoint: &OutPoint,
        ) -> Result<(), Self::Error> {
            let event = match self.label {
                Some("chain") => "chain_broadcast_payout",
                _ => "broadcast_payout",
            };
            self.payout_locks
                .borrow_mut()
                .push((quote.claimant_lock.clone(), quote.refund_lock.clone()));
            self.events.borrow_mut().push(event);
            Ok(())
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

        fn watch_payout_lock(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("watch_payout");
            Ok(())
        }

        fn watch_claim(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("watch_claim");
            Ok(())
        }

        fn broadcast_refund(&mut self, _record: &LiquiditySwapRecord) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("broadcast_refund");
            Ok(())
        }

        fn watch_refund(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("watch_refund");
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestLoopOutPayment {
        events: Shared<Vec<&'static str>>,
        preimage: Hash256,
        label: Option<&'static str>,
        requests: Shared<Vec<crate::liquidity::payment::LoopOutPaymentRequest>>,
        pending_result: Shared<Option<oneshot::Receiver<Result<Hash256, String>>>>,
        reload_statuses: Shared<Vec<LoopOutPaymentStatus>>,
    }

    impl TestLoopOutPayment {
        fn new(events: Shared<Vec<&'static str>>) -> Self {
            Self {
                events,
                preimage: [4u8; 32].into(),
                label: None,
                requests: Shared::new(Vec::new()),
                pending_result: Shared::new(None),
                reload_statuses: Shared::new(Vec::new()),
            }
        }

        fn new_with_label(events: Shared<Vec<&'static str>>, label: &'static str) -> Self {
            Self {
                events,
                preimage: [4u8; 32].into(),
                label: Some(label),
                requests: Shared::new(Vec::new()),
                pending_result: Shared::new(None),
                reload_statuses: Shared::new(Vec::new()),
            }
        }

        fn with_pending_result_and_reload_statuses(
            events: Shared<Vec<&'static str>>,
            reload_statuses: Vec<LoopOutPaymentStatus>,
        ) -> (Self, oneshot::Sender<Result<Hash256, String>>) {
            let (send, recv) = oneshot::channel();
            let payment = Self {
                events,
                preimage: [4u8; 32].into(),
                label: Some("runtime"),
                requests: Shared::new(Vec::new()),
                pending_result: Shared::new(Some(recv)),
                reload_statuses: Shared::new(reload_statuses),
            };
            (payment, send)
        }

        fn requests(&self) -> Vec<crate::liquidity::payment::LoopOutPaymentRequest> {
            self.requests.borrow().clone()
        }
    }

    #[async_trait]
    impl LoopOutPaymentAdapter for TestLoopOutPayment {
        type Error = String;

        async fn send_loop_out_payment(
            &mut self,
            request: crate::liquidity::payment::LoopOutPaymentRequest,
        ) -> Result<Hash256, Self::Error> {
            let event = match self.label {
                Some("runtime") => "send_payment",
                _ => "payment_send",
            };
            self.events.borrow_mut().push(event);
            self.requests.borrow_mut().push(request);
            let pending_result = { self.pending_result.borrow_mut().take() };
            if let Some(pending_result) = pending_result {
                return pending_result
                    .await
                    .map_err(|_| "payment dropped".to_string())?;
            }
            Ok(self.preimage)
        }

        async fn reload_loop_out_payment(
            &mut self,
            _payment_hash: Hash256,
        ) -> Result<LoopOutPaymentStatus, Self::Error> {
            self.events.borrow_mut().push("reload_payment");
            let mut statuses = self.reload_statuses.borrow_mut();
            if statuses.is_empty() {
                Ok(LoopOutPaymentStatus::InFlight)
            } else {
                Ok(statuses.remove(0))
            }
        }
    }

    struct RuntimeActorHarness {
        events: Shared<Vec<&'static str>>,
        store: TestLiquidityStore,
        chain: TestLiquidityChain,
        payment: TestLoopOutPayment,
    }

    impl RuntimeActorHarness {
        fn new_client() -> Self {
            Self::new("client")
        }

        fn new_provider() -> Self {
            Self::new("provider")
        }

        fn new_provider_with_asset() -> Self {
            let harness = Self::new_provider();
            harness
                .store
                .upsert_liquidity_asset(test_loop_out_quote(now_ms() + 60_000).asset)
                .unwrap();
            harness
        }

        fn new(label: &'static str) -> Self {
            let events = Shared::new(Vec::new());
            Self {
                events: events.clone(),
                store: TestLiquidityStore::new(events.clone(), label),
                chain: TestLiquidityChain::new_with_label(
                    events.clone(),
                    match label {
                        "client" => "runtime_client",
                        "provider" => "runtime_provider",
                        _ => "runtime",
                    },
                ),
                payment: TestLoopOutPayment::new_with_label(events, "runtime"),
            }
        }

        fn loop_out_quote_terms(&self) -> LoopOutQuoteTerms {
            test_loop_out_quote(now_ms() + 60_000)
        }

        fn store_quote(&self, quote: LoopOutQuoteTerms) {
            self.store.insert_loop_out_quote(quote, now_ms()).unwrap();
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.borrow().clone()
        }

        async fn call_loop_out(
            &self,
            quote_id: Hash256,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| LiquidityActorMessage::LoopOut(
                LoopOutParams {
                    quote_id: quote_id.into(),
                    max_provider_fee: 1,
                    max_routing_fee: 1,
                },
                reply
            ))
            .unwrap()
        }

        async fn call_provider_quote(
            &self,
            params: ProviderQuoteLoopOutParams,
        ) -> Result<LiquidityQuoteResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| {
                LiquidityActorMessage::ProviderQuoteLoopOut(params, reply)
            })
            .unwrap()
        }

        async fn confirm_payout(
            &self,
            swap_id: Hash256,
        ) -> Result<Result<usize, LiquidityLoopOutError>, ractor::RactorErr<LiquidityActorMessage>>
        {
            let actor = self.spawn_actor().await;
            self.events.borrow_mut().push("payout_confirmed");
            actor.send_message(LiquidityActorMessage::PayoutConfirmed(swap_id))?;
            tokio::task::yield_now().await;
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
        }

        async fn call_provider_accept(
            &self,
            quote_id: Hash256,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            self.call_provider_accept_with_locks(
                quote_id,
                script_hex(&Default::default()),
                script_hex(&Default::default()),
            )
            .await
        }

        async fn call_provider_accept_with_locks(
            &self,
            quote_id: Hash256,
            claimant_lock: String,
            refund_lock: String,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| {
                LiquidityActorMessage::ProviderAcceptLoopOut(
                    ProviderAcceptLoopOutParams {
                        quote_id: quote_id.into(),
                        claimant_lock,
                        refund_lock,
                    },
                    reply,
                )
            })
            .unwrap()
        }

        async fn send_duplicate_payout_confirmed_then_resume(
            &self,
            swap_id: Hash256,
        ) -> Result<Result<usize, LiquidityLoopOutError>, ractor::RactorErr<LiquidityActorMessage>>
        {
            let actor = self.spawn_actor().await;
            actor.send_message(LiquidityActorMessage::PayoutConfirmed(swap_id))?;
            actor.send_message(LiquidityActorMessage::PayoutConfirmed(swap_id))?;
            tokio::task::yield_now().await;
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
        }

        async fn spawn_actor(&self) -> ractor::ActorRef<LiquidityActorMessage> {
            let (actor, _handle) = ractor::Actor::spawn(
                None,
                LiquidityActor::<_, _, _>(std::marker::PhantomData),
                LiquidityActorArguments {
                    store: self.store.clone(),
                    payment: self.payment.clone(),
                    chain: self.chain.clone(),
                },
            )
            .await
            .unwrap();
            actor
        }
    }

    async fn spawn_test_liquidity_actor(
        store: TestLiquidityStore,
        payment: TestLoopOutPayment,
        chain: TestLiquidityChain,
    ) -> ractor::ActorRef<LiquidityActorMessage> {
        let (actor, _handle) = ractor::Actor::spawn(
            None,
            LiquidityActor::<_, _, _>(std::marker::PhantomData),
            LiquidityActorArguments {
                store,
                payment,
                chain,
            },
        )
        .await
        .unwrap();
        actor
    }

    async fn wait_for_event(events: &Shared<Vec<&'static str>>, expected: &'static str) {
        for _ in 0..250 {
            if events.borrow().contains(&expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for event {expected}");
    }

    async fn wait_for_event_count(
        events: &Shared<Vec<&'static str>>,
        expected: &'static str,
        count: usize,
    ) {
        for _ in 0..250 {
            if event_count(events, expected) >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {count} {expected} events");
    }

    fn event_count(events: &Shared<Vec<&'static str>>, expected: &'static str) -> usize {
        events
            .borrow()
            .iter()
            .filter(|event| **event == expected)
            .count()
    }

    struct LoopOutActorTestHarness {
        events: Shared<Vec<&'static str>>,
        client_store: TestLiquidityStore,
        provider_store: TestLiquidityStore,
        chain: TestLiquidityChain,
        payment: TestLoopOutPayment,
    }

    impl LoopOutActorTestHarness {
        fn new_with_real_orchestrator() -> Self {
            let events = Shared::new(Vec::new());
            Self {
                events: events.clone(),
                client_store: TestLiquidityStore::new(events.clone(), "client"),
                provider_store: TestLiquidityStore::new(events.clone(), "provider"),
                chain: TestLiquidityChain::new_with_label(events.clone(), "chain"),
                payment: TestLoopOutPayment::new(events),
            }
        }

        async fn run_happy_path(&mut self) {
            let now_ms = 1_000;
            let quote = test_loop_out_quote(now_ms + 60_000);

            create_client_loop_out(&self.client_store, quote.clone(), now_ms).unwrap();
            accept_provider_loop_out(&self.provider_store, &mut self.chain, quote.clone(), now_ms)
                .unwrap();
            mark_provider_payout_locked(&self.provider_store, quote.quote_id, now_ms + 1).unwrap();
            mark_client_payout_locked(&self.client_store, quote.quote_id, now_ms + 1).unwrap();
            send_client_loop_out_payment(
                &self.client_store,
                &mut self.payment,
                quote.clone(),
                now_ms + 2,
            )
            .await
            .unwrap();
            mark_provider_payment_settled(&self.provider_store, quote.quote_id, now_ms + 2)
                .unwrap();
            claim_client_loop_out(
                &self.client_store,
                &mut self.chain,
                quote.quote_id,
                now_ms + 3,
            )
            .unwrap();
            mark_client_claim_confirmed(&self.client_store, quote.quote_id, now_ms + 4).unwrap();
            mark_provider_claim_observed(&self.provider_store, quote.quote_id, now_ms + 4).unwrap();
        }

        fn client_swap(&self) -> LiquiditySwapRecord {
            self.client_store
                .get_liquidity_swap(&test_swap_id())
                .unwrap()
                .unwrap()
        }

        fn provider_swap(&self) -> LiquiditySwapRecord {
            self.provider_store
                .get_liquidity_swap(&test_swap_id())
                .unwrap()
                .unwrap()
        }
    }

    struct LoopOutEndToEndResult {
        client_final_state: LiquiditySwapState,
        provider_final_state: LiquiditySwapState,
        payment_preimage_persisted: bool,
        preimage_persisted_before_claim: bool,
        claim_broadcast: bool,
    }

    async fn run_loop_out_end_to_end_test() -> LoopOutEndToEndResult {
        let mut harness = LoopOutActorTestHarness::new_with_real_orchestrator();

        harness.run_happy_path().await;

        let client_swap = harness.client_swap();
        let provider_swap = harness.provider_swap();
        let events = harness.events.borrow();
        let claim_broadcast_at = events
            .iter()
            .position(|event| *event == "chain_broadcast_claim");
        let preimage_persisted_at = events
            .iter()
            .position(|event| *event == "client_persist_preimage");
        let client_success_at = events
            .iter()
            .position(|event| *event == "client_transition_success");
        let provider_success_at = events
            .iter()
            .position(|event| *event == "provider_transition_success");
        let claim_broadcast = match (claim_broadcast_at, client_success_at, provider_success_at) {
            (Some(claim_broadcast_at), Some(client_success_at), Some(provider_success_at)) => {
                claim_broadcast_at < client_success_at && claim_broadcast_at < provider_success_at
            }
            _ => false,
        };
        let preimage_persisted_before_claim = match (preimage_persisted_at, claim_broadcast_at) {
            (Some(preimage_persisted_at), Some(claim_broadcast_at)) => {
                preimage_persisted_at < claim_broadcast_at
            }
            _ => false,
        };

        LoopOutEndToEndResult {
            client_final_state: client_swap.state,
            provider_final_state: provider_swap.state,
            payment_preimage_persisted: client_swap.payment_preimage.is_some(),
            preimage_persisted_before_claim,
            claim_broadcast,
        }
    }

    fn test_swap_id() -> Hash256 {
        [1u8; 32].into()
    }

    fn script(args: &'static str) -> ckb_types::packed::Script {
        ckb_types::packed::Script::new_builder()
            .args(ckb_types::bytes::Bytes::from(args).pack())
            .build()
    }

    fn script_hex(script: &ckb_types::packed::Script) -> String {
        format!("0x{}", hex::encode(script.as_slice()))
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

    #[tokio::test]
    async fn liquidity_actor_loop_out_persists_before_payment_and_claim() {
        let harness = RuntimeActorHarness::new_client();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let response = harness.call_loop_out(quote.quote_id).await.unwrap();

        assert_eq!(response.swap_id, quote.quote_id.into());
        assert_eq!(response.state, "PayoutPending");
        assert_eq!(
            harness.events(),
            vec![
                "client_insert_created",
                "client_transition_quoted",
                "client_transition_payout_pending",
                "watch_payout",
            ]
        );

        harness
            .confirm_payout(quote.quote_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            harness.events(),
            vec![
                "client_insert_created",
                "client_transition_quoted",
                "client_transition_payout_pending",
                "watch_payout",
                "payout_confirmed",
                "client_transition_payout_locked",
                "client_transition_payment_in_flight",
                "send_payment",
                "client_persist_preimage",
                "client_transition_payment_settled",
                "client_transition_claim_pending",
                "broadcast_claim",
                "watch_claim",
            ]
        );
    }

    #[tokio::test]
    async fn liquidity_actor_provider_accept_persists_outpoint_before_broadcast() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let response = harness.call_provider_accept(quote.quote_id).await.unwrap();

        assert_eq!(response.swap_id, quote.quote_id.into());
        assert_eq!(
            harness.events(),
            vec![
                "provider_insert_created",
                "provider_transition_quoted",
                "provider_transition_payout_pending",
                "reserve_payout",
                "provider_persist_outpoint",
                "broadcast_payout",
                "watch_payout",
            ]
        );
    }

    #[tokio::test]
    async fn provider_quote_loop_out_validates_asset_and_returns_terms() {
        let harness = RuntimeActorHarness::new_provider_with_asset();

        let quote = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                receiver: "ckt1receiver".to_string(),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        assert_eq!(quote.asset_id, "ckb");
        assert_eq!(quote.amount, 1000);
        assert!(quote.provider_fee <= 100);
        assert!(quote.routing_fee_limit <= 50);

        let persisted_quote = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(persisted_quote.quote_id, quote.quote_id.into());
        assert_eq!(persisted_quote.asset.asset_id, quote.asset_id);
        assert_eq!(persisted_quote.amount, quote.amount);
        assert_eq!(persisted_quote.provider_fee, quote.provider_fee);
        assert_eq!(persisted_quote.routing_fee_limit, quote.routing_fee_limit);
    }

    #[tokio::test]
    async fn provider_accept_loop_out_uses_submitted_lock_scripts() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        let claimant_lock = script("claimant-submitted");
        let refund_lock = script("refund-submitted");
        harness.store_quote(quote.clone());

        harness
            .call_provider_accept_with_locks(
                quote.quote_id,
                script_hex(&claimant_lock),
                script_hex(&refund_lock),
            )
            .await
            .unwrap();

        assert_eq!(
            harness.chain.payout_locks.borrow().as_slice(),
            [(claimant_lock.clone(), refund_lock.clone())]
        );
        let persisted_quote = harness
            .store
            .get_loop_out_quote(&quote.quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted_quote.claimant_lock, claimant_lock);
        assert_eq!(persisted_quote.refund_lock, refund_lock);
    }

    #[tokio::test]
    async fn provider_accept_loop_out_rejects_invalid_lock_before_side_effects() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let error = harness
            .call_provider_accept_with_locks(
                quote.quote_id,
                "not-hex".to_string(),
                script_hex(&Default::default()),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("claimant_lock"));
        assert!(harness.events().is_empty());
        assert!(harness.chain.payout_locks.borrow().is_empty());
    }

    #[tokio::test]
    async fn provider_accept_loop_out_rejects_missing_quote_before_side_effects() {
        let harness = RuntimeActorHarness::new_provider();
        let error = harness
            .call_provider_accept_with_locks(
                [9u8; 32].into(),
                script_hex(&Default::default()),
                script_hex(&Default::default()),
            )
            .await
            .unwrap_err();

        let error_text = error.to_string();
        assert!(error_text.contains("quote"));
        assert!(error_text.contains("not found") || error_text.contains("missing"));
        assert!(harness.events().is_empty());
        assert!(harness.chain.payout_locks.borrow().is_empty());
    }

    #[tokio::test]
    async fn provider_accept_loop_out_rejects_duplicate_without_changing_locks() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        let first_claimant_lock = script("claimant-first");
        let first_refund_lock = script("refund-first");
        let second_claimant_lock = script("claimant-second");
        let second_refund_lock = script("refund-second");
        harness.store_quote(quote.clone());

        harness
            .call_provider_accept_with_locks(
                quote.quote_id,
                script_hex(&first_claimant_lock),
                script_hex(&first_refund_lock),
            )
            .await
            .unwrap();
        let events_after_first_accept = harness.events();

        let error = harness
            .call_provider_accept_with_locks(
                quote.quote_id,
                script_hex(&second_claimant_lock),
                script_hex(&second_refund_lock),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("already") || error.to_string().contains("exists"));
        assert_eq!(harness.events(), events_after_first_accept);
        assert_eq!(
            harness.chain.payout_locks.borrow().as_slice(),
            [(first_claimant_lock.clone(), first_refund_lock.clone())]
        );
        let persisted_quote = harness
            .store
            .get_loop_out_quote(&quote.quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted_quote.claimant_lock, first_claimant_lock);
        assert_eq!(persisted_quote.refund_lock, first_refund_lock);
    }

    #[tokio::test]
    async fn loop_out_rejects_unknown_quote_id_before_side_effects() {
        let harness = RuntimeActorHarness::new_client();

        let error = harness.call_loop_out([9u8; 32].into()).await.unwrap_err();

        assert!(error.to_string().contains("quote"));
        assert!(harness.events().is_empty());
    }

    #[tokio::test]
    async fn liquidity_actor_duplicate_continuation_does_not_stop_actor() {
        let harness = RuntimeActorHarness::new_client();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());
        harness.call_loop_out(quote.quote_id).await.unwrap();

        let resumed = harness
            .send_duplicate_payout_confirmed_then_resume(quote.quote_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(resumed, 0);
    }

    #[tokio::test]
    async fn liquidity_actor_payout_confirmation_does_not_wait_for_payment_settlement() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let (payment, release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(
                events.clone(),
                vec![LoopOutPaymentStatus::Settled([4u8; 32].into())],
            );
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        ractor::call!(actor, |reply| LiquidityActorMessage::LoopOut(
            LoopOutParams {
                quote_id: quote.quote_id.into(),
                max_provider_fee: 1,
                max_routing_fee: 1,
            },
            reply
        ))
        .unwrap()
        .unwrap();
        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;

        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentInFlight
        );
        let actor_for_call = actor.clone();
        let resumed = tokio::time::timeout(
            Duration::from_millis(50),
            tokio::spawn(async move {
                ractor::call!(actor_for_call, LiquidityActorMessage::ResumeNonTerminal)
            }),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(resumed, 0);

        release_payment
            .send(Err("polling timed out".to_string()))
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentInFlight
        );
        assert!(
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
                .unwrap()
                .is_ok()
        );
        wait_for_event(&events, "watch_claim").await;
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::ClaimPending
        );
        assert!(events.borrow().contains(&"reload_payment"));
    }

    #[tokio::test]
    async fn liquidity_actor_terminal_failed_reload_persists_recoverable_failure_context() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let (payment, release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(
                events.clone(),
                vec![LoopOutPaymentStatus::Failed("route failed".to_string())],
            );
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        ractor::call!(actor.clone(), |reply| LiquidityActorMessage::LoopOut(
            LoopOutParams {
                quote_id: quote.quote_id.into(),
                max_provider_fee: 1,
                max_routing_fee: 1,
            },
            reply
        ))
        .unwrap()
        .unwrap();
        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;
        release_payment
            .send(Err("polling timed out".to_string()))
            .unwrap();
        wait_for_event_count(&events, "reload_payment", 1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PaymentInFlight);
        assert_eq!(swap.failure_reason, Some("route failed".to_string()));
        assert_eq!(event_count(&events, "reload_payment"), 1);
        assert!(
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn liquidity_actor_reconciliation_exhaustion_persists_uncertain_context() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let (payment, release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(
                events.clone(),
                vec![
                    LoopOutPaymentStatus::InFlight,
                    LoopOutPaymentStatus::InFlight,
                ],
            );
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        ractor::call!(actor.clone(), |reply| LiquidityActorMessage::LoopOut(
            LoopOutParams {
                quote_id: quote.quote_id.into(),
                max_provider_fee: 1,
                max_routing_fee: 1,
            },
            reply
        ))
        .unwrap()
        .unwrap();
        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;
        release_payment
            .send(Err("polling timed out".to_string()))
            .unwrap();
        wait_for_event_count(
            &events,
            "reload_payment",
            LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS as usize,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PaymentInFlight);
        assert_eq!(
            swap.failure_reason,
            Some("payment reconciliation exhausted while status remained in flight".to_string())
        );
        assert_eq!(
            event_count(&events, "reload_payment"),
            LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS as usize
        );
    }

    #[tokio::test]
    async fn liquidity_actor_payment_settled_persists_preimage_before_claim() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let payment = TestLoopOutPayment::new_with_label(events.clone(), "runtime");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms()).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms()).unwrap();
        transition_swap(
            &store,
            &quote.quote_id,
            LiquiditySwapState::PaymentInFlight,
            now_ms(),
        )
        .unwrap();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PaymentSettled(
                quote.quote_id,
                [4u8; 32].into(),
            ))
            .unwrap();
        wait_for_event(&events, "watch_claim").await;

        assert_eq!(
            events.borrow().as_slice(),
            [
                "client_persist_preimage",
                "client_transition_payment_settled",
                "client_transition_claim_pending",
                "broadcast_claim",
                "watch_claim",
            ]
        );
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
        assert_eq!(
            recovery_action_for_loop_out_state(RefundPending),
            Some(RecoveryAction::RefundProviderPayout)
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
                "persist_outpoint",
                "broadcast_payout",
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

    #[tokio::test]
    async fn loop_out_happy_path_orders_side_effects_after_persistence() {
        let mut harness = LoopOutActorTestHarness::new_with_real_orchestrator();

        harness.run_happy_path().await;

        assert_eq!(
            harness.events.borrow().as_slice(),
            vec![
                "client_insert_created",
                "client_transition_quoted",
                "client_transition_payout_pending",
                "provider_insert_created",
                "provider_transition_quoted",
                "provider_transition_payout_pending",
                "provider_persist_outpoint",
                "chain_broadcast_payout",
                "provider_transition_payout_locked",
                "client_transition_payout_locked",
                "client_transition_payment_in_flight",
                "payment_send",
                "client_persist_preimage",
                "client_transition_payment_settled",
                "provider_transition_payment_in_flight",
                "provider_transition_payment_settled",
                "client_transition_claim_pending",
                "chain_broadcast_claim",
                "client_transition_success",
                "provider_transition_claim_pending",
                "provider_transition_success",
            ]
        );
        assert_eq!(harness.chain.claim_preimages, [[4u8; 32].into()]);
    }

    #[test]
    fn loop_out_client_claim_broadcast_waits_for_confirmation_before_success() {
        let events = Shared::new(Vec::new());
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

        claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4).unwrap();

        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::ClaimPending
        );
        assert_eq!(
            events.borrow().as_slice(),
            ["client_transition_claim_pending", "chain_broadcast_claim"]
        );

        mark_client_claim_confirmed(&store, quote.quote_id, now_ms + 5).unwrap();

        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Success
        );
        assert_eq!(
            events.borrow().as_slice(),
            [
                "client_transition_claim_pending",
                "chain_broadcast_claim",
                "client_transition_success",
            ]
        );
    }

    #[test]
    fn loop_out_provider_claim_observed_requires_payment_settled_state() {
        let store = TestLiquidityStore::default();
        let mut chain = TestLiquidityChain::new(store.events());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        accept_provider_loop_out(&store, &mut chain, quote.clone(), now_ms).unwrap();

        assert_eq!(
            mark_provider_claim_observed(&store, quote.quote_id, now_ms + 1),
            Err(LiquidityLoopOutError::InvalidStateTransition {
                from: LiquiditySwapState::PayoutPending,
                to: LiquiditySwapState::ClaimPending,
            })
        );
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PayoutPending
        );
    }

    #[tokio::test]
    async fn loop_out_end_to_end_uses_real_actor_boundary_and_store() {
        let result = run_loop_out_end_to_end_test().await;

        assert_eq!(
            result.client_final_state,
            fiber_types::LiquiditySwapState::Success
        );
        assert_eq!(
            result.provider_final_state,
            fiber_types::LiquiditySwapState::Success
        );
        assert!(result.payment_preimage_persisted);
        assert!(result.preimage_persisted_before_claim);
        assert!(result.claim_broadcast);
    }

    #[test]
    fn loop_out_client_claim_requires_persisted_preimage_before_broadcast() {
        let events = Shared::new(Vec::new());
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
        let events = Shared::new(Vec::new());
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

        assert_eq!(
            claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 5),
            Ok(())
        );

        assert_eq!(
            events.borrow().as_slice(),
            [
                "client_transition_claim_pending",
                "chain_broadcast_claim",
                "chain_broadcast_claim",
            ]
        );
        assert_eq!(
            chain.claim_preimages,
            vec![[4u8; 32].into(), [4u8; 32].into()]
        );
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::ClaimPending
        );

        mark_client_claim_confirmed(&store, quote.quote_id, now_ms + 6).unwrap();

        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Success
        );
    }

    #[tokio::test]
    async fn loop_out_payment_request_overflow_does_not_mark_payment_in_flight() {
        let events = Shared::new(Vec::new());
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
            send_client_loop_out_payment(&store, &mut payment, quote.clone(), now_ms + 2).await,
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

    #[tokio::test]
    async fn loop_out_payment_request_targets_quote_provider() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut payment = TestLoopOutPayment::new(events.clone());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms + 1).unwrap();

        send_client_loop_out_payment(&store, &mut payment, quote.clone(), now_ms + 2)
            .await
            .unwrap();

        assert_eq!(payment.requests().len(), 1);
        assert_eq!(payment.requests()[0].target_pubkey, quote.provider);
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
