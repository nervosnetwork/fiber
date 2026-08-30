//! Liquidity actor boundary messages and Loop Out client guard helpers.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::marker::PhantomData;
use std::time::Duration;

use async_trait::async_trait;

use ckb_types::packed::OutPoint;
use fiber_json_types::{
    AddLiquidityAssetParams, ImportLiquidityQuoteParams, LiquidityAssetInfo,
    LiquidityChainTransaction, LiquidityChainTransactionRole, LiquidityProviderStatus,
    LiquidityQuoteEnvelope, LiquiditySwapResponse, ListLiquidityAssetsResponse,
    ListLiquidityChainTransactionsParams, ListLiquidityChainTransactionsResponse, LoopInParams,
    LoopOutParams, ProviderAcceptLoopInParams, ProviderAcceptLoopOutParams,
    ProviderQuoteLoopOutParams, QuoteLoopInParams, QuoteLoopOutParams,
    SetLiquidityProviderModeParams, UpdateLiquidityAssetParams,
};
use fiber_types::{
    Hash256, HashAlgorithm, LiquidityChainTxRole, LiquidityChainTxStatus, LiquiditySwapState,
};
use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
#[cfg(test)]
use secp256k1::{SecretKey, SECP256K1};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub use crate::liquidity::chain::{
    LiquidityChainWatcher as LoopOutChainAdapter, LoopOutClaimPlan, LoopOutClaimRequest,
    PayoutValidationError,
};
use crate::liquidity::quote::{
    absolute_timestamp_since, build_loop_in_quote_terms, json_asset_to_liquidity_asset,
    liquidity_asset_to_json_info, liquidity_quote_envelope_from_terms, parse_script_hex,
    validate_imported_quote, validate_loop_out_quote_request,
};
use crate::liquidity::store::{
    LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapKind,
    LiquiditySwapRecord, LiquiditySwapRole, LiquiditySwapUpdate,
};
use crate::liquidity::types::{
    loop_out_payment_principal, LiquidityLoopOutError, LoopOutQuoteTerms,
};

#[cfg(not(test))]
const LOOP_OUT_PAYMENT_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const LOOP_OUT_PAYMENT_RECONCILE_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS: u32 = 60;
#[cfg(test)]
const LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS: u32 = 2;
const PROVIDER_LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS: u32 = 60;
#[cfg(not(test))]
const PAYOUT_VALIDATION_RETRY_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const PAYOUT_VALIDATION_RETRY_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL: Duration = Duration::from_millis(30);
const PAYOUT_VALIDATION_MAX_ATTEMPTS: u32 = 3;

/// Client state that authorizes an exact payout validation attempt.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ClientPayoutAuthorization {
    /// A live payout confirmation authorizes validation while payout is pending.
    PayoutConfirmation,
    /// Restart recovery requires revalidation of a persisted payout lock.
    PayoutLockedRecovery,
}

impl ClientPayoutAuthorization {
    fn expected_state(self) -> LiquiditySwapState {
        match self {
            Self::PayoutConfirmation => LiquiditySwapState::PayoutPending,
            Self::PayoutLockedRecovery => LiquiditySwapState::PayoutLocked,
        }
    }
}

struct AuthorizedClientPayout {
    swap_id: Hash256,
    request: crate::liquidity::payment::LoopOutPaymentRequest,
}

impl AuthorizedClientPayout {
    fn new(quote: LoopOutQuoteTerms) -> Result<Self, LiquidityLoopOutError> {
        let request = crate::liquidity::payment::LoopOutPaymentRequest::new(
            quote.payment_hash,
            quote.provider,
            quote.amount,
            quote.provider_fee,
            quote.routing_fee_limit,
        )?;
        Ok(Self {
            swap_id: quote.quote_id,
            request,
        })
    }
}

/// Messages accepted by the liquidity actor boundary.
#[derive(Debug)]
pub enum LiquidityActorMessage {
    /// Client-side request for a provider Loop Out quote.
    QuoteLoopOut(
        QuoteLoopOutParams,
        RpcReplyPort<Result<LiquidityQuoteEnvelope, LiquidityLoopOutError>>,
    ),
    /// Client-side request for a provider Loop In quote.
    QuoteLoopIn(
        QuoteLoopInParams,
        RpcReplyPort<Result<LiquidityQuoteEnvelope, LiquidityLoopOutError>>,
    ),
    /// Import complete quote terms received from another node.
    ImportLiquidityQuote(
        ImportLiquidityQuoteParams,
        RpcReplyPort<Result<LiquidityQuoteEnvelope, LiquidityLoopOutError>>,
    ),
    /// Client-side acceptance/execution of a Loop Out quote.
    LoopOut(
        LoopOutParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Client-side acceptance/execution of a Loop In quote.
    LoopIn(
        LoopInParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Provider-side quote request.
    ProviderQuoteLoopOut(
        ProviderQuoteLoopOutParams,
        RpcReplyPort<Result<LiquidityQuoteEnvelope, LiquidityLoopOutError>>,
    ),
    /// Provider-side quote acceptance.
    ProviderAcceptLoopOut(
        ProviderAcceptLoopOutParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Provider-side acceptance of an observed Loop In lock.
    ProviderAcceptLoopIn(
        ProviderAcceptLoopInParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Add a provider asset registry entry.
    AddLiquidityAsset(
        AddLiquidityAssetParams,
        RpcReplyPort<Result<LiquidityAssetInfo, LiquidityLoopOutError>>,
    ),
    /// Update a provider asset registry entry.
    UpdateLiquidityAsset(
        UpdateLiquidityAssetParams,
        RpcReplyPort<Result<LiquidityAssetInfo, LiquidityLoopOutError>>,
    ),
    /// Disable a provider asset registry entry.
    DisableLiquidityAsset(
        String,
        RpcReplyPort<Result<LiquidityAssetInfo, LiquidityLoopOutError>>,
    ),
    /// List configured provider assets.
    ListLiquidityAssets(RpcReplyPort<Result<ListLiquidityAssetsResponse, LiquidityLoopOutError>>),
    /// List persisted chain transactions for a swap.
    ListLiquidityChainTransactions(
        ListLiquidityChainTransactionsParams,
        RpcReplyPort<Result<ListLiquidityChainTransactionsResponse, LiquidityLoopOutError>>,
    ),
    /// Return provider status.
    GetLiquidityProviderStatus(
        RpcReplyPort<Result<LiquidityProviderStatus, LiquidityLoopOutError>>,
    ),
    /// Enable or disable provider mode.
    SetLiquidityProviderMode(
        SetLiquidityProviderModeParams,
        RpcReplyPort<Result<LiquidityProviderStatus, LiquidityLoopOutError>>,
    ),
    /// Resume every persisted non-terminal Loop Out swap.
    ResumeNonTerminal(RpcReplyPort<Result<usize, LiquidityLoopOutError>>),
    /// Internal continuation after payout lock confirmation.
    PayoutConfirmed(Hash256),
    /// Internal continuation for retrying a transient payout validation failure.
    RetryPayoutValidation(Hash256, ClientPayoutAuthorization, u32, u64),
    /// Internal continuation after payment settlement.
    PaymentSettled(Hash256, Hash256),
    /// Internal continuation after client claim confirmation.
    ClaimConfirmed(Hash256),
    /// Internal continuation after provider observes the client claim.
    ProviderClaimObserved(Hash256),
    /// Internal continuation after provider refund confirmation.
    RefundConfirmed(Hash256),
    /// Internal continuation after a watched chain transaction is rejected.
    ChainTxRejected(Hash256, LiquidityChainTxRole, String),
    /// Internal continuation after the Loop In client lock is confirmed.
    LoopInLockConfirmed(Hash256),
    /// Internal continuation after a payment recovery task reaches a non-settled terminal point.
    PaymentRecoveryFinished(Hash256),
}

impl LiquidityActorMessage {
    #[cfg(test)]
    fn variant_names() -> &'static [&'static str] {
        &[
            "quote_loop_out",
            "quote_loop_in",
            "import_liquidity_quote",
            "loop_out",
            "loop_in",
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
            "resume_non_terminal",
            "payout_confirmed",
            "retry_payout_validation",
            "payment_settled",
            "claim_confirmed",
            "provider_claim_observed",
            "refund_confirmed",
            "chain_tx_rejected",
            "loop_in_lock_confirmed",
            "payment_recovery_finished",
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
    /// Fiber public key advertised by the serving provider node.
    pub provider_pubkey: fiber_types::Pubkey,
    /// Provider's own recovery lock script derived from the node identity.
    pub provider_funding_lock_script: ckb_types::packed::Script,
}

/// Durable mutation actor for liquidity workflows.
pub struct LiquidityActor<S, P, C>(pub PhantomData<(S, P, C)>);

/// Mutable runtime state held by `LiquidityActor`.
pub struct LiquidityActorState<S, P, C> {
    store: S,
    payment: P,
    chain: C,
    provider_pubkey: fiber_types::Pubkey,
    provider_funding_lock_script: ckb_types::packed::Script,
    watched_payout_swaps: HashSet<Hash256>,
    payout_validation_retries: HashMap<Hash256, u64>,
    definitive_payout_validation_failures: HashSet<Hash256>,
    active_payment_swaps: HashSet<Hash256>,
    watched_claim_swaps: HashSet<Hash256>,
    active_refund_swaps: HashSet<Hash256>,
    job_cancellation: CancellationToken,
    jobs: TaskTracker,
}

impl<S, P, C> Drop for LiquidityActorState<S, P, C> {
    fn drop(&mut self) {
        self.job_cancellation.cancel();
        self.jobs.close();
    }
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
            provider_pubkey: args.provider_pubkey,
            provider_funding_lock_script: args.provider_funding_lock_script,
            watched_payout_swaps: HashSet::new(),
            payout_validation_retries: HashMap::new(),
            definitive_payout_validation_failures: HashSet::new(),
            active_payment_swaps: HashSet::new(),
            watched_claim_swaps: HashSet::new(),
            active_refund_swaps: HashSet::new(),
            job_cancellation: CancellationToken::new(),
            jobs: TaskTracker::new(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LiquidityActorMessage::LoopIn(params, reply) => {
                let result = state.handle_loop_in(params, myself.clone()).await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::LoopOut(params, reply) => {
                let result = state.handle_loop_out(params, myself.clone()).await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ProviderAcceptLoopOut(params, reply) => {
                let result = state
                    .handle_provider_accept_loop_out(params, myself.clone())
                    .await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ProviderAcceptLoopIn(params, reply) => {
                let result = state
                    .handle_provider_accept_loop_in(params, myself.clone())
                    .await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ResumeNonTerminal(reply) => {
                let result = state.resume_non_terminal(myself.clone()).await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::PayoutConfirmed(swap_id) => {
                if state.payout_validation_retries.contains_key(&swap_id)
                    || state
                        .definitive_payout_validation_failures
                        .contains(&swap_id)
                {
                    return Ok(());
                }
                if let Err(error) = state
                    .handle_payout_confirmed(swap_id, 1, myself.clone())
                    .await
                {
                    tracing::warn!(?swap_id, %error, "loop out payout continuation failed operationally; retrying");
                    state.schedule_payout_validation_retry(
                        swap_id,
                        ClientPayoutAuthorization::PayoutConfirmation,
                        1,
                        PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL,
                        myself.clone(),
                    );
                }
            }
            LiquidityActorMessage::RetryPayoutValidation(
                swap_id,
                authorization,
                attempt,
                generation,
            ) => {
                if state.payout_validation_retries.get(&swap_id) != Some(&generation) {
                    return Ok(());
                }
                state
                    .authorize_client_payout_with_retry(
                        swap_id,
                        authorization,
                        attempt,
                        myself.clone(),
                    )
                    .await;
            }
            LiquidityActorMessage::PaymentSettled(swap_id, preimage) => {
                if let Err(error) = state
                    .handle_payment_settled(swap_id, preimage, myself.clone())
                    .await
                {
                    tracing::warn!(?swap_id, %error, "ignoring loop out payment settled continuation");
                }
            }
            LiquidityActorMessage::ClaimConfirmed(swap_id) => {
                if let Err(error) = state.handle_claim_confirmed(swap_id).await {
                    tracing::warn!(?swap_id, %error, "ignoring liquidity claim continuation");
                } else {
                    state.prune_recovery_guards(swap_id);
                }
            }
            LiquidityActorMessage::ProviderClaimObserved(swap_id) => {
                if let Err(error) = mark_provider_claim_observed(&state.store, swap_id, now_ms()) {
                    tracing::warn!(?swap_id, %error, "ignoring loop out provider claim continuation");
                } else {
                    state.prune_recovery_guards(swap_id);
                }
            }
            LiquidityActorMessage::RefundConfirmed(swap_id) => {
                if let Err(error) = state.store.update_liquidity_chain_tx_status(
                    &swap_id,
                    LiquidityChainTxRole::Refund,
                    LiquidityChainTxStatus::Confirmed,
                    None,
                    now_ms(),
                ) {
                    tracing::warn!(?swap_id, %error, "failed to mark loop out refund tx confirmed");
                } else if let Err(error) = mark_refund_confirmed(&state.store, swap_id, now_ms()) {
                    tracing::warn!(?swap_id, %error, "ignoring loop out refund continuation");
                } else {
                    state.prune_recovery_guards(swap_id);
                }
            }
            LiquidityActorMessage::ChainTxRejected(swap_id, role, reason) => {
                match state.store.get_liquidity_chain_tx(&swap_id, role) {
                    Ok(Some(record)) if record.status == LiquidityChainTxStatus::Confirmed => {}
                    Ok(_) => {
                        if let Err(error) = state.store.update_liquidity_chain_tx_status(
                            &swap_id,
                            role,
                            LiquidityChainTxStatus::Rejected,
                            Some(reason),
                            now_ms(),
                        ) {
                            tracing::warn!(?swap_id, ?role, %error, "failed to mark liquidity chain tx rejected");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(?swap_id, ?role, %error, "failed to load liquidity chain tx for rejection");
                    }
                }
                state.prune_recovery_guards(swap_id);
            }
            LiquidityActorMessage::LoopInLockConfirmed(swap_id) => {
                if let Err(error) = state
                    .handle_loop_in_lock_confirmed(swap_id, myself.clone())
                    .await
                {
                    tracing::warn!(?swap_id, %error, "ignoring loop in lock continuation");
                }
            }
            LiquidityActorMessage::PaymentRecoveryFinished(swap_id) => {
                state.active_payment_swaps.remove(&swap_id);
            }
            LiquidityActorMessage::QuoteLoopIn(params, reply) => {
                let result = state.handle_quote_loop_in(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::QuoteLoopOut(params, reply) => {
                let result = state.handle_quote_loop_out(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ImportLiquidityQuote(params, reply) => {
                let result = state.handle_import_liquidity_quote(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ProviderQuoteLoopOut(params, reply) => {
                let result = state.handle_provider_quote_loop_out(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::AddLiquidityAsset(params, reply) => {
                let result = state.handle_add_liquidity_asset(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::UpdateLiquidityAsset(params, reply) => {
                let result = state.handle_update_liquidity_asset(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::DisableLiquidityAsset(asset_id, reply) => {
                let result = state.handle_disable_liquidity_asset(&asset_id);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ListLiquidityAssets(reply) => {
                let result = state.handle_list_liquidity_assets();
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ListLiquidityChainTransactions(params, reply) => {
                let result = state.handle_list_liquidity_chain_transactions(params);
                let _ = reply.send(result);
            }
            LiquidityActorMessage::GetLiquidityProviderStatus(reply) => {
                let result = state.handle_get_liquidity_provider_status();
                let _ = reply.send(result);
            }
            LiquidityActorMessage::SetLiquidityProviderMode(params, reply) => {
                let result = state.handle_set_liquidity_provider_mode(params);
                let _ = reply.send(result);
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.job_cancellation.cancel();
        state.jobs.close();
        state.jobs.wait().await;
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
    fn spawn_job(&self, future: impl Future<Output = ()> + Send + 'static) {
        let cancellation = self.job_cancellation.clone();
        self.jobs.spawn(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {}
                _ = future => {}
            }
        });
    }

    async fn handle_loop_in(
        &mut self,
        params: LoopInParams,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        let quote_id: Hash256 = params.quote_id.into();
        let quote = self.quote_terms(&quote_id)?;
        ensure_loop_in_quote_terms(&quote)?;
        let now_ms = now_ms();
        let swap_id = accept_client_loop_in(
            &self.store,
            &mut self.chain,
            quote,
            params.funding_tx,
            now_ms,
            myself,
        )
        .await?;
        self.loop_in_swap_response(&swap_id)
    }

    async fn handle_loop_out(
        &mut self,
        params: LoopOutParams,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        let quote_id: Hash256 = params.quote_id.into();
        let quote = self.quote_terms(&quote_id)?;
        ensure_loop_out_quote_terms(&quote)?;
        if quote.provider_fee > params.max_provider_fee {
            return Err(LiquidityLoopOutError::ProviderFeeTooHigh);
        }
        if quote.routing_fee_limit > params.max_routing_fee {
            return Err(LiquidityLoopOutError::RoutingFeeTooHigh);
        }
        let now_ms = now_ms();
        let payout_outpoint = params.payout_outpoint.map(Into::into);
        let swap_id = create_client_loop_out(&self.store, quote.clone(), now_ms, payout_outpoint)?;
        self.chain
            .watch_payout_lock(swap_id, myself.clone())
            .await
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        self.swap_response(&swap_id)
    }

    fn handle_provider_quote_loop_out(
        &mut self,
        params: ProviderQuoteLoopOutParams,
    ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
        ensure_provider_mode(&self.store)?;
        let asset = self
            .store
            .get_liquidity_asset(&params.asset_id)
            .map_err(map_store_error)?
            .ok_or_else(|| LiquidityLoopOutError::AssetNotFound(params.asset_id.clone()))?;
        let now_ms = now_ms();
        let expires_at = quote_expires_at(now_ms, params.expires_after_seconds)?;
        let claimant_lock = parse_script_hex(&params.claimant_lock, "claimant_lock")?;
        let validated = validate_loop_out_quote_request(
            &asset,
            params.amount,
            params.max_provider_fee,
            params.max_routing_fee,
            asset.udt_type_script.as_ref(),
            now_ms,
            expires_at,
        )?;
        let preimage = crate::gen_rand_sha256_hash();
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
        let quote_id: Hash256 = crate::gen_rand_sha256_hash();
        let terms = LoopOutQuoteTerms {
            quote_id,
            swap_kind: LiquiditySwapKind::LoopOut,
            provider: self.provider_pubkey,
            asset,
            amount: params.amount,
            provider_fee: validated.provider_fee,
            routing_fee_limit: validated.routing_fee_limit,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 10_000,
            payment_hash,
            payment_preimage: Some(preimage),
            expires_at: validated.expires_at,
            payout_deadline: validated.expires_at.saturating_add(10_000),
            refund_after_lock_time: absolute_timestamp_since(
                validated.expires_at.saturating_add(20_000),
            )?,
            claimant_lock,
            refund_lock: self.provider_funding_lock_script.clone(),
            client_invoice: None,
        };
        self.store
            .insert_loop_out_quote(terms.clone(), now_ms)
            .map_err(map_store_error)?;
        Ok(liquidity_quote_envelope_from_terms(&terms))
    }

    fn handle_quote_loop_in(
        &mut self,
        params: QuoteLoopInParams,
    ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
        ensure_provider_mode(&self.store)?;
        let asset = self
            .store
            .get_liquidity_asset(&params.asset_id)
            .map_err(map_store_error)?
            .ok_or_else(|| LiquidityLoopOutError::AssetNotFound(params.asset_id.clone()))?;
        let now_ms = now_ms();
        let expires_at = quote_expires_at(now_ms, params.expires_after_seconds)?;
        let mut terms = build_loop_in_quote_terms(
            loop_in_quote_hash(&params, now_ms, b"quote"),
            self.provider_pubkey,
            &asset,
            params.amount,
            asset.udt_type_script.as_ref(),
            params.client_invoice,
            params.max_routing_fee,
            expires_at,
            1_000,
        )?;
        terms.claimant_lock = self.provider_funding_lock_script.clone();
        terms.refund_lock = parse_script_hex(&params.refund_lock, "refund_lock")?;
        if terms.provider_fee > params.max_provider_fee {
            return Err(LiquidityLoopOutError::ProviderFeeTooHigh);
        }
        self.store
            .insert_loop_out_quote(terms.clone(), now_ms)
            .map_err(map_store_error)?;
        Ok(liquidity_quote_envelope_from_terms(&terms))
    }

    fn handle_quote_loop_out(
        &mut self,
        params: QuoteLoopOutParams,
    ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
        // No remote provider client is wired here; quote against the local provider registry.
        let QuoteLoopOutParams {
            provider: _,
            asset_id,
            amount,
            claimant_lock,
            max_provider_fee,
            max_routing_fee,
            expires_after_seconds,
        } = params;

        self.handle_provider_quote_loop_out(ProviderQuoteLoopOutParams {
            asset_id,
            amount,
            claimant_lock,
            max_provider_fee,
            max_routing_fee,
            expires_after_seconds,
        })
    }

    fn handle_import_liquidity_quote(
        &mut self,
        params: ImportLiquidityQuoteParams,
    ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
        let now_ms = now_ms();
        let terms = validate_imported_quote(
            params.quote,
            params.max_provider_fee,
            params.max_routing_fee,
            now_ms,
        )?;
        let existing = self
            .store
            .get_loop_out_quote(&terms.quote_id)
            .map_err(map_store_error)?;

        match existing {
            Some(existing) if existing == terms => {
                Ok(liquidity_quote_envelope_from_terms(&existing))
            }
            Some(_) => Err(LiquidityLoopOutError::Store(format!(
                "liquidity quote import conflict for quote ID {:?}",
                terms.quote_id
            ))),
            None => {
                self.store
                    .insert_loop_out_quote(terms.clone(), now_ms)
                    .map_err(map_store_error)?;
                Ok(liquidity_quote_envelope_from_terms(&terms))
            }
        }
    }

    fn handle_add_liquidity_asset(
        &mut self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, LiquidityLoopOutError> {
        let asset = params.asset;
        if self
            .store
            .get_liquidity_asset(&asset.asset_id)
            .map_err(map_store_error)?
            .is_some()
        {
            return Err(LiquidityLoopOutError::Store(format!(
                "asset {} already exists",
                asset.asset_id
            )));
        }
        let liquidity_asset = json_asset_to_liquidity_asset(&asset)?;
        self.store
            .upsert_liquidity_asset(liquidity_asset)
            .map_err(map_store_error)?;
        Ok(asset)
    }

    fn handle_update_liquidity_asset(
        &mut self,
        params: UpdateLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, LiquidityLoopOutError> {
        let asset = params.asset;
        if self
            .store
            .get_liquidity_asset(&asset.asset_id)
            .map_err(map_store_error)?
            .is_none()
        {
            return Err(LiquidityLoopOutError::Store(format!(
                "asset {} not found",
                asset.asset_id
            )));
        }
        let liquidity_asset = json_asset_to_liquidity_asset(&asset)?;
        self.store
            .upsert_liquidity_asset(liquidity_asset)
            .map_err(map_store_error)?;
        Ok(asset)
    }

    fn handle_disable_liquidity_asset(
        &mut self,
        asset_id: &str,
    ) -> Result<LiquidityAssetInfo, LiquidityLoopOutError> {
        let mut asset = self
            .store
            .get_liquidity_asset(asset_id)
            .map_err(map_store_error)?
            .ok_or_else(|| LiquidityLoopOutError::Store(format!("asset {} not found", asset_id)))?;
        asset.enabled = false;
        self.store
            .upsert_liquidity_asset(asset.clone())
            .map_err(map_store_error)?;
        Ok(liquidity_asset_to_json_info(&asset))
    }

    fn handle_list_liquidity_assets(
        &self,
    ) -> Result<ListLiquidityAssetsResponse, LiquidityLoopOutError> {
        let assets = self
            .store
            .list_liquidity_assets()
            .map_err(map_store_error)?;
        let assets = assets.iter().map(liquidity_asset_to_json_info).collect();
        Ok(ListLiquidityAssetsResponse { assets })
    }

    fn handle_list_liquidity_chain_transactions(
        &self,
        params: ListLiquidityChainTransactionsParams,
    ) -> Result<ListLiquidityChainTransactionsResponse, LiquidityLoopOutError> {
        let swap_id: Hash256 = params.swap_id.into();
        let Some(swap) = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(map_store_error)?
        else {
            return Ok(ListLiquidityChainTransactionsResponse {
                transactions: Vec::new(),
            });
        };
        let mut transactions: Vec<LiquidityChainTransaction> = self
            .store
            .list_liquidity_chain_txs_by_swap(&swap_id)
            .map_err(map_store_error)?
            .into_iter()
            .map(|record| liquidity_chain_transaction_from_store(record, swap.swap_kind))
            .collect();
        transactions.sort_by_key(|transaction| chain_tx_role_order(transaction.role));
        Ok(ListLiquidityChainTransactionsResponse { transactions })
    }

    fn handle_get_liquidity_provider_status(
        &self,
    ) -> Result<LiquidityProviderStatus, LiquidityLoopOutError> {
        let enabled = self.store.get_provider_mode().map_err(map_store_error)?;
        let enabled_asset_count = self
            .store
            .list_liquidity_assets()
            .map_err(map_store_error)?
            .iter()
            .filter(|asset| asset.enabled)
            .count() as u64;
        let active_swaps = self.count_non_terminal_provider_swaps()?;
        Ok(LiquidityProviderStatus {
            enabled,
            enabled_asset_count,
            active_swaps,
        })
    }

    fn handle_set_liquidity_provider_mode(
        &self,
        params: SetLiquidityProviderModeParams,
    ) -> Result<LiquidityProviderStatus, LiquidityLoopOutError> {
        if self.store.get_provider_mode().map_err(map_store_error)? != params.enabled {
            self.store
                .set_provider_mode(params.enabled)
                .map_err(map_store_error)?;
        }
        self.handle_get_liquidity_provider_status()
    }

    fn count_non_terminal_provider_swaps(&self) -> Result<u64, LiquidityLoopOutError> {
        use LiquiditySwapState::*;
        let loop_out_states = [
            PayoutPending,
            PayoutLocked,
            PaymentInFlight,
            PaymentSettled,
            ClaimPending,
            RefundPending,
        ];
        let loop_in_states = [
            OnchainLockPending,
            OnchainLocked,
            PaymentInFlight,
            PaymentSettled,
            ClaimPending,
            RefundPending,
        ];
        let count = self
            .store
            .list_liquidity_swaps_by_states(&loop_out_states, LiquiditySwapKind::LoopOut)
            .map_err(map_store_error)?
            .iter()
            .filter(|s| s.role == crate::liquidity::store::LiquiditySwapRole::Provider)
            .count()
            + self
                .store
                .list_liquidity_swaps_by_states(&loop_in_states, LiquiditySwapKind::LoopIn)
                .map_err(map_store_error)?
                .iter()
                .filter(|s| s.role == crate::liquidity::store::LiquiditySwapRole::Provider)
                .count();
        Ok(count as u64)
    }

    async fn handle_payout_confirmed(
        &mut self,
        swap_id: Hash256,
        attempt: u32,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let swap = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        if swap.role == LiquiditySwapRole::Client {
            self.authorize_client_payout_with_retry(
                swap_id,
                ClientPayoutAuthorization::PayoutConfirmation,
                attempt,
                myself,
            )
            .await;
            return Ok(());
        }
        let now_ms = now_ms();

        let has_payout_tx = self
            .store
            .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Payout)
            .map_err(map_store_error)?
            .is_some();
        if has_payout_tx {
            self.store
                .update_liquidity_chain_tx_status(
                    &swap_id,
                    LiquidityChainTxRole::Payout,
                    LiquidityChainTxStatus::Confirmed,
                    None,
                    now_ms,
                )
                .map_err(map_store_error)?;
        }

        mark_provider_payout_locked(&self.store, swap_id, now_ms)?;
        if self.active_payment_swaps.contains(&swap_id) {
            return Ok(());
        }
        let quote = self.quote_terms(&swap_id)?;
        let preimage = quote.payment_preimage.ok_or_else(|| {
            LiquidityLoopOutError::Store(format!(
                "provider loop out quote {swap_id:?} is missing its preimage"
            ))
        })?;
        let payment_hash = quote.payment_hash;
        mark_provider_payment_in_flight(&self.store, swap_id, now_ms)?;
        self.active_payment_swaps.insert(swap_id);
        let payment = self.payment.clone();
        let store = self.store.clone();
        let actor = myself.clone();
        self.spawn_job(async move {
            observe_provider_loop_out_payment(
                store,
                payment,
                actor,
                swap_id,
                preimage,
                payment_hash,
            )
            .await;
        });

        Ok(())
    }

    async fn authorize_client_payout_with_retry(
        &mut self,
        swap_id: Hash256,
        authorization: ClientPayoutAuthorization,
        attempt: u32,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> bool {
        match self
            .authorize_client_payout(swap_id, authorization, attempt, myself.clone())
            .await
        {
            Ok(resumed) => resumed,
            Err(error) => {
                tracing::warn!(?swap_id, %error, "loop out payout authorization failed operationally; retrying");
                self.schedule_payout_validation_retry(
                    swap_id,
                    authorization,
                    attempt.min(PAYOUT_VALIDATION_MAX_ATTEMPTS),
                    PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL,
                    myself,
                );
                true
            }
        }
    }

    async fn authorize_client_payout(
        &mut self,
        swap_id: Hash256,
        authorization: ClientPayoutAuthorization,
        attempt: u32,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<bool, LiquidityLoopOutError> {
        let swap = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        if swap.role != LiquiditySwapRole::Client
            || swap.swap_kind != LiquiditySwapKind::LoopOut
            || swap.state != authorization.expected_state()
            || self.active_payment_swaps.contains(&swap_id)
        {
            self.payout_validation_retries.remove(&swap_id);
            return Ok(false);
        }
        if self
            .definitive_payout_validation_failures
            .contains(&swap_id)
        {
            self.payout_validation_retries.remove(&swap_id);
            return Ok(false);
        }

        let now_ms = now_ms();
        let payout_tx = self
            .store
            .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Payout)
            .map_err(map_store_error)?;

        let quote = self.quote_terms(&swap.quote_id)?;
        let validation = match swap.onchain_outpoint.as_ref() {
            Some(outpoint) => {
                self.chain
                    .validate_observed_loop_out_payout(&quote, outpoint)
                    .await
            }
            None => Err(PayoutValidationError::Definitive(
                "payout outpoint is missing".to_string(),
            )),
        };
        match validation {
            Err(PayoutValidationError::Transient(error)) => {
                let bounded_attempt = attempt.min(PAYOUT_VALIDATION_MAX_ATTEMPTS);
                let reason = format!(
                    "temporary loop out payout validation failure (attempt {bounded_attempt}/{PAYOUT_VALIDATION_MAX_ATTEMPTS}): {error}"
                );
                if payout_tx.is_some() {
                    self.store
                        .update_liquidity_chain_tx_status(
                            &swap_id,
                            LiquidityChainTxRole::Payout,
                            LiquidityChainTxStatus::Confirmed,
                            Some(reason),
                            now_ms,
                        )
                        .map_err(map_store_error)?;
                }
                let (next_attempt, delay) = if bounded_attempt < PAYOUT_VALIDATION_MAX_ATTEMPTS {
                    (bounded_attempt + 1, PAYOUT_VALIDATION_RETRY_INTERVAL)
                } else {
                    (
                        PAYOUT_VALIDATION_MAX_ATTEMPTS,
                        PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL,
                    )
                };
                self.schedule_payout_validation_retry(
                    swap_id,
                    authorization,
                    next_attempt,
                    delay,
                    myself,
                );
                return Ok(true);
            }
            Err(PayoutValidationError::Definitive(error)) => {
                let reason = format!("loop out payout validation failed: {error}");
                if payout_tx.is_none() {
                    return Err(LiquidityLoopOutError::Store(format!(
                        "cannot persist definitive payout validation failure without payout chain record: {swap_id:?}"
                    )));
                }
                self.store
                    .update_liquidity_chain_tx_status(
                        &swap_id,
                        LiquidityChainTxRole::Payout,
                        LiquidityChainTxStatus::Confirmed,
                        Some(reason.clone()),
                        now_ms,
                    )
                    .map_err(map_store_error)?;
                self.store
                    .update_liquidity_swap(
                        &swap_id,
                        LiquiditySwapUpdate {
                            failure_reason: Some(reason),
                            updated_at: now_ms,
                            ..Default::default()
                        },
                    )
                    .map_err(map_store_error)?;
                self.definitive_payout_validation_failures.insert(swap_id);
                self.payout_validation_retries.remove(&swap_id);
                self.watched_payout_swaps.remove(&swap_id);
                return Ok(false);
            }
            Ok(()) => {}
        }

        if let Some(payout_tx) = payout_tx {
            if let Some(reason) = payout_tx.failure_reason.as_deref() {
                self.store
                    .clear_liquidity_swap_failure_reason(&swap_id, reason, now_ms)
                    .map_err(map_store_error)?;
            }
            self.store
                .update_liquidity_chain_tx_status(
                    &swap_id,
                    LiquidityChainTxRole::Payout,
                    LiquidityChainTxStatus::Confirmed,
                    None,
                    now_ms,
                )
                .map_err(map_store_error)?;
        }
        if authorization == ClientPayoutAuthorization::PayoutConfirmation {
            mark_client_payout_locked(&self.store, swap_id, now_ms)?;
        }
        self.payout_validation_retries.remove(&swap_id);
        self.watched_payout_swaps.remove(&swap_id);
        let authorized = AuthorizedClientPayout::new(quote)?;
        self.start_client_payment_dispatch(authorized, now_ms, myself)?;
        Ok(true)
    }

    fn schedule_payout_validation_retry(
        &mut self,
        swap_id: Hash256,
        authorization: ClientPayoutAuthorization,
        attempt: u32,
        delay: Duration,
        myself: ActorRef<LiquidityActorMessage>,
    ) {
        let generation = self
            .payout_validation_retries
            .get(&swap_id)
            .copied()
            .unwrap_or_default()
            .checked_add(1)
            .unwrap_or(1);
        self.payout_validation_retries.insert(swap_id, generation);
        self.spawn_job(async move {
            tokio::time::sleep(delay).await;
            if let Err(error) = myself.send_message(LiquidityActorMessage::RetryPayoutValidation(
                swap_id,
                authorization,
                attempt,
                generation,
            )) {
                tracing::warn!(?swap_id, %error, "failed to schedule payout validation retry");
            }
        });
    }

    fn start_client_payment_dispatch(
        &mut self,
        authorized: AuthorizedClientPayout,
        now_ms: u64,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let AuthorizedClientPayout { swap_id, request } = authorized;
        let payment_hash = request.payment_hash;
        self.active_payment_swaps.insert(swap_id);
        let mut payment = self.payment.clone();
        let store = self.store.clone();
        self.spawn_job(async move {
            if let Err(error) = mark_payment_dispatch_started(&store, swap_id, now_ms) {
                tracing::warn!(?swap_id, %error, "failed to persist loop out payment dispatch");
                send_payment_recovery_finished(&myself, swap_id);
                return;
            }
            match payment.send_loop_out_payment(request).await {
                Ok(preimage) => send_payment_settled(&myself, swap_id, preimage),
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
        Ok(())
    }

    async fn handle_payment_settled(
        &mut self,
        swap_id: Hash256,
        preimage: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let result = async {
            let swap = self
                .store
                .get_liquidity_swap(&swap_id)
                .map_err(map_store_error)?
                .ok_or_else(|| {
                    LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
                })?;
            if swap.role == LiquiditySwapRole::Provider
                && swap.swap_kind == LiquiditySwapKind::LoopIn
            {
                LoopOutClaimPlan::validate_payment_preimage(swap.payment_hash, preimage)?;
                persist_provider_loop_in_payment_preimage(
                    &self.store,
                    swap_id,
                    preimage,
                    now_ms(),
                )?;
                self.active_payment_swaps.remove(&swap_id);
                claim_provider_loop_in(
                    &self.store,
                    &mut self.chain,
                    swap_id,
                    now_ms(),
                    myself.clone(),
                )
                .await?;
                self.chain
                    .watch_claim(swap_id, myself)
                    .await
                    .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                self.watched_claim_swaps.insert(swap_id);
                return Ok(());
            }
            if swap.role == LiquiditySwapRole::Provider {
                LoopOutClaimPlan::validate_payment_preimage(swap.payment_hash, preimage)?;
                mark_provider_payment_settled(&self.store, swap_id, now_ms())?;
                if !self.watched_claim_swaps.contains(&swap_id) {
                    self.chain
                        .watch_provider_claim(swap_id, myself)
                        .await
                        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                    self.watched_claim_swaps.insert(swap_id);
                }
                return Ok(());
            }
            LoopOutClaimPlan::validate_payment_preimage(swap.payment_hash, preimage)?;
            persist_client_loop_out_payment_preimage(&self.store, swap_id, preimage, now_ms())?;
            self.active_payment_swaps.remove(&swap_id);
            claim_client_loop_out(
                &self.store,
                &mut self.chain,
                swap_id,
                now_ms(),
                myself.clone(),
            )
            .await?;
            self.chain
                .watch_claim(swap_id, myself)
                .await
                .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
            self.watched_claim_swaps.insert(swap_id);
            Ok(())
        }
        .await;

        if result.is_err() {
            self.active_payment_swaps.remove(&swap_id);
        }
        result
    }

    async fn handle_loop_in_lock_confirmed(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let now = now_ms();
        self.store
            .update_liquidity_chain_tx_status(
                &swap_id,
                LiquidityChainTxRole::Payout,
                LiquidityChainTxStatus::Confirmed,
                None,
                now,
            )
            .map_err(map_store_error)?;
        mark_loop_in_lock_confirmed(&self.store, swap_id, now)?;

        let swap = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        if swap.swap_kind == LiquiditySwapKind::LoopIn && swap.role == LiquiditySwapRole::Provider {
            self.start_provider_loop_in_payment(swap, myself).await?;
        }
        Ok(())
    }

    async fn handle_claim_confirmed(
        &mut self,
        swap_id: Hash256,
    ) -> Result<(), LiquidityLoopOutError> {
        let now = now_ms();
        self.store
            .update_liquidity_chain_tx_status(
                &swap_id,
                LiquidityChainTxRole::Claim,
                LiquidityChainTxStatus::Confirmed,
                None,
                now,
            )
            .map_err(map_store_error)?;
        let swap = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(map_store_error)?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        if swap.swap_kind == LiquiditySwapKind::LoopIn && swap.role == LiquiditySwapRole::Provider {
            mark_loop_in_provider_claim_confirmed(&self.store, swap_id, now)
        } else {
            mark_client_claim_confirmed(&self.store, swap_id, now)
        }
    }

    async fn handle_provider_accept_loop_out(
        &mut self,
        params: ProviderAcceptLoopOutParams,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        ensure_provider_mode(&self.store)?;
        let quote_id: Hash256 = params.quote_id.into();
        if let Some(existing) = self
            .store
            .get_liquidity_swap(&quote_id)
            .map_err(map_store_error)?
        {
            if existing.role != LiquiditySwapRole::Provider
                || existing.swap_kind != LiquiditySwapKind::LoopOut
            {
                return Err(LiquidityLoopOutError::Store(format!(
                    "existing swap conflicts with provider loop out accept for quote {quote_id:?}: expected provider loop out, found {:?} {:?}",
                    existing.role, existing.swap_kind
                )));
            }
            let quote = self.quote_terms(&quote_id)?;
            self.register_provider_loop_out_invoice_for_quote(&quote)
                .await?;
            return self.swap_response(&quote_id);
        }
        let quote = self.quote_terms(&quote_id)?;
        ensure_loop_out_quote_terms(&quote)?;
        let now_ms = now_ms();
        let swap_id =
            accept_provider_loop_out(&self.store, &mut self.chain, quote, now_ms, myself.clone())
                .await?;
        self.chain
            .watch_payout_lock(swap_id, myself)
            .await
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        let quote = self.quote_terms(&quote_id)?;
        self.register_provider_loop_out_invoice_for_quote(&quote)
            .await?;
        self.swap_response(&swap_id)
    }

    async fn register_provider_loop_out_invoice_for_quote(
        &mut self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<(), LiquidityLoopOutError> {
        let preimage = quote.payment_preimage.ok_or_else(|| {
            LiquidityLoopOutError::Store(format!(
                "provider loop out quote {:?} is missing its preimage",
                quote.quote_id
            ))
        })?;
        let principal = loop_out_payment_principal(quote.amount, quote.provider_fee)?;
        let udt_type_script = quote.asset.udt_type_script.clone().map(Into::into);
        self.payment
            .register_provider_loop_out_invoice(
                quote.payment_hash,
                preimage,
                principal,
                udt_type_script,
            )
            .await
            .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))
    }

    async fn handle_provider_accept_loop_in(
        &mut self,
        params: ProviderAcceptLoopInParams,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        ensure_provider_mode(&self.store)?;
        let quote_id: Hash256 = params.quote_id.into();
        if let Some(existing) = self
            .store
            .get_liquidity_swap(&quote_id)
            .map_err(map_store_error)?
        {
            if existing.role != LiquiditySwapRole::Provider
                || existing.swap_kind != LiquiditySwapKind::LoopIn
            {
                return Err(LiquidityLoopOutError::Store(format!(
                    "existing swap conflicts with provider loop in accept for quote {quote_id:?}: expected provider loop in, found {:?} {:?}",
                    existing.role, existing.swap_kind
                )));
            }
            let lock_tx_hash: Hash256 = params.lock_tx_hash.into();
            let outpoint = OutPoint::new(lock_tx_hash.into(), params.lock_output_index);
            let existing_tx = self
                .store
                .get_liquidity_chain_tx(&quote_id, LiquidityChainTxRole::Payout)
                .map_err(map_store_error)?
                .ok_or_else(|| {
                    LiquidityLoopOutError::Store(format!(
                        "existing provider loop in swap conflicts with accept params for quote {quote_id:?}: persisted lock outpoint is missing"
                    ))
                })?;
            if existing_tx.tx_hash != lock_tx_hash
                || existing_tx.outpoint.as_ref() != Some(&outpoint)
            {
                return Err(LiquidityLoopOutError::Store(format!(
                    "existing provider loop in lock transaction does not match accept params for quote {quote_id:?}"
                )));
            }
            return self.loop_in_swap_response(&quote_id);
        }
        let quote = self.quote_terms(&quote_id)?;
        ensure_loop_in_quote_terms(&quote)?;
        let now_ms = now_ms();
        let swap_id =
            accept_provider_loop_in(&self.store, &mut self.chain, quote, params, now_ms).await?;
        self.chain
            .watch_loop_in_lock(swap_id, myself)
            .await
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        self.loop_in_swap_response(&swap_id)
    }

    async fn resume_non_terminal(
        &mut self,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<usize, LiquidityLoopOutError> {
        use LiquiditySwapState::*;

        let loop_out_states = [
            PayoutPending,
            PayoutLocked,
            PaymentInFlight,
            PaymentSettled,
            ClaimPending,
            RefundPending,
        ];
        let loop_in_states = [
            OnchainLockPending,
            OnchainLocked,
            PaymentInFlight,
            PaymentSettled,
            ClaimPending,
            RefundPending,
        ];
        let mut swaps = self
            .store
            .list_liquidity_swaps_by_states(&loop_out_states, LiquiditySwapKind::LoopOut)
            .map_err(map_store_error)?;
        swaps.extend(
            self.store
                .list_liquidity_swaps_by_states(&loop_in_states, LiquiditySwapKind::LoopIn)
                .map_err(map_store_error)?,
        );
        let mut resumed = 0;

        for swap in swaps {
            match self.resume_swap(swap.clone(), myself.clone()).await {
                Ok(true) => resumed += 1,
                Ok(false) => {}
                Err(error) => {
                    persist_loop_out_payment_failure_context(
                        &self.store,
                        swap.swap_id,
                        error.to_string(),
                    );
                    tracing::warn!(swap_id = ?swap.swap_id, %error, "failed to resume liquidity swap; continuing recovery scan");
                }
            }
        }

        Ok(resumed)
    }

    async fn resume_swap(
        &mut self,
        swap: LiquiditySwapRecord,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<bool, LiquidityLoopOutError> {
        if swap.swap_kind == LiquiditySwapKind::LoopIn {
            return self.resume_loop_in_swap(swap, myself).await;
        }

        match recovery_action_for_loop_out_state(swap.state) {
            Some(RecoveryAction::WatchPayout) => {
                if swap.role == LiquiditySwapRole::Client
                    && (self.payout_validation_retries.contains_key(&swap.swap_id)
                        || self
                            .definitive_payout_validation_failures
                            .contains(&swap.swap_id))
                {
                    return Ok(false);
                }
                if swap.onchain_outpoint.is_none() {
                    if swap.role == LiquiditySwapRole::Client {
                        self.quote_terms(&swap.quote_id)?;
                        if self.watched_payout_swaps.contains(&swap.swap_id) {
                            return Ok(false);
                        }
                        self.chain
                            .watch_payout_lock(swap.swap_id, myself.clone())
                            .await
                            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                        self.watched_payout_swaps.insert(swap.swap_id);
                        return Ok(true);
                    }
                    persist_loop_out_payment_failure_context(
                        &self.store,
                        swap.swap_id,
                        "payout recovery missing persisted outpoint".to_string(),
                    );
                    return Ok(false);
                }
                if self.watched_payout_swaps.contains(&swap.swap_id) {
                    return Ok(false);
                }
                self.chain
                    .watch_payout_lock(swap.swap_id, myself.clone())
                    .await
                    .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                self.watched_payout_swaps.insert(swap.swap_id);
                Ok(true)
            }
            Some(RecoveryAction::ResumePayment) => {
                if swap.role == LiquiditySwapRole::Client {
                    if self.active_payment_swaps.contains(&swap.swap_id)
                        || self.payout_validation_retries.contains_key(&swap.swap_id)
                        || self
                            .definitive_payout_validation_failures
                            .contains(&swap.swap_id)
                    {
                        return Ok(false);
                    }
                    return Ok(self
                        .authorize_client_payout_with_retry(
                            swap.swap_id,
                            ClientPayoutAuthorization::PayoutLockedRecovery,
                            1,
                            myself.clone(),
                        )
                        .await);
                }
                self.resume_provider_loop_out_payment(swap, myself.clone())
                    .await
            }
            Some(RecoveryAction::ReloadPayment) => {
                if swap.role == LiquiditySwapRole::Client {
                    if self.active_payment_swaps.contains(&swap.swap_id) {
                        return Ok(false);
                    }
                    self.active_payment_swaps.insert(swap.swap_id);
                    let payment = self.payment.clone();
                    let store = self.store.clone();
                    let actor = myself.clone();
                    let swap_id = swap.swap_id;
                    let payment_hash = swap.payment_hash;
                    self.spawn_job(async move {
                        reconcile_loop_out_payment(
                            store,
                            payment,
                            actor,
                            swap_id,
                            payment_hash,
                            LOOP_OUT_PAYMENT_RECONCILE_INTERVAL,
                        )
                        .await;
                    });
                } else {
                    return self
                        .resume_provider_loop_out_payment(swap, myself.clone())
                        .await;
                }
                Ok(true)
            }
            Some(RecoveryAction::BroadcastClaim) => {
                if swap.role == LiquiditySwapRole::Client {
                    if self.watched_claim_swaps.contains(&swap.swap_id) {
                        return Ok(false);
                    }
                    let has_watchable_claim = self
                        .store
                        .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Claim)
                        .map_err(map_store_error)?
                        .is_some_and(|record| {
                            matches!(
                                record.status,
                                fiber_types::LiquidityChainTxStatus::Broadcast
                                    | fiber_types::LiquidityChainTxStatus::Confirmed
                            )
                        });
                    if has_watchable_claim {
                        self.chain
                            .watch_claim(swap.swap_id, myself.clone())
                            .await
                            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                    } else {
                        claim_client_loop_out(
                            &self.store,
                            &mut self.chain,
                            swap.swap_id,
                            now_ms(),
                            myself.clone(),
                        )
                        .await?;
                        self.chain
                            .watch_claim(swap.swap_id, myself.clone())
                            .await
                            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                    }
                    self.watched_claim_swaps.insert(swap.swap_id);
                } else if swap.swap_kind == LiquiditySwapKind::LoopOut {
                    if self.watched_claim_swaps.contains(&swap.swap_id) {
                        return Ok(false);
                    }
                    self.chain
                        .watch_provider_claim(swap.swap_id, myself.clone())
                        .await
                        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                    self.watched_claim_swaps.insert(swap.swap_id);
                } else {
                    if self.watched_claim_swaps.contains(&swap.swap_id) {
                        return Ok(false);
                    }
                    self.chain
                        .watch_claim(swap.swap_id, myself.clone())
                        .await
                        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                    self.watched_claim_swaps.insert(swap.swap_id);
                }
                Ok(true)
            }
            Some(RecoveryAction::WatchClaim) => {
                if self.watched_claim_swaps.contains(&swap.swap_id) {
                    return Ok(false);
                }
                self.chain
                    .watch_claim(swap.swap_id, myself.clone())
                    .await
                    .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                self.watched_claim_swaps.insert(swap.swap_id);
                Ok(true)
            }
            Some(RecoveryAction::RefundProviderPayout) => {
                self.resume_refund_pending_swap(swap, myself.clone()).await
            }
            None => Ok(false),
        }
    }

    async fn resume_provider_loop_out_payment(
        &mut self,
        swap: LiquiditySwapRecord,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<bool, LiquidityLoopOutError> {
        if self.active_payment_swaps.contains(&swap.swap_id) {
            return Ok(false);
        }
        let quote = self.quote_terms(&swap.quote_id)?;
        let preimage = quote.payment_preimage.ok_or_else(|| {
            LiquidityLoopOutError::Store(format!(
                "provider loop out quote {:?} is missing its preimage",
                swap.quote_id
            ))
        })?;
        self.register_provider_loop_out_invoice_for_quote(&quote)
            .await?;
        self.active_payment_swaps.insert(swap.swap_id);
        let payment = self.payment.clone();
        let store = self.store.clone();
        let actor = myself.clone();
        let swap_id = swap.swap_id;
        let payment_hash = quote.payment_hash;
        self.spawn_job(async move {
            observe_provider_loop_out_payment(
                store,
                payment,
                actor,
                swap_id,
                preimage,
                payment_hash,
            )
            .await;
        });
        Ok(true)
    }

    async fn resume_loop_in_swap(
        &mut self,
        swap: LiquiditySwapRecord,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<bool, LiquidityLoopOutError> {
        if swap.role == LiquiditySwapRole::Provider {
            return self.resume_provider_loop_in_swap(swap, myself).await;
        }

        if swap.state == LiquiditySwapState::OnchainLockPending {
            let has_watchable_lock = self
                .store
                .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
                .map_err(map_store_error)?
                .map(|record| {
                    (
                        record.status,
                        matches!(
                            record.status,
                            LiquidityChainTxStatus::Planned
                                | LiquidityChainTxStatus::Broadcast
                                | LiquidityChainTxStatus::Confirmed
                        ) && record.outpoint.is_some(),
                    )
                });
            if has_watchable_lock.is_some_and(|(_, watchable)| watchable) {
                self.chain
                    .watch_loop_in_lock(swap.swap_id, myself)
                    .await
                    .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                return Ok(true);
            }
            if has_watchable_lock
                .is_some_and(|(status, _)| status == LiquidityChainTxStatus::Rejected)
            {
                mark_loop_in_broadcast_failed(
                    &self.store,
                    swap.swap_id,
                    "loop in lock transaction rejected before restart".to_string(),
                    now_ms(),
                )?;
                return Ok(true);
            }
        }
        if swap.state == LiquiditySwapState::RefundPending {
            return self.resume_refund_pending_swap(swap, myself).await;
        }
        Ok(false)
    }

    async fn resume_refund_pending_swap(
        &mut self,
        swap: LiquiditySwapRecord,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<bool, LiquidityLoopOutError> {
        if self.active_refund_swaps.contains(&swap.swap_id) {
            return Ok(false);
        }
        if swap.onchain_outpoint.is_none() {
            persist_loop_out_payment_failure_context(
                &self.store,
                swap.swap_id,
                "refund recovery missing persisted outpoint".to_string(),
            );
            return Ok(false);
        }
        let should_broadcast_refund = !matches!(
            self.store
                .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Refund)
                .map_err(map_store_error)?,
            Some(record)
                if matches!(
                    record.status,
                    LiquidityChainTxStatus::Broadcast | LiquidityChainTxStatus::Confirmed
                )
        );
        if should_broadcast_refund {
            self.chain
                .broadcast_refund(&swap)
                .await
                .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        }
        self.chain
            .watch_refund(swap.swap_id, myself)
            .await
            .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
        self.active_refund_swaps.insert(swap.swap_id);
        Ok(true)
    }

    async fn resume_provider_loop_in_swap(
        &mut self,
        swap: LiquiditySwapRecord,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<bool, LiquidityLoopOutError> {
        match swap.state {
            LiquiditySwapState::OnchainLockPending => {
                let has_watchable_lock = self
                    .store
                    .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
                    .map_err(map_store_error)?
                    .map(|record| {
                        (
                            record.status,
                            matches!(
                                record.status,
                                LiquidityChainTxStatus::Planned
                                    | LiquidityChainTxStatus::Broadcast
                                    | LiquidityChainTxStatus::Confirmed
                            ) && record.outpoint.is_some(),
                        )
                    });
                if has_watchable_lock.is_some_and(|(_, watchable)| watchable) {
                    self.chain
                        .watch_loop_in_lock(swap.swap_id, myself)
                        .await
                        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                    return Ok(true);
                }
                if has_watchable_lock
                    .is_some_and(|(status, _)| status == LiquidityChainTxStatus::Rejected)
                {
                    mark_loop_in_broadcast_failed(
                        &self.store,
                        swap.swap_id,
                        "loop in lock transaction rejected before restart".to_string(),
                        now_ms(),
                    )?;
                    return Ok(true);
                }
                Ok(false)
            }
            LiquiditySwapState::OnchainLocked => {
                if self.active_payment_swaps.contains(&swap.swap_id) {
                    return Ok(false);
                }
                self.start_provider_loop_in_payment(swap, myself).await?;
                Ok(true)
            }
            LiquiditySwapState::PaymentInFlight => {
                if self.active_payment_swaps.contains(&swap.swap_id) {
                    return Ok(false);
                }
                self.active_payment_swaps.insert(swap.swap_id);
                let payment = self.payment.clone();
                let store = self.store.clone();
                let actor = myself.clone();
                let swap_id = swap.swap_id;
                let payment_hash = swap.payment_hash;
                self.spawn_job(async move {
                    reconcile_loop_out_payment(
                        store,
                        payment,
                        actor,
                        swap_id,
                        payment_hash,
                        LOOP_OUT_PAYMENT_RECONCILE_INTERVAL,
                    )
                    .await;
                });
                Ok(true)
            }
            LiquiditySwapState::PaymentSettled | LiquiditySwapState::ClaimPending => {
                if self.watched_claim_swaps.contains(&swap.swap_id) {
                    return Ok(false);
                }
                let has_watchable_claim = self
                    .store
                    .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Claim)
                    .map_err(map_store_error)?
                    .is_some_and(|record| {
                        matches!(
                            record.status,
                            LiquidityChainTxStatus::Broadcast | LiquidityChainTxStatus::Confirmed
                        )
                    });
                if has_watchable_claim {
                    self.chain
                        .watch_claim(swap.swap_id, myself.clone())
                        .await
                        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                } else {
                    claim_provider_loop_in(
                        &self.store,
                        &mut self.chain,
                        swap.swap_id,
                        now_ms(),
                        myself.clone(),
                    )
                    .await?;
                    self.chain
                        .watch_claim(swap.swap_id, myself.clone())
                        .await
                        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
                }
                self.watched_claim_swaps.insert(swap.swap_id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn start_provider_loop_in_payment(
        &mut self,
        swap: LiquiditySwapRecord,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let quote = self.quote_terms(&swap.quote_id)?;
        ensure_loop_in_quote_terms(&quote)?;
        let request = prepare_provider_loop_in_payment(&self.store, quote)?;
        let payment_hash = request.payment_hash;
        self.active_payment_swaps.insert(swap.swap_id);
        let mut payment = self.payment.clone();
        let store = self.store.clone();
        let swap_id = swap.swap_id;
        self.spawn_job(async move {
            if let Err(error) = mark_payment_dispatch_started(&store, swap_id, now_ms()) {
                tracing::warn!(?swap_id, %error, "failed to persist provider loop in payment dispatch");
                send_payment_recovery_finished(&myself, swap_id);
                return;
            }
            match payment.send_loop_out_payment(request).await {
                Ok(preimage) => send_payment_settled(&myself, swap_id, preimage),
                Err(error) => {
                    tracing::warn!(?swap_id, %error, "loop in provider payment remains in flight after send failure");
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
        Ok(())
    }

    fn prune_recovery_guards(&mut self, swap_id: Hash256) {
        self.watched_payout_swaps.remove(&swap_id);
        self.payout_validation_retries.remove(&swap_id);
        self.definitive_payout_validation_failures.remove(&swap_id);
        self.active_payment_swaps.remove(&swap_id);
        self.watched_claim_swaps.remove(&swap_id);
        self.active_refund_swaps.remove(&swap_id);
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
            payout_outpoint: match swap.swap_kind {
                LiquiditySwapKind::LoopOut => swap.onchain_outpoint.map(Into::into),
                LiquiditySwapKind::LoopIn => None,
            },
            created_at: swap.created_at,
        })
    }

    fn loop_in_swap_response(
        &self,
        swap_id: &Hash256,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        self.swap_response(swap_id)
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
                send_payment_recovery_finished(&myself, swap_id);
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
            send_payment_recovery_finished(&myself, swap_id);
        }
    }
}

async fn observe_provider_loop_out_payment<P>(
    store: impl LiquidityStore + Send + Sync + 'static,
    mut payment: P,
    myself: ActorRef<LiquidityActorMessage>,
    swap_id: Hash256,
    preimage: Hash256,
    payment_hash: Hash256,
) where
    P: LoopOutPaymentAdapter + Send + 'static,
    P::Error: Display,
{
    for attempt in 0..PROVIDER_LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS {
        tokio::time::sleep(LOOP_OUT_PAYMENT_RECONCILE_INTERVAL).await;
        match payment.reload_provider_loop_out_payment(payment_hash).await {
            Ok(LoopOutPaymentStatus::Settled(_)) => {
                send_payment_settled(&myself, swap_id, preimage);
                return;
            }
            Ok(LoopOutPaymentStatus::InFlight) => {
                tracing::debug!(
                    ?swap_id,
                    ?payment_hash,
                    "provider loop out payment still in flight"
                );
            }
            Ok(LoopOutPaymentStatus::Failed(reason)) => {
                tracing::warn!(?swap_id, ?payment_hash, %reason, "provider loop out payment failed while observing");
                persist_loop_out_payment_failure_context(&store, swap_id, reason);
                send_payment_recovery_finished(&myself, swap_id);
                return;
            }
            Err(error) => {
                tracing::warn!(?swap_id, ?payment_hash, %error, "failed to reload provider loop out payment; retrying");
            }
        }

        if attempt + 1 == PROVIDER_LOOP_OUT_PAYMENT_RECONCILE_MAX_RELOAD_ATTEMPTS {
            persist_loop_out_payment_failure_context(
                &store,
                swap_id,
                "provider payment observation exhausted while status remained in flight"
                    .to_string(),
            );
            send_payment_recovery_finished(&myself, swap_id);
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

fn send_payment_recovery_finished(myself: &ActorRef<LiquidityActorMessage>, swap_id: Hash256) {
    if let Err(error) = myself.send_message(LiquidityActorMessage::PaymentRecoveryFinished(swap_id))
    {
        tracing::warn!(?swap_id, %error, "failed to finish loop out payment recovery");
    }
}

fn quote_expires_at(now_ms: u64, expires_after_seconds: u64) -> Result<u64, LiquidityLoopOutError> {
    expires_after_seconds
        .checked_mul(1_000)
        .and_then(|ttl_ms| now_ms.checked_add(ttl_ms))
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

fn loop_in_quote_hash(params: &QuoteLoopInParams, now_ms: u64, domain: &[u8]) -> Hash256 {
    let mut seed = Vec::new();
    seed.extend_from_slice(domain);
    seed.extend_from_slice(params.asset_id.as_bytes());
    seed.extend_from_slice(&params.amount.to_le_bytes());
    seed.extend_from_slice(params.client_invoice.as_bytes());
    seed.extend_from_slice(params.refund_lock.as_bytes());
    seed.extend_from_slice(&params.max_provider_fee.to_le_bytes());
    seed.extend_from_slice(&params.max_routing_fee.to_le_bytes());
    seed.extend_from_slice(&params.expires_after_seconds.to_le_bytes());
    seed.extend_from_slice(&now_ms.to_le_bytes());
    ckb_hash::blake2b_256(seed).into()
}

#[cfg(test)]
fn deterministic_provider_pubkey() -> fiber_types::Pubkey {
    let sk = SecretKey::from_slice(&[42; 32]).expect("valid deterministic provider secret key");
    fiber_types::Pubkey::from(sk.public_key(SECP256K1))
}

fn ensure_loop_in_quote_terms(quote: &LoopOutQuoteTerms) -> Result<(), LiquidityLoopOutError> {
    if quote.swap_kind == LiquiditySwapKind::LoopIn {
        return Ok(());
    }

    Err(LiquidityLoopOutError::Store(format!(
        "quote is not a loop in quote: {:?}",
        quote.quote_id
    )))
}

fn ensure_loop_out_quote_terms(quote: &LoopOutQuoteTerms) -> Result<(), LiquidityLoopOutError> {
    if quote.swap_kind != LiquiditySwapKind::LoopOut {
        return Err(LiquidityLoopOutError::Store(format!(
            "quote is not a loop out quote: {:?}",
            quote.quote_id
        )));
    }

    Ok(())
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
        ClaimPending => Some(RecoveryAction::BroadcastClaim),
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

    /// Register the provider invoice + preimage so an incoming Loop Out payment settles.
    async fn register_provider_loop_out_invoice(
        &mut self,
        payment_hash: Hash256,
        preimage: Hash256,
        amount: u128,
        udt_type_script: Option<ckb_types::packed::Script>,
    ) -> Result<(), Self::Error>;

    /// Poll the registered provider invoice and classify incoming payment settlement.
    async fn reload_provider_loop_out_payment(
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
    payout_outpoint: Option<ckb_types::packed::OutPoint>,
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

    if let Some(outpoint) = payout_outpoint {
        store
            .update_liquidity_swap(
                &swap_id,
                LiquiditySwapUpdate {
                    onchain_outpoint: Some(outpoint.clone()),
                    updated_at: now_ms,
                    ..Default::default()
                },
            )
            .map_err(map_store_error)?;
        store
            .insert_liquidity_chain_tx(fiber_types::LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: Hash256::from(outpoint.tx_hash()),
                outpoint: Some(outpoint),
                status: LiquidityChainTxStatus::Planned,
                failure_reason: None,
                created_at: now_ms,
                updated_at: now_ms,
            })
            .map_err(map_store_error)?;
    }

    Ok(swap_id)
}

/// Accept a client Loop In quote and persist restart-safe state before chain broadcast.
pub async fn accept_client_loop_in<S, C>(
    store: &S,
    chain: &mut C,
    quote: LoopOutQuoteTerms,
    funding_tx: String,
    now_ms: u64,
    myself: ActorRef<LiquidityActorMessage>,
) -> Result<Hash256, LiquidityLoopOutError>
where
    S: LiquidityStore,
    C: LoopOutChainAdapter,
    C::Error: Display,
{
    if quote.expires_at <= now_ms {
        return Err(LiquidityLoopOutError::QuoteExpired);
    }
    chain
        .ensure_loop_in_lock_available(&funding_tx)
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;

    let swap_id = quote.quote_id;
    if let Some(existing) = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
    {
        if existing.swap_kind != LiquiditySwapKind::LoopIn
            || existing.role != LiquiditySwapRole::Client
        {
            return Err(LiquidityLoopOutError::Store(
                "existing swap is not a client loop in swap".to_string(),
            ));
        }
        if existing.state != LiquiditySwapState::OnchainLockPending {
            return Err(LiquidityLoopOutError::InvalidStateTransition {
                from: existing.state,
                to: LiquiditySwapState::OnchainLockPending,
            });
        }
    } else {
        store
            .insert_liquidity_swap(loop_in_record(&quote, LiquiditySwapRole::Client, now_ms))
            .map_err(map_store_error)?;
    }
    if let Err(error) = chain
        .broadcast_loop_in_lock(&quote, &funding_tx, myself)
        .await
    {
        let reason = error.to_string();
        if store
            .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Payout)
            .map_err(map_store_error)?
            .is_some()
        {
            persist_loop_out_payment_failure_context(store, swap_id, reason.clone());
        } else {
            mark_loop_in_broadcast_failed(store, swap_id, reason.clone(), now_ms)?;
        }
        return Err(LiquidityLoopOutError::Chain(reason));
    }

    Ok(swap_id)
}

/// Accept an observed client Loop In lock on the provider side before watching it.
pub async fn accept_provider_loop_in<S, C>(
    store: &S,
    chain: &mut C,
    quote: LoopOutQuoteTerms,
    params: ProviderAcceptLoopInParams,
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
    if quote.quote_id != params.quote_id.into() {
        return Err(LiquidityLoopOutError::Store(
            "provider loop in accept quote id mismatch".to_string(),
        ));
    }

    let swap_id = quote.quote_id;
    let lock_tx_hash: Hash256 = params.lock_tx_hash.into();
    let outpoint = OutPoint::new(lock_tx_hash.into(), params.lock_output_index);
    chain
        .validate_observed_loop_in_lock(&quote, &outpoint)
        .await
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    if let Some(existing_tx) = store
        .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Payout)
        .map_err(map_store_error)?
    {
        if existing_tx.tx_hash != lock_tx_hash || existing_tx.outpoint.as_ref() != Some(&outpoint) {
            return Err(LiquidityLoopOutError::Store(
                "existing provider loop in lock transaction does not match accept params"
                    .to_string(),
            ));
        }
    }
    if let Some(existing) = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
    {
        if existing.swap_kind != LiquiditySwapKind::LoopIn
            || existing.role != LiquiditySwapRole::Provider
        {
            return Err(LiquidityLoopOutError::Store(
                "existing swap is not a provider loop in swap".to_string(),
            ));
        }
        if existing.state != LiquiditySwapState::OnchainLockPending {
            return Err(LiquidityLoopOutError::InvalidStateTransition {
                from: existing.state,
                to: LiquiditySwapState::OnchainLockPending,
            });
        }
    } else {
        store
            .insert_liquidity_swap(loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms))
            .map_err(map_store_error)?;
    }

    store
        .update_liquidity_swap(
            &swap_id,
            LiquiditySwapUpdate {
                onchain_outpoint: Some(outpoint.clone()),
                updated_at: now_ms,
                ..Default::default()
            },
        )
        .map_err(map_store_error)?;
    if store
        .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Payout)
        .map_err(map_store_error)?
        .is_none()
    {
        store
            .insert_liquidity_chain_tx(fiber_types::LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: lock_tx_hash,
                outpoint: Some(outpoint),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms,
                updated_at: now_ms,
            })
            .map_err(map_store_error)?;
    }

    Ok(swap_id)
}

/// Accept a provider Loop Out quote and persist restart-safe state before chain broadcast.
pub async fn accept_provider_loop_out<S, C>(
    store: &S,
    chain: &mut C,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
    myself: ActorRef<LiquidityActorMessage>,
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
        .await
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
        .broadcast_payout_lock(&quote, &onchain_outpoint, myself)
        .await
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

/// Mark the client-side Loop In lock as confirmed on-chain.
pub fn mark_loop_in_lock_confirmed<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::OnchainLocked, now_ms)
}

/// Mark a Loop In swap failed when lock broadcast fails before a transaction is accepted.
pub fn mark_loop_in_broadcast_failed<S>(
    store: &S,
    swap_id: Hash256,
    reason: String,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    store
        .update_liquidity_swap_state(
            &swap_id,
            LiquidityStateTransition {
                state: LiquiditySwapState::Failed,
                updated_at: now_ms,
                reason: Some(reason),
            },
        )
        .map_err(map_store_error)
}

/// Persist that the provider Loop In payment has started after client lock confirmation.
pub fn start_provider_loop_in_payment<S>(
    store: &S,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
) -> Result<crate::liquidity::payment::LoopOutPaymentRequest, LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    let swap_id = quote.quote_id;
    let request = prepare_provider_loop_in_payment(store, quote)?;
    mark_payment_dispatch_started(store, swap_id, now_ms)?;
    Ok(request)
}

fn prepare_provider_loop_in_payment<S>(
    store: &S,
    quote: LoopOutQuoteTerms,
) -> Result<crate::liquidity::payment::LoopOutPaymentRequest, LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    let swap_id = quote.quote_id;
    ensure_loop_in_quote_terms(&quote)?;
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
        })?;
    if swap.swap_kind != LiquiditySwapKind::LoopIn || swap.role != LiquiditySwapRole::Provider {
        return Err(LiquidityLoopOutError::Store(
            "cannot start provider loop in payment for non-provider loop in swap".to_string(),
        ));
    }
    if swap.state != LiquiditySwapState::OnchainLocked {
        return Err(LiquidityLoopOutError::InvalidStateTransition {
            from: swap.state,
            to: LiquiditySwapState::PaymentInFlight,
        });
    }
    let invoice = quote.client_invoice.clone().ok_or_else(|| {
        LiquidityLoopOutError::PaymentFailed(
            "cannot start provider loop in payment without client invoice".to_string(),
        )
    })?;
    let request = crate::liquidity::payment::LoopOutPaymentRequest::new_invoice(
        quote.payment_hash,
        invoice,
        quote.amount,
        quote.routing_fee_limit,
    );

    Ok(request)
}

fn mark_payment_dispatch_started<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)
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
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
        })?;
    if !swap
        .state
        .can_transition_to(LiquiditySwapState::PaymentSettled)
    {
        return Err(LiquidityLoopOutError::InvalidStateTransition {
            from: swap.state,
            to: LiquiditySwapState::PaymentSettled,
        });
    }
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

/// Persist a settled provider Loop In payment preimage before broadcasting the claim.
pub fn persist_provider_loop_in_payment_preimage<S>(
    store: &S,
    swap_id: Hash256,
    preimage: Hash256,
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
    if swap.swap_kind != LiquiditySwapKind::LoopIn || swap.role != LiquiditySwapRole::Provider {
        return Err(LiquidityLoopOutError::Store(
            "cannot settle provider loop in payment for non-provider loop in swap".to_string(),
        ));
    }
    if !swap
        .state
        .can_transition_to(LiquiditySwapState::PaymentSettled)
    {
        return Err(LiquidityLoopOutError::InvalidStateTransition {
            from: swap.state,
            to: LiquiditySwapState::PaymentSettled,
        });
    }
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
pub async fn claim_client_loop_out<S, C>(
    store: &S,
    chain: &mut C,
    swap_id: Hash256,
    now_ms: u64,
    myself: ActorRef<LiquidityActorMessage>,
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
    let claim_plan = LoopOutClaimPlan::from_record(&swap).map_err(|error| {
        if swap.payment_preimage.is_none() {
            LiquidityLoopOutError::InvalidStateTransition {
                from: swap.state,
                to: LiquiditySwapState::ClaimPending,
            }
        } else {
            error
        }
    })?;

    if swap.state == LiquiditySwapState::PaymentSettled {
        transition_swap(store, &swap_id, LiquiditySwapState::ClaimPending, now_ms)?;
    }
    chain
        .broadcast_claim(claim_plan.into(), myself)
        .await
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    Ok(())
}

/// Claim the on-chain Loop In client lock after the provider payment settles.
pub async fn claim_provider_loop_in<S, C>(
    store: &S,
    chain: &mut C,
    swap_id: Hash256,
    now_ms: u64,
    myself: ActorRef<LiquidityActorMessage>,
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
    if swap.swap_kind != LiquiditySwapKind::LoopIn || swap.role != LiquiditySwapRole::Provider {
        return Err(LiquidityLoopOutError::Store(
            "cannot claim provider loop in for non-provider loop in swap".to_string(),
        ));
    }
    if !client_can_claim(swap.state) && swap.state != LiquiditySwapState::ClaimPending {
        return Err(LiquidityLoopOutError::InvalidStateTransition {
            from: swap.state,
            to: LiquiditySwapState::ClaimPending,
        });
    }
    let claim_plan = LoopOutClaimPlan::from_record(&swap).map_err(|error| {
        if swap.payment_preimage.is_none() {
            LiquidityLoopOutError::InvalidStateTransition {
                from: swap.state,
                to: LiquiditySwapState::ClaimPending,
            }
        } else {
            error
        }
    })?;

    if swap.state == LiquiditySwapState::PaymentSettled {
        transition_swap(store, &swap_id, LiquiditySwapState::ClaimPending, now_ms)?;
    }
    chain
        .broadcast_claim(claim_plan.into(), myself)
        .await
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

/// Mark the provider-side Loop In claim as confirmed on-chain.
pub fn mark_loop_in_provider_claim_confirmed<S>(
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
    if swap.swap_kind != LiquiditySwapKind::LoopIn || swap.role != LiquiditySwapRole::Provider {
        return Err(LiquidityLoopOutError::Store(
            "cannot confirm loop in provider claim for non-provider loop in swap".to_string(),
        ));
    }
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

/// Mark the provider-side Loop Out incoming payment as in flight.
pub fn mark_provider_payment_in_flight<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)
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
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
        })?;
    if swap.state == LiquiditySwapState::PayoutLocked {
        transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)?;
    }
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
    if swap.role != LiquiditySwapRole::Provider || swap.swap_kind != LiquiditySwapKind::LoopOut {
        return Err(LiquidityLoopOutError::Chain(
            "cannot observe provider claim for non-provider loop out swap".to_string(),
        ));
    }
    if swap.state == LiquiditySwapState::Success {
        return Ok(());
    }
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

/// Mark the provider-side Loop Out refund as confirmed on-chain.
pub fn mark_provider_refund_confirmed<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::Refunded, now_ms)
}

/// Mark a supported refund owner as confirmed on-chain.
pub fn mark_refund_confirmed<S>(
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
    match (swap.swap_kind, swap.role) {
        (LiquiditySwapKind::LoopOut, LiquiditySwapRole::Provider)
        | (LiquiditySwapKind::LoopIn, LiquiditySwapRole::Client) => {
            transition_swap(store, &swap_id, LiquiditySwapState::Refunded, now_ms)
        }
        _ => Err(LiquidityLoopOutError::Chain(format!(
            "unsupported refund confirmation for {:?} {:?}",
            swap.swap_kind, swap.role
        ))),
    }
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

fn loop_in_record(
    quote: &LoopOutQuoteTerms,
    role: LiquiditySwapRole,
    now_ms: u64,
) -> LiquiditySwapRecord {
    LiquiditySwapRecord {
        swap_id: quote.quote_id,
        quote_id: quote.quote_id,
        role,
        swap_kind: LiquiditySwapKind::LoopIn,
        asset_id: quote.asset.asset_id.clone(),
        state: LiquiditySwapState::OnchainLockPending,
        payment_hash: quote.payment_hash,
        payment_preimage: None,
        amount: quote.amount,
        onchain_outpoint: None,
        payout_deadline: None,
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

fn liquidity_chain_transaction_from_store(
    record: fiber_types::LiquidityChainTxRecord,
    swap_kind: LiquiditySwapKind,
) -> LiquidityChainTransaction {
    LiquidityChainTransaction {
        role: semantic_chain_tx_role(swap_kind, record.role),
        tx_hash: record.tx_hash.into(),
        outpoint: record.outpoint,
        status: chain_tx_status_to_string(record.status),
        failure_reason: record.failure_reason,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

/// Map a persisted chain tx role to its semantic role for the swap kind.
///
/// A Loop In swap stores its client on-chain lock transaction under the
/// `Payout` role; that role is surfaced as [`LiquidityChainTransactionRole::LoopInLock`].
fn semantic_chain_tx_role(
    swap_kind: LiquiditySwapKind,
    role: LiquidityChainTxRole,
) -> LiquidityChainTransactionRole {
    match (swap_kind, role) {
        (LiquiditySwapKind::LoopIn, LiquidityChainTxRole::Payout) => {
            LiquidityChainTransactionRole::LoopInLock
        }
        (_, LiquidityChainTxRole::Payout) => LiquidityChainTransactionRole::Payout,
        (_, LiquidityChainTxRole::Claim) => LiquidityChainTransactionRole::Claim,
        (_, LiquidityChainTxRole::Refund) => LiquidityChainTransactionRole::Refund,
    }
}

/// Stable ordering key for chain transactions regardless of store iteration order.
fn chain_tx_role_order(role: LiquidityChainTransactionRole) -> u8 {
    match role {
        LiquidityChainTransactionRole::Payout | LiquidityChainTransactionRole::LoopInLock => 0,
        LiquidityChainTransactionRole::Claim => 1,
        LiquidityChainTransactionRole::Refund => 2,
    }
}

fn chain_tx_status_to_string(status: LiquidityChainTxStatus) -> String {
    match status {
        LiquidityChainTxStatus::Planned => "planned",
        LiquidityChainTxStatus::Broadcast => "broadcast",
        LiquidityChainTxStatus::Confirmed => "confirmed",
        LiquidityChainTxStatus::Rejected => "rejected",
    }
    .to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn ensure_provider_mode<S>(store: &S) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    if !store.get_provider_mode().map_err(map_store_error)? {
        return Err(LiquidityLoopOutError::Store(
            "provider mode is disabled".to_string(),
        ));
    }
    Ok(())
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
    use fiber_types::{
        Hash256, HashAlgorithm, LiquidityAsset, LiquidityAssetKind, LiquidityChainTxRecord,
        LiquidityChainTxRole, LiquidityChainTxStatus, LiquiditySwapState, Pubkey,
    };
    use ractor::concurrency::Duration;
    use secp256k1::{Secp256k1, SecretKey, SECP256K1};
    use tokio::sync::oneshot;

    use crate::liquidity::store::{
        LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
        LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapUpdate,
    };
    use crate::{
        gen_deterministic_secp256k1_keypair_tuple, invoice::Currency, invoice::InvoiceBuilder,
    };

    use super::*;

    fn test_json_payout_outpoint() -> ckb_jsonrpc_types::OutPoint {
        OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0).into()
    }

    #[test]
    fn actor_message_names_cover_m3_mutations_and_recovery() {
        let names = LiquidityActorMessage::variant_names();

        assert_eq!(
            names,
            &[
                "quote_loop_out",
                "quote_loop_in",
                "import_liquidity_quote",
                "loop_out",
                "loop_in",
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
                "resume_non_terminal",
                "payout_confirmed",
                "retry_payout_validation",
                "payment_settled",
                "claim_confirmed",
                "provider_claim_observed",
                "refund_confirmed",
                "chain_tx_rejected",
                "loop_in_lock_confirmed",
                "payment_recovery_finished",
            ]
        );
    }

    #[test]
    fn resume_non_terminal_action_is_explicit_actor_message() {
        assert!(LiquidityActorMessage::variant_names().contains(&"resume_non_terminal"));
    }

    #[test]
    fn actor_message_names_cover_loop_in_runtime() {
        let names = LiquidityActorMessage::variant_names();

        assert!(names.contains(&"quote_loop_in"));
        assert!(names.contains(&"loop_in"));
        assert!(names.contains(&"loop_in_lock_confirmed"));
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
        quote_writes: Shared<usize>,
        assets: Shared<HashMap<String, LiquidityAsset>>,
        chain_txs: Shared<HashMap<(Hash256, LiquidityChainTxRole), LiquidityChainTxRecord>>,
        signed_txs:
            Shared<HashMap<(Hash256, LiquidityChainTxRole), ckb_types::packed::Transaction>>,
        events: Shared<Vec<&'static str>>,
        listed_swap_kinds: Shared<Vec<LiquiditySwapKind>>,
        label: Option<&'static str>,
        provider_mode: Shared<bool>,
        provider_mode_writes: Shared<usize>,
        quote_lookup_results: Shared<Vec<Result<(), String>>>,
        swap_failure_write_results: Shared<Vec<Result<(), String>>>,
        chain_failure_write_results: Shared<Vec<Result<(), String>>>,
        definitive_failure_write_results: Shared<Vec<Result<(), String>>>,
    }

    impl TestLiquidityStore {
        fn new(events: Shared<Vec<&'static str>>, label: &'static str) -> Self {
            Self {
                swaps: Shared::new(HashMap::new()),
                quotes: Shared::new(HashMap::new()),
                quote_writes: Shared::new(0),
                assets: Shared::new(HashMap::new()),
                chain_txs: Shared::new(HashMap::new()),
                signed_txs: Shared::new(HashMap::new()),
                events,
                listed_swap_kinds: Shared::new(Vec::new()),
                label: Some(label),
                provider_mode: Shared::new(false),
                provider_mode_writes: Shared::new(0),
                quote_lookup_results: Shared::new(Vec::new()),
                swap_failure_write_results: Shared::new(Vec::new()),
                chain_failure_write_results: Shared::new(Vec::new()),
                definitive_failure_write_results: Shared::new(Vec::new()),
            }
        }

        fn events(&self) -> Shared<Vec<&'static str>> {
            self.events.clone()
        }

        fn listed_swap_kinds(&self) -> Vec<LiquiditySwapKind> {
            self.listed_swap_kinds.borrow().clone()
        }

        fn set_quote_lookup_results(&self, results: Vec<Result<(), String>>) {
            *self.quote_lookup_results.borrow_mut() = results;
        }

        fn set_swap_failure_write_results(&self, results: Vec<Result<(), String>>) {
            *self.swap_failure_write_results.borrow_mut() = results;
        }

        fn set_chain_failure_write_results(&self, results: Vec<Result<(), String>>) {
            *self.chain_failure_write_results.borrow_mut() = results;
        }

        fn set_definitive_failure_write_results(&self, results: Vec<Result<(), String>>) {
            *self.definitive_failure_write_results.borrow_mut() = results;
        }

        fn apply_definitive_failure_write_result(&self) -> Result<(), LiquidityStoreError> {
            let mut results = self.definitive_failure_write_results.borrow_mut();
            if results.is_empty() {
                Ok(())
            } else {
                results.remove(0).map_err(LiquidityStoreError::Backend)
            }
        }

        fn insert_event(
            &self,
            role: LiquiditySwapRole,
            swap_kind: LiquiditySwapKind,
        ) -> &'static str {
            match (self.label, role, swap_kind) {
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapKind::LoopIn) => {
                    "client_insert_swap"
                }
                (Some("client"), LiquiditySwapRole::Client, LiquiditySwapKind::LoopOut) => {
                    "client_insert_created"
                }
                (Some("provider"), LiquiditySwapRole::Provider, _) => "provider_insert_created",
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
            *self.quote_writes.borrow_mut() += 1;
            self.quotes.borrow_mut().insert(quote.quote_id, quote);
            Ok(())
        }

        fn get_loop_out_quote(
            &self,
            quote_id: &Hash256,
        ) -> Result<Option<LoopOutQuoteTerms>, LiquidityStoreError> {
            let mut results = self.quote_lookup_results.borrow_mut();
            if !results.is_empty() {
                results.remove(0).map_err(LiquidityStoreError::Backend)?;
            }
            Ok(self.quotes.borrow().get(quote_id).cloned())
        }

        fn insert_liquidity_swap(
            &self,
            swap: LiquiditySwapRecord,
        ) -> Result<(), LiquidityStoreError> {
            self.events
                .borrow_mut()
                .push(self.insert_event(swap.role, swap.swap_kind));
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

        fn list_liquidity_swaps_by_states(
            &self,
            states: &[LiquiditySwapState],
            swap_kind: LiquiditySwapKind,
        ) -> Result<Vec<LiquiditySwapRecord>, LiquidityStoreError> {
            self.listed_swap_kinds.borrow_mut().push(swap_kind);
            Ok(self
                .swaps
                .borrow()
                .values()
                .filter(|swap| swap.swap_kind == swap_kind && states.contains(&swap.state))
                .cloned()
                .collect())
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
            if update.failure_reason.is_some() {
                self.apply_definitive_failure_write_result()?;
                let mut results = self.swap_failure_write_results.borrow_mut();
                if !results.is_empty() {
                    results.remove(0).map_err(LiquidityStoreError::Backend)?;
                }
            }
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

        fn clear_liquidity_swap_failure_reason(
            &self,
            swap_id: &Hash256,
            expected_reason: &str,
            updated_at: u64,
        ) -> Result<bool, LiquidityStoreError> {
            let mut swaps = self.swaps.borrow_mut();
            let swap = swaps
                .get_mut(swap_id)
                .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
            if swap.failure_reason.as_deref() != Some(expected_reason) {
                return Ok(false);
            }
            swap.failure_reason = None;
            swap.updated_at = updated_at;
            Ok(true)
        }

        fn insert_liquidity_chain_tx(
            &self,
            record: LiquidityChainTxRecord,
        ) -> Result<(), LiquidityStoreError> {
            if record.role == LiquidityChainTxRole::Payout {
                self.events.borrow_mut().push("persist_payout_tx");
            }
            let mut chain_txs = self.chain_txs.borrow_mut();
            let key = (record.swap_id, record.role);
            if chain_txs.contains_key(&key) {
                return Err(LiquidityStoreError::Backend(
                    "liquidity chain tx already exists".to_string(),
                ));
            }
            chain_txs.insert(key, record);
            Ok(())
        }

        fn get_liquidity_chain_tx(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
        ) -> Result<Option<LiquidityChainTxRecord>, LiquidityStoreError> {
            Ok(self.chain_txs.borrow().get(&(*swap_id, role)).cloned())
        }

        fn insert_liquidity_chain_tx_signed_tx(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
            tx: ckb_types::packed::Transaction,
        ) -> Result<(), LiquidityStoreError> {
            self.signed_txs.borrow_mut().insert((*swap_id, role), tx);
            Ok(())
        }

        fn get_liquidity_chain_tx_signed_tx(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
        ) -> Result<Option<ckb_types::packed::Transaction>, LiquidityStoreError> {
            Ok(self.signed_txs.borrow().get(&(*swap_id, role)).cloned())
        }

        fn update_liquidity_chain_tx_status(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
            status: LiquidityChainTxStatus,
            failure_reason: Option<String>,
            updated_at: u64,
        ) -> Result<(), LiquidityStoreError> {
            if role == LiquidityChainTxRole::Payout && failure_reason.is_some() {
                self.apply_definitive_failure_write_result()?;
                let mut results = self.chain_failure_write_results.borrow_mut();
                if !results.is_empty() {
                    results.remove(0).map_err(LiquidityStoreError::Backend)?;
                }
            }
            let mut chain_txs = self.chain_txs.borrow_mut();
            let record = chain_txs.get_mut(&(*swap_id, role)).ok_or_else(|| {
                LiquidityStoreError::Backend("liquidity chain tx not found".to_string())
            })?;
            record.status = status;
            record.failure_reason = failure_reason;
            record.updated_at = updated_at;
            Ok(())
        }

        fn list_liquidity_chain_txs_by_status(
            &self,
            statuses: &[LiquidityChainTxStatus],
        ) -> Result<Vec<LiquidityChainTxRecord>, LiquidityStoreError> {
            Ok(self
                .chain_txs
                .borrow()
                .values()
                .filter(|record| statuses.contains(&record.status))
                .cloned()
                .collect())
        }

        fn list_liquidity_chain_txs_by_swap(
            &self,
            swap_id: &Hash256,
        ) -> Result<Vec<LiquidityChainTxRecord>, LiquidityStoreError> {
            Ok(self
                .chain_txs
                .borrow()
                .values()
                .filter(|record| record.swap_id == *swap_id)
                .cloned()
                .collect())
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

        fn set_provider_mode(&self, enabled: bool) -> Result<(), LiquidityStoreError> {
            *self.provider_mode.borrow_mut() = enabled;
            *self.provider_mode_writes.borrow_mut() += 1;
            Ok(())
        }

        fn get_provider_mode(&self) -> Result<bool, LiquidityStoreError> {
            Ok(*self.provider_mode.borrow())
        }
    }

    #[derive(Clone)]
    struct TestLiquidityChain {
        events: Shared<Vec<&'static str>>,
        outpoint: OutPoint,
        label: Option<&'static str>,
        store: Option<TestLiquidityStore>,
        fail_next_claim: bool,
        fail_next_loop_in_lock: bool,
        fail_next_loop_in_broadcast: bool,
        persist_loop_in_lock_before_failure: bool,
        reject_observed_loop_in_lock: bool,
        observed_loop_out_payout_results: Vec<Result<(), PayoutValidationError>>,
        provider_claim_watch_results: Vec<Result<(), String>>,
        claim_preimages: Vec<Hash256>,
        payout_locks: Shared<Vec<(ckb_types::packed::Script, ckb_types::packed::Script)>>,
        loop_in_funding_txs: Shared<Vec<String>>,
    }

    impl TestLiquidityChain {
        fn new(events: Shared<Vec<&'static str>>) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
                label: None,
                store: None,
                fail_next_claim: false,
                fail_next_loop_in_lock: false,
                fail_next_loop_in_broadcast: false,
                persist_loop_in_lock_before_failure: false,
                reject_observed_loop_in_lock: false,
                observed_loop_out_payout_results: Vec::new(),
                provider_claim_watch_results: Vec::new(),
                claim_preimages: Vec::new(),
                payout_locks: Shared::new(Vec::new()),
                loop_in_funding_txs: Shared::new(Vec::new()),
            }
        }

        fn new_with_label(events: Shared<Vec<&'static str>>, label: &'static str) -> Self {
            Self {
                events,
                outpoint: OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0),
                label: Some(label),
                store: None,
                fail_next_claim: false,
                fail_next_loop_in_lock: false,
                fail_next_loop_in_broadcast: false,
                persist_loop_in_lock_before_failure: false,
                reject_observed_loop_in_lock: false,
                observed_loop_out_payout_results: Vec::new(),
                provider_claim_watch_results: Vec::new(),
                claim_preimages: Vec::new(),
                payout_locks: Shared::new(Vec::new()),
                loop_in_funding_txs: Shared::new(Vec::new()),
            }
        }

        fn with_store(mut self, store: TestLiquidityStore) -> Self {
            self.store = Some(store);
            self
        }

        fn fail_next_claim(&mut self) {
            self.fail_next_claim = true;
        }

        fn fail_next_loop_in_lock(&mut self) {
            self.fail_next_loop_in_lock = true;
        }

        fn fail_next_loop_in_broadcast(&mut self) {
            self.fail_next_loop_in_broadcast = true;
        }

        fn fail_next_loop_in_broadcast_after_persisting_lock(&mut self) {
            self.fail_next_loop_in_broadcast = true;
            self.persist_loop_in_lock_before_failure = true;
        }

        fn reject_observed_loop_in_lock(&mut self) {
            self.reject_observed_loop_in_lock = true;
        }

        fn reject_observed_loop_out_payout_with(&mut self, error: impl Into<String>) {
            self.observed_loop_out_payout_results =
                vec![Err(PayoutValidationError::Definitive(error.into()))];
        }

        fn set_provider_claim_watch_results(&mut self, results: Vec<Result<(), String>>) {
            self.provider_claim_watch_results = results;
        }

        fn set_observed_loop_out_payout_results(
            &mut self,
            results: Vec<Result<(), PayoutValidationError>>,
        ) {
            self.observed_loop_out_payout_results = results;
        }

        fn loop_in_funding_txs(&self) -> Vec<String> {
            self.loop_in_funding_txs.borrow().clone()
        }
    }

    #[async_trait]
    impl LoopOutChainAdapter for TestLiquidityChain {
        type Error = String;

        async fn reserve_payout_lock_outpoint(
            &mut self,
            quote: &LoopOutQuoteTerms,
        ) -> Result<OutPoint, Self::Error> {
            if self.label.is_some_and(|label| label.starts_with("ckb")) {
                let store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| "missing store".to_string())?;
                store
                    .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                        swap_id: quote.quote_id,
                        role: LiquidityChainTxRole::Payout,
                        tx_hash: [7u8; 32].into(),
                        outpoint: Some(self.outpoint.clone()),
                        status: LiquidityChainTxStatus::Planned,
                        failure_reason: None,
                        created_at: now_ms(),
                        updated_at: now_ms(),
                    })
                    .map_err(|error| error.to_string())?;
            } else if self.label.is_some_and(|label| label.starts_with("runtime")) {
                self.events.borrow_mut().push("reserve_payout");
            }
            Ok(self.outpoint.clone())
        }

        async fn broadcast_payout_lock(
            &mut self,
            quote: &LoopOutQuoteTerms,
            _outpoint: &OutPoint,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            let event = match self.label {
                Some("chain") => "chain_broadcast_payout",
                _ => "broadcast_payout",
            };
            self.payout_locks
                .borrow_mut()
                .push((quote.claimant_lock.clone(), quote.refund_lock.clone()));
            if self.label.is_some_and(|label| label.starts_with("ckb")) {
                self.events.borrow_mut().push("send_tx");
                if self.label == Some("ckb_send_failure") {
                    return Err("chain operation failed: mock send tx error".to_string());
                }
            } else {
                self.events.borrow_mut().push(event);
            }
            Ok(())
        }

        async fn broadcast_claim(
            &mut self,
            request: LoopOutClaimRequest,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
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
            if let Some(store) = &self.store {
                if store
                    .get_liquidity_chain_tx(&request.swap_id, LiquidityChainTxRole::Claim)
                    .map_err(|error| error.to_string())?
                    .is_some()
                {
                    store
                        .update_liquidity_chain_tx_status(
                            &request.swap_id,
                            LiquidityChainTxRole::Claim,
                            LiquidityChainTxStatus::Broadcast,
                            None,
                            now_ms(),
                        )
                        .map_err(|error| error.to_string())?;
                } else {
                    store
                        .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                            swap_id: request.swap_id,
                            role: LiquidityChainTxRole::Claim,
                            tx_hash: [8u8; 32].into(),
                            outpoint: None,
                            status: LiquidityChainTxStatus::Broadcast,
                            failure_reason: None,
                            created_at: now_ms(),
                            updated_at: now_ms(),
                        })
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        }

        async fn watch_payout_lock(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            if self.label.is_some_and(|label| label.starts_with("ckb")) {
                self.events.borrow_mut().push("create_tx_tracer");
            }
            self.events.borrow_mut().push("watch_payout");
            Ok(())
        }

        async fn watch_loop_in_lock(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("watch_loop_in_lock");
            Ok(())
        }

        async fn broadcast_loop_in_lock(
            &mut self,
            quote: &LoopOutQuoteTerms,
            funding_tx: &str,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("broadcast_loop_in_lock");
            self.loop_in_funding_txs
                .borrow_mut()
                .push(funding_tx.to_string());
            if self.persist_loop_in_lock_before_failure {
                self.persist_loop_in_lock_before_failure = false;
                let store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| "missing store".to_string())?;
                store
                    .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                        swap_id: quote.quote_id,
                        role: LiquidityChainTxRole::Payout,
                        tx_hash: [37u8; 32].into(),
                        outpoint: Some(self.outpoint.clone()),
                        status: LiquidityChainTxStatus::Rejected,
                        failure_reason: Some("loop in broadcast failed".to_string()),
                        created_at: now_ms(),
                        updated_at: now_ms(),
                    })
                    .map_err(|error| error.to_string())?;
            }
            if self.fail_next_loop_in_broadcast {
                self.fail_next_loop_in_broadcast = false;
                return Err("loop in broadcast failed".to_string());
            }
            Ok(())
        }

        fn ensure_loop_in_lock_available(&mut self, _funding_tx: &str) -> Result<(), Self::Error> {
            if self.fail_next_loop_in_lock {
                self.fail_next_loop_in_lock = false;
                return Err("loop in lock unavailable".to_string());
            }
            Ok(())
        }

        async fn validate_observed_loop_in_lock(
            &mut self,
            quote: &LoopOutQuoteTerms,
            outpoint: &OutPoint,
        ) -> Result<(), Self::Error> {
            self.events
                .borrow_mut()
                .push("validate_observed_loop_in_lock");
            if self.reject_observed_loop_in_lock {
                self.reject_observed_loop_in_lock = false;
                return Err("observed loop in lock does not match quote".to_string());
            }
            if outpoint.tx_hash() == Byte32::default() || quote.claimant_lock == Default::default()
            {
                return Err("observed loop in lock does not match quote".to_string());
            }
            Ok(())
        }

        async fn validate_observed_loop_out_payout(
            &mut self,
            _quote: &LoopOutQuoteTerms,
            outpoint: &OutPoint,
        ) -> Result<(), PayoutValidationError> {
            self.events
                .borrow_mut()
                .push("validate_observed_loop_out_payout");
            if !self.observed_loop_out_payout_results.is_empty() {
                return self.observed_loop_out_payout_results.remove(0);
            }
            if outpoint.tx_hash() == Byte32::default() {
                return Err(PayoutValidationError::Definitive(
                    "observed loop out payout does not match quote".to_string(),
                ));
            }
            Ok(())
        }

        async fn watch_claim(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("watch_claim");
            Ok(())
        }

        async fn watch_provider_claim(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("watch_provider_claim");
            if self.provider_claim_watch_results.is_empty() {
                Ok(())
            } else {
                self.provider_claim_watch_results.remove(0)
            }
        }

        async fn broadcast_refund(
            &mut self,
            _record: &LiquiditySwapRecord,
        ) -> Result<(), Self::Error> {
            self.events.borrow_mut().push("broadcast_refund");
            Ok(())
        }

        async fn watch_refund(
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
        registered_invoices: Shared<Vec<Hash256>>,
        registered_invoice_amounts: Shared<Vec<u128>>,
        fail_registration: Shared<bool>,
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
                registered_invoices: Shared::new(Vec::new()),
                registered_invoice_amounts: Shared::new(Vec::new()),
                fail_registration: Shared::new(false),
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
                registered_invoices: Shared::new(Vec::new()),
                registered_invoice_amounts: Shared::new(Vec::new()),
                fail_registration: Shared::new(false),
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
                registered_invoices: Shared::new(Vec::new()),
                registered_invoice_amounts: Shared::new(Vec::new()),
                fail_registration: Shared::new(false),
            };
            (payment, send)
        }

        fn requests(&self) -> Vec<crate::liquidity::payment::LoopOutPaymentRequest> {
            self.requests.borrow().clone()
        }

        fn registered_invoices(&self) -> Vec<Hash256> {
            self.registered_invoices.borrow().clone()
        }

        fn registered_invoice_amounts(&self) -> Vec<u128> {
            self.registered_invoice_amounts.borrow().clone()
        }

        fn fail_next_registration(&self) {
            *self.fail_registration.borrow_mut() = true;
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

        async fn register_provider_loop_out_invoice(
            &mut self,
            payment_hash: Hash256,
            _preimage: Hash256,
            amount: u128,
            _udt_type_script: Option<ckb_types::packed::Script>,
        ) -> Result<(), Self::Error> {
            let fail = {
                let mut flag = self.fail_registration.borrow_mut();
                let fail = *flag;
                *flag = false;
                fail
            };
            if fail {
                return Err("registration failed".to_string());
            }
            self.events.borrow_mut().push("register_invoice");
            self.registered_invoices.borrow_mut().push(payment_hash);
            self.registered_invoice_amounts.borrow_mut().push(amount);
            Ok(())
        }

        async fn reload_provider_loop_out_payment(
            &mut self,
            _payment_hash: Hash256,
        ) -> Result<LoopOutPaymentStatus, Self::Error> {
            self.events.borrow_mut().push("reload_provider_payment");
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
            let harness = Self::new("provider");
            harness.store.set_provider_mode(true).unwrap();
            harness
        }

        fn new_provider_with_asset() -> Self {
            let harness = Self::new_provider();
            harness
                .store
                .upsert_liquidity_asset(test_loop_out_quote(now_ms() + 60_000).asset)
                .unwrap();
            harness
        }

        fn new_provider_with_realistic_ckb_watcher() -> Self {
            Self::new_provider_with_chain_label("ckb_success")
        }

        fn new_provider_with_failing_send_tx() -> Self {
            Self::new_provider_with_chain_label("ckb_send_failure")
        }

        fn new_provider_with_chain_label(chain_label: &'static str) -> Self {
            let events = Shared::new(Vec::new());
            let store = TestLiquidityStore::new(events.clone(), "provider");
            store.set_provider_mode(true).unwrap();
            Self {
                events: events.clone(),
                store: store.clone(),
                chain: TestLiquidityChain::new_with_label(events.clone(), chain_label)
                    .with_store(store.clone()),
                payment: TestLoopOutPayment::new_with_label(events, "runtime"),
            }
        }

        fn new(label: &'static str) -> Self {
            let events = Shared::new(Vec::new());
            let store = TestLiquidityStore::new(events.clone(), label);
            Self {
                events: events.clone(),
                store: store.clone(),
                chain: TestLiquidityChain::new_with_label(
                    events.clone(),
                    match label {
                        "client" => "runtime_client",
                        "provider" => "runtime_provider",
                        _ => "runtime",
                    },
                )
                .with_store(store.clone()),
                payment: TestLoopOutPayment::new_with_label(events, "runtime"),
            }
        }

        fn loop_out_quote_terms(&self) -> LoopOutQuoteTerms {
            let mut quote = test_loop_out_quote(now_ms() + 60_000);
            quote.payment_preimage = Some([4u8; 32].into());
            quote
        }

        fn store_quote(&self, quote: LoopOutQuoteTerms) {
            self.store.insert_loop_out_quote(quote, now_ms()).unwrap();
        }

        fn use_fake_payment_preimage_for_quote(&self, quote: &mut LiquidityQuoteEnvelope) {
            let quote_id = quote.quote_id.into();
            let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash([4u8; 32]).into();
            quote.payment_hash = payment_hash.into();
            self.store
                .quotes
                .borrow_mut()
                .get_mut(&quote_id)
                .unwrap()
                .payment_hash = payment_hash;
        }

        fn import_provider_quote(&self, provider: &RuntimeActorHarness, quote_id: Hash256) {
            let quote = provider
                .store
                .get_loop_out_quote(&quote_id)
                .unwrap()
                .unwrap();
            self.store_quote(quote);
        }

        fn swap_state(&self, swap_id: Hash256) -> LiquiditySwapState {
            self.store
                .get_liquidity_swap(&swap_id)
                .unwrap()
                .unwrap()
                .state
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.borrow().clone()
        }

        fn chain_tx_record(
            &self,
            swap_id: Hash256,
            role: LiquidityChainTxRole,
        ) -> Option<LiquidityChainTxRecord> {
            self.store.get_liquidity_chain_tx(&swap_id, role).unwrap()
        }

        async fn call_loop_out(
            &self,
            quote_id: Hash256,
            max_provider_fee: u128,
            max_routing_fee: u128,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| LiquidityActorMessage::LoopOut(
                LoopOutParams {
                    quote_id: quote_id.into(),
                    max_provider_fee,
                    max_routing_fee,
                    payout_outpoint: None,
                },
                reply
            ))
            .unwrap()
        }

        async fn call_loop_out_with_outpoint(
            &self,
            quote_id: Hash256,
            max_provider_fee: u128,
            max_routing_fee: u128,
            payout_outpoint: ckb_jsonrpc_types::OutPoint,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| LiquidityActorMessage::LoopOut(
                LoopOutParams {
                    quote_id: quote_id.into(),
                    max_provider_fee,
                    max_routing_fee,
                    payout_outpoint: Some(payout_outpoint),
                },
                reply
            ))
            .unwrap()
        }

        async fn call_provider_quote(
            &self,
            params: ProviderQuoteLoopOutParams,
        ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            let result = ractor::call!(actor, |reply| {
                LiquidityActorMessage::ProviderQuoteLoopOut(params, reply)
            })
            .unwrap();
            actor.stop(None);
            result
        }

        async fn call_quote(
            &self,
            params: QuoteLoopOutParams,
        ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            let result = ractor::call!(actor, |reply| LiquidityActorMessage::QuoteLoopOut(
                params, reply
            ))
            .unwrap();
            actor.stop(None);
            result
        }

        async fn call_quote_loop_in(
            &self,
            params: QuoteLoopInParams,
        ) -> Result<LiquidityQuoteEnvelope, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            let result = ractor::call!(actor, |reply| LiquidityActorMessage::QuoteLoopIn(
                params, reply
            ))
            .unwrap();
            actor.stop(None);
            result
        }

        async fn call_provider_accept_loop_in(
            &self,
            quote_id: Hash256,
            lock_tx_hash: Hash256,
            lock_output_index: u32,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| {
                LiquidityActorMessage::ProviderAcceptLoopIn(
                    ProviderAcceptLoopInParams {
                        quote_id: quote_id.into(),
                        lock_tx_hash: lock_tx_hash.into(),
                        lock_output_index,
                    },
                    reply,
                )
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

        async fn call_payment_settled(&self, swap_id: Hash256) {
            let actor = self.spawn_actor().await;
            actor
                .send_message(LiquidityActorMessage::PaymentSettled(
                    swap_id,
                    [4u8; 32].into(),
                ))
                .unwrap();
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
                .unwrap()
                .unwrap();
        }

        async fn call_claim_confirmed(&self, swap_id: Hash256) {
            let actor = self.spawn_actor().await;
            actor
                .send_message(LiquidityActorMessage::ClaimConfirmed(swap_id))
                .unwrap();
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
                .unwrap()
                .unwrap();
        }

        async fn call_provider_claim_observed(&self, swap_id: Hash256) {
            let actor = self.spawn_actor().await;
            actor
                .send_message(LiquidityActorMessage::ProviderClaimObserved(swap_id))
                .unwrap();
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
                .unwrap()
                .unwrap();
        }

        async fn call_provider_accept(
            &self,
            quote_id: Hash256,
        ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
            let actor = self.spawn_actor().await;
            ractor::call!(actor, |reply| {
                LiquidityActorMessage::ProviderAcceptLoopOut(
                    ProviderAcceptLoopOutParams {
                        quote_id: quote_id.into(),
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
            self.spawn_actor_with_handle().await.0
        }

        async fn spawn_actor_with_provider_pubkey(
            &self,
            provider_pubkey: Pubkey,
        ) -> (
            ractor::ActorRef<LiquidityActorMessage>,
            tokio::task::JoinHandle<()>,
        ) {
            ractor::Actor::spawn(
                None,
                LiquidityActor::<_, _, _>(std::marker::PhantomData),
                LiquidityActorArguments {
                    store: self.store.clone(),
                    payment: self.payment.clone(),
                    chain: self.chain.clone(),
                    provider_pubkey,
                    provider_funding_lock_script: deterministic_provider_funding_lock_script(),
                },
            )
            .await
            .unwrap()
        }

        async fn spawn_actor_with_handle(
            &self,
        ) -> (
            ractor::ActorRef<LiquidityActorMessage>,
            tokio::task::JoinHandle<()>,
        ) {
            ractor::Actor::spawn(
                None,
                LiquidityActor::<_, _, _>(std::marker::PhantomData),
                LiquidityActorArguments {
                    store: self.store.clone(),
                    payment: self.payment.clone(),
                    chain: self.chain.clone(),
                    provider_pubkey: deterministic_provider_pubkey(),
                    provider_funding_lock_script: deterministic_provider_funding_lock_script(),
                },
            )
            .await
            .unwrap()
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
                provider_pubkey: deterministic_provider_pubkey(),
                provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            },
        )
        .await
        .unwrap();
        actor
    }

    fn recovery_swap(seed: u8, state: LiquiditySwapState) -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            swap_id: [seed; 32].into(),
            quote_id: [seed; 32].into(),
            role: LiquiditySwapRole::Client,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: "ckb".to_string(),
            state,
            payment_hash: HashAlgorithm::CkbHash.hash([4u8; 32]).into(),
            payment_preimage: Some([4u8; 32].into()),
            amount: 1_000,
            onchain_outpoint: Some(OutPoint::new(
                Byte32::from_slice(&[seed.saturating_add(40); 32]).unwrap(),
                0,
            )),
            payout_deadline: Some(2_000),
            refund_after_lock_time: 3_000,
            expires_at: 4_000,
            failure_reason: None,
            created_at: 1_000,
            updated_at: 1_000,
        }
    }

    fn insert_recovered_client_payout_locked(
        store: &TestLiquidityStore,
        seed: u8,
    ) -> (LiquiditySwapRecord, LoopOutQuoteTerms) {
        let swap = recovery_swap(seed, LiquiditySwapState::PayoutLocked);
        let quote = LoopOutQuoteTerms {
            quote_id: swap.quote_id,
            payment_hash: swap.payment_hash,
            ..test_loop_out_quote(now_ms() + 60_000)
        };
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: swap.swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [seed.saturating_add(40); 32].into(),
                outpoint: swap.onchain_outpoint.clone(),
                status: LiquidityChainTxStatus::Confirmed,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        (swap, quote)
    }

    async fn call_resume_non_terminal(actor: ractor::ActorRef<LiquidityActorMessage>) -> usize {
        ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
            .unwrap()
            .unwrap()
    }

    async fn call_resume_non_terminal_result(
        actor: ractor::ActorRef<LiquidityActorMessage>,
    ) -> Result<usize, LiquidityLoopOutError> {
        ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal).unwrap()
    }

    async fn call_set_provider_mode(
        actor: &ractor::ActorRef<LiquidityActorMessage>,
        enabled: bool,
    ) -> Result<LiquidityProviderStatus, LiquidityLoopOutError> {
        ractor::call!(actor, |reply| {
            LiquidityActorMessage::SetLiquidityProviderMode(
                SetLiquidityProviderModeParams { enabled },
                reply,
            )
        })
        .unwrap()
    }

    async fn call_loop_in(
        actor: ractor::ActorRef<LiquidityActorMessage>,
        quote_id: Hash256,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        call_loop_in_with_funding_tx(actor, quote_id, "0x01").await
    }

    async fn call_loop_in_with_funding_tx(
        actor: ractor::ActorRef<LiquidityActorMessage>,
        quote_id: Hash256,
        funding_tx: &str,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        ractor::call!(actor, |reply| LiquidityActorMessage::LoopIn(
            LoopInParams {
                quote_id: quote_id.into(),
                funding_tx: funding_tx.to_string(),
            },
            reply,
        ))
        .unwrap()
    }

    async fn call_provider_accept_loop_out(
        actor: &ractor::ActorRef<LiquidityActorMessage>,
        quote_id: Hash256,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        ractor::call!(actor, |reply| {
            LiquidityActorMessage::ProviderAcceptLoopOut(
                ProviderAcceptLoopOutParams {
                    quote_id: quote_id.into(),
                },
                reply,
            )
        })
        .unwrap()
    }

    async fn call_provider_accept_loop_in(
        actor: &ractor::ActorRef<LiquidityActorMessage>,
        quote_id: Hash256,
        lock_tx_hash: Hash256,
        lock_output_index: u32,
    ) -> Result<LiquiditySwapResponse, LiquidityLoopOutError> {
        ractor::call!(actor, |reply| {
            LiquidityActorMessage::ProviderAcceptLoopIn(
                ProviderAcceptLoopInParams {
                    quote_id: quote_id.into(),
                    lock_tx_hash: lock_tx_hash.into(),
                    lock_output_index,
                },
                reply,
            )
        })
        .unwrap()
    }

    async fn call_list_liquidity_chain_transactions(
        actor: &ractor::ActorRef<LiquidityActorMessage>,
        swap_id: Hash256,
    ) -> Result<ListLiquidityChainTransactionsResponse, LiquidityLoopOutError> {
        ractor::call!(actor, |reply| {
            LiquidityActorMessage::ListLiquidityChainTransactions(
                ListLiquidityChainTransactionsParams {
                    swap_id: swap_id.into(),
                },
                reply,
            )
        })
        .unwrap()
    }

    fn loop_in_swap_record(swap_id: Hash256) -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            swap_id,
            quote_id: swap_id,
            role: LiquiditySwapRole::Client,
            swap_kind: LiquiditySwapKind::LoopIn,
            asset_id: "ckb".to_string(),
            state: LiquiditySwapState::OnchainLocked,
            payment_hash: [7u8; 32].into(),
            payment_preimage: None,
            amount: 1_000,
            onchain_outpoint: None,
            payout_deadline: None,
            refund_after_lock_time: 1_000,
            expires_at: 2_000,
            failure_reason: None,
            created_at: 10,
            updated_at: 10,
        }
    }

    fn liquidity_chain_tx_record(
        swap_id: Hash256,
        role: LiquidityChainTxRole,
        tx_seed: u8,
    ) -> LiquidityChainTxRecord {
        LiquidityChainTxRecord {
            swap_id,
            role,
            tx_hash: [tx_seed; 32].into(),
            outpoint: None,
            status: LiquidityChainTxStatus::Planned,
            failure_reason: None,
            created_at: 10,
            updated_at: 10,
        }
    }

    #[tokio::test]
    async fn list_liquidity_chain_transactions_labels_loop_in_payout_and_orders_stably() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let swap_id: Hash256 = [5u8; 32].into();
        store
            .insert_liquidity_swap(loop_in_swap_record(swap_id))
            .unwrap();
        for (role, seed) in [
            (LiquidityChainTxRole::Refund, 3u8),
            (LiquidityChainTxRole::Payout, 1u8),
            (LiquidityChainTxRole::Claim, 2u8),
        ] {
            store
                .insert_liquidity_chain_tx(liquidity_chain_tx_record(swap_id, role, seed))
                .unwrap();
        }

        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        let response = call_list_liquidity_chain_transactions(&actor, swap_id)
            .await
            .unwrap();

        let roles: Vec<_> = response.transactions.iter().map(|tx| tx.role).collect();
        assert_eq!(
            roles,
            vec![
                LiquidityChainTransactionRole::LoopInLock,
                LiquidityChainTransactionRole::Claim,
                LiquidityChainTransactionRole::Refund,
            ]
        );
    }

    #[tokio::test]
    async fn list_liquidity_chain_transactions_labels_loop_out_payout() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let swap = recovery_swap(11, LiquiditySwapState::PayoutLocked);
        let swap_id = swap.swap_id;
        store.insert_liquidity_swap(swap).unwrap();
        store
            .insert_liquidity_chain_tx(liquidity_chain_tx_record(
                swap_id,
                LiquidityChainTxRole::Payout,
                1,
            ))
            .unwrap();

        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        let response = call_list_liquidity_chain_transactions(&actor, swap_id)
            .await
            .unwrap();

        assert_eq!(response.transactions.len(), 1);
        assert_eq!(
            response.transactions[0].role,
            LiquidityChainTransactionRole::Payout
        );
        assert_eq!(response.transactions[0].status, "planned");
    }

    #[tokio::test]
    async fn list_liquidity_chain_transactions_returns_empty_for_unknown_swap() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        let response = call_list_liquidity_chain_transactions(&actor, [42u8; 32].into())
            .await
            .unwrap();

        assert!(response.transactions.is_empty());
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

    #[tokio::test]
    async fn resume_non_terminal_schedules_every_m3_loop_out_state_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let states = [
            LiquiditySwapState::PayoutPending,
            LiquiditySwapState::PayoutLocked,
            LiquiditySwapState::PaymentInFlight,
            LiquiditySwapState::PaymentSettled,
            LiquiditySwapState::ClaimPending,
            LiquiditySwapState::RefundPending,
        ];
        for (index, state) in states.into_iter().enumerate() {
            let swap = recovery_swap(index as u8 + 1, state);
            store.insert_liquidity_swap(swap.clone()).unwrap();
            store
                .insert_loop_out_quote(
                    LoopOutQuoteTerms {
                        quote_id: swap.quote_id,
                        payment_hash: swap.payment_hash,
                        ..test_loop_out_quote(now_ms() + 60_000)
                    },
                    now_ms(),
                )
                .unwrap();
        }
        events.borrow_mut().clear();
        let (payment, _release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(
                events.clone(),
                vec![LoopOutPaymentStatus::InFlight],
            );
        let actor = spawn_test_liquidity_actor(
            store,
            payment,
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 6);
        wait_for_event(&events, "send_payment").await;
        wait_for_event(&events, "reload_payment").await;
        assert_eq!(event_count(&events, "watch_payout"), 1);
        assert_eq!(event_count(&events, "send_payment"), 1);
        assert_eq!(event_count(&events, "reload_payment"), 1);
        assert_eq!(event_count(&events, "broadcast_claim"), 2);
        assert_eq!(event_count(&events, "watch_claim"), 2);
        assert_eq!(event_count(&events, "watch_refund"), 1);
    }

    #[tokio::test]
    async fn claim_pending_recovery_broadcasts_claim_and_schedules_watch() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        store
            .insert_liquidity_swap(recovery_swap(7, LiquiditySwapState::ClaimPending))
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "broadcast_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 1);
    }

    #[tokio::test]
    async fn client_payout_pending_recovery_without_outpoint_passively_watches_payout() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut swap = recovery_swap(9, LiquiditySwapState::PayoutPending);
        swap.onchain_outpoint = None;
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_loop_out_quote(
                LoopOutQuoteTerms {
                    quote_id: [9u8; 32].into(),
                    payment_hash: HashAlgorithm::CkbHash.hash([4u8; 32]).into(),
                    ..test_loop_out_quote(now_ms() + 60_000)
                },
                now_ms(),
            )
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_payout"), 1);
        assert_eq!(event_count(&events, "broadcast_payout"), 0);
        assert_eq!(event_count(&events, "reserve_payout"), 0);
    }

    #[tokio::test]
    async fn provider_payout_pending_recovery_without_outpoint_fails_closed_without_watch() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut swap = recovery_swap(18, LiquiditySwapState::PayoutPending);
        swap.role = LiquiditySwapRole::Provider;
        swap.onchain_outpoint = None;
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 0);
        assert_eq!(event_count(&events, "watch_payout"), 0);
        assert_eq!(event_count(&events, "broadcast_payout"), 0);
        assert_eq!(event_count(&events, "reserve_payout"), 0);
        assert_eq!(
            store
                .get_liquidity_swap(&swap.swap_id)
                .unwrap()
                .unwrap()
                .failure_reason,
            Some("payout recovery missing persisted outpoint".to_string())
        );
    }

    #[tokio::test]
    async fn resume_non_terminal_repeated_call_schedules_watches_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let payout_pending = recovery_swap(10, LiquiditySwapState::PayoutPending);
        let mut provider_payout_locked = recovery_swap(11, LiquiditySwapState::PayoutLocked);
        provider_payout_locked.role = LiquiditySwapRole::Provider;
        let claim_pending = recovery_swap(12, LiquiditySwapState::ClaimPending);
        let refund_pending = recovery_swap(13, LiquiditySwapState::RefundPending);
        for swap in [
            payout_pending,
            provider_payout_locked.clone(),
            claim_pending,
            refund_pending,
        ] {
            store.insert_liquidity_swap(swap).unwrap();
        }
        store
            .insert_loop_out_quote(
                LoopOutQuoteTerms {
                    quote_id: provider_payout_locked.quote_id,
                    payment_hash: provider_payout_locked.payment_hash,
                    payment_preimage: Some([4u8; 32].into()),
                    ..test_loop_out_quote(now_ms() + 60_000)
                },
                now_ms(),
            )
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let first = call_resume_non_terminal(actor.clone()).await;
        let second = call_resume_non_terminal(actor).await;

        assert_eq!(first, 4);
        assert_eq!(second, 0);
        assert_eq!(event_count(&events, "watch_payout"), 1);
        assert_eq!(event_count(&events, "register_invoice"), 1);
        assert_eq!(event_count(&events, "broadcast_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 1);
        assert_eq!(event_count(&events, "watch_refund"), 1);
    }

    #[tokio::test]
    async fn refund_pending_recovery_without_outpoint_fails_closed_without_refund_action() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut refund_pending = recovery_swap(14, LiquiditySwapState::RefundPending);
        refund_pending.onchain_outpoint = None;
        store.insert_liquidity_swap(refund_pending.clone()).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 0);
        assert_eq!(event_count(&events, "broadcast_refund"), 0);
        assert_eq!(event_count(&events, "watch_refund"), 0);
        assert_eq!(
            store
                .get_liquidity_swap(&refund_pending.swap_id)
                .unwrap()
                .unwrap()
                .failure_reason,
            Some("refund recovery missing persisted outpoint".to_string())
        );
    }

    #[tokio::test]
    async fn refund_pending_recovery_broadcasts_refund_and_schedules_watch() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut refund_pending = recovery_swap(15, LiquiditySwapState::RefundPending);
        refund_pending.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(refund_pending).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "broadcast_refund"), 1);
        assert_eq!(event_count(&events, "watch_refund"), 1);
    }

    #[tokio::test]
    async fn loop_in_client_refund_pending_recovery_broadcasts_refund_and_schedules_watch() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut refund_pending = recovery_swap(16, LiquiditySwapState::RefundPending);
        refund_pending.swap_kind = LiquiditySwapKind::LoopIn;
        refund_pending.role = LiquiditySwapRole::Client;
        store.insert_liquidity_swap(refund_pending).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "broadcast_refund"), 1);
        assert_eq!(event_count(&events, "watch_refund"), 1);
    }

    #[tokio::test]
    async fn refund_confirmed_marks_refund_tx_record_confirmed_and_swap_refunded() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut refund_pending = recovery_swap(17, LiquiditySwapState::RefundPending);
        refund_pending.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(refund_pending.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: refund_pending.swap_id,
                role: LiquidityChainTxRole::Refund,
                tx_hash: [10u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::RefundConfirmed(
                refund_pending.swap_id,
            ))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(
            store
                .get_liquidity_chain_tx(&refund_pending.swap_id, LiquidityChainTxRole::Refund)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
        assert_eq!(
            store
                .get_liquidity_swap(&refund_pending.swap_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Refunded
        );
    }

    #[tokio::test]
    async fn loop_in_client_refund_confirmed_marks_refund_tx_confirmed_and_swap_refunded() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut refund_pending = recovery_swap(20, LiquiditySwapState::RefundPending);
        refund_pending.swap_kind = LiquiditySwapKind::LoopIn;
        refund_pending.role = LiquiditySwapRole::Client;
        store.insert_liquidity_swap(refund_pending.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: refund_pending.swap_id,
                role: LiquidityChainTxRole::Refund,
                tx_hash: [13u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::RefundConfirmed(
                refund_pending.swap_id,
            ))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(
            store
                .get_liquidity_chain_tx(&refund_pending.swap_id, LiquidityChainTxRole::Refund)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
        assert_eq!(
            store
                .get_liquidity_swap(&refund_pending.swap_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Refunded
        );
    }

    #[tokio::test]
    async fn rejected_watched_refund_tx_is_retryable_in_same_process() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut refund_pending = recovery_swap(18, LiquiditySwapState::RefundPending);
        refund_pending.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(refund_pending.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: refund_pending.swap_id,
                role: LiquidityChainTxRole::Refund,
                tx_hash: [11u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 1);
        assert_eq!(event_count(&events, "watch_refund"), 1);
        actor
            .send_message(LiquidityActorMessage::ChainTxRejected(
                refund_pending.swap_id,
                LiquidityChainTxRole::Refund,
                "rejected".to_string(),
            ))
            .unwrap();
        for _ in 0..250 {
            if store
                .get_liquidity_chain_tx(&refund_pending.swap_id, LiquidityChainTxRole::Refund)
                .unwrap()
                .unwrap()
                .status
                == LiquidityChainTxStatus::Rejected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            store
                .get_liquidity_chain_tx(&refund_pending.swap_id, LiquidityChainTxRole::Refund)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Rejected
        );
        assert_eq!(call_resume_non_terminal(actor).await, 1);
        assert_eq!(event_count(&events, "broadcast_refund"), 1);
        assert_eq!(event_count(&events, "watch_refund"), 2);
    }

    #[tokio::test]
    async fn late_chain_tx_rejection_does_not_downgrade_confirmed_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut refund_pending = recovery_swap(19, LiquiditySwapState::Refunded);
        refund_pending.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(refund_pending.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: refund_pending.swap_id,
                role: LiquidityChainTxRole::Refund,
                tx_hash: [12u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Confirmed,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::ChainTxRejected(
                refund_pending.swap_id,
                LiquidityChainTxRole::Refund,
                "late rejected".to_string(),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            store
                .get_liquidity_chain_tx(&refund_pending.swap_id, LiquidityChainTxRole::Refund)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn resume_non_terminal_repeated_provider_payment_settled_watches_claim_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut payment_settled = recovery_swap(16, LiquiditySwapState::PaymentSettled);
        payment_settled.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(payment_settled).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let first = call_resume_non_terminal(actor.clone()).await;
        let second = call_resume_non_terminal(actor).await;

        assert_eq!(first, 1);
        assert_eq!(second, 0);
        assert_eq!(event_count(&events, "watch_provider_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 0);
    }

    #[tokio::test]
    async fn provider_claim_pending_recovery_uses_provider_claim_watch_only() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut claim_pending = recovery_swap(43, LiquiditySwapState::ClaimPending);
        claim_pending.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(claim_pending).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        assert_eq!(call_resume_non_terminal(actor).await, 1);
        assert_eq!(event_count(&events, "watch_provider_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 0);
        assert_eq!(event_count(&events, "broadcast_claim"), 0);
    }

    #[tokio::test]
    async fn failed_provider_claim_registration_does_not_poison_retry_guard() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut swap = recovery_swap(44, LiquiditySwapState::ClaimPending);
        swap.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_provider");
        chain.set_provider_claim_watch_results(vec![
            Err("conflicting tracer metadata".to_string()),
            Ok(()),
        ]);
        let mut state = LiquidityActorState {
            store,
            payment: TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            chain,
            provider_pubkey: deterministic_provider_pubkey(),
            provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            watched_payout_swaps: HashSet::new(),
            payout_validation_retries: HashMap::new(),
            definitive_payout_validation_failures: HashSet::new(),
            active_payment_swaps: HashSet::new(),
            watched_claim_swaps: HashSet::new(),
            active_refund_swaps: HashSet::new(),
            job_cancellation: CancellationToken::new(),
            jobs: TaskTracker::new(),
        };

        let error = state
            .resume_swap(swap.clone(), actor.clone())
            .await
            .expect_err("registration conflict must fail recovery");
        assert!(error.to_string().contains("conflicting tracer metadata"));
        assert!(!state.watched_claim_swaps.contains(&swap.swap_id));

        assert!(state.resume_swap(swap.clone(), actor).await.unwrap());
        assert!(state.watched_claim_swaps.contains(&swap.swap_id));
        assert_eq!(event_count(&events, "watch_provider_claim"), 2);
    }

    #[tokio::test]
    async fn payment_settled_recovery_schedules_claim_watch_after_successful_broadcast() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let swap = recovery_swap(19, LiquiditySwapState::PaymentSettled);
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "broadcast_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 1);
    }

    #[tokio::test]
    async fn resume_non_terminal_retries_payment_after_in_flight_recovery_exhausts() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        store
            .insert_liquidity_swap(recovery_swap(15, LiquiditySwapState::PaymentInFlight))
            .unwrap();
        let (payment, _release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(
                events.clone(),
                vec![
                    LoopOutPaymentStatus::InFlight,
                    LoopOutPaymentStatus::InFlight,
                    LoopOutPaymentStatus::InFlight,
                ],
            );
        let actor = spawn_test_liquidity_actor(
            store,
            payment,
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let first = call_resume_non_terminal(actor.clone()).await;
        let second = call_resume_non_terminal(actor.clone()).await;
        wait_for_event_count(&events, "reload_payment", 2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let third = call_resume_non_terminal(actor).await;
        wait_for_event_count(&events, "reload_payment", 3).await;

        assert_eq!(first, 1);
        assert_eq!(second, 0);
        assert_eq!(third, 1);
        assert_eq!(event_count(&events, "reload_payment"), 3);
    }

    #[tokio::test]
    async fn provider_payment_in_flight_recovery_resumes_payment_observation() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut swap = recovery_swap(24, LiquiditySwapState::PaymentInFlight);
        swap.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_loop_out_quote(
                LoopOutQuoteTerms {
                    quote_id: swap.quote_id,
                    payment_hash: swap.payment_hash,
                    payment_preimage: Some([4u8; 32].into()),
                    ..test_loop_out_quote(now_ms() + 60_000)
                },
                now_ms(),
            )
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        let first = call_resume_non_terminal(actor.clone()).await;
        let second = call_resume_non_terminal(actor.clone()).await;
        wait_for_event(&events, "reload_provider_payment").await;

        assert_eq!(first, 1);
        assert_eq!(second, 0);
        assert_eq!(event_count(&events, "register_invoice"), 1);
        assert_eq!(event_count(&events, "watch_payout"), 0);
        assert!(event_count(&events, "reload_provider_payment") >= 1);
    }

    #[tokio::test]
    async fn payment_settled_failure_clears_active_guard_for_future_recovery() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let swap = recovery_swap(23, LiquiditySwapState::PaymentInFlight);
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;
        let mut state = LiquidityActorState {
            store,
            payment: TestLoopOutPayment::with_pending_result_and_reload_statuses(
                events.clone(),
                vec![LoopOutPaymentStatus::InFlight],
            )
            .0,
            chain: TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
            provider_pubkey: deterministic_provider_pubkey(),
            provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            watched_payout_swaps: HashSet::new(),
            payout_validation_retries: HashMap::new(),
            definitive_payout_validation_failures: HashSet::new(),
            active_payment_swaps: HashSet::new(),
            watched_claim_swaps: HashSet::new(),
            active_refund_swaps: HashSet::new(),
            job_cancellation: CancellationToken::new(),
            jobs: TaskTracker::new(),
        };
        state.active_payment_swaps.insert(swap.swap_id);

        let error = state
            .handle_payment_settled(swap.swap_id, [9u8; 32].into(), actor.clone())
            .await
            .expect_err("mismatched preimage should fail");
        assert!(matches!(error, LiquidityLoopOutError::Chain(_)));
        let resumed = state.resume_swap(swap, actor).await.unwrap();

        assert!(resumed);
        wait_for_event(&events, "reload_payment").await;
        assert_eq!(event_count(&events, "reload_payment"), 1);
    }

    #[tokio::test]
    async fn payment_settled_recovery_without_preimage_fails_closed_without_claim_broadcast() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut swap = recovery_swap(8, LiquiditySwapState::PaymentSettled);
        swap.payment_preimage = None;
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 0);
        assert_eq!(event_count(&events, "broadcast_claim"), 0);
        assert_eq!(
            store
                .get_liquidity_swap(&swap.swap_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentSettled
        );
    }

    #[tokio::test]
    async fn payment_settled_recovery_failed_claim_broadcast_persists_claim_pending() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let swap = recovery_swap(17, LiquiditySwapState::PaymentSettled);
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        chain.fail_next_claim();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            chain,
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 0);
        assert_eq!(event_count(&events, "client_transition_claim_pending"), 1);
        assert_eq!(event_count(&events, "broadcast_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 0);
        let swap_after_failed_broadcast = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
        assert_eq!(
            swap_after_failed_broadcast.state,
            LiquiditySwapState::ClaimPending
        );
    }

    #[tokio::test]
    async fn resume_non_terminal_continues_after_bad_swap() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut bad = recovery_swap(20, LiquiditySwapState::PaymentSettled);
        bad.payment_preimage = None;
        let healthy = recovery_swap(21, LiquiditySwapState::ClaimPending);
        store.insert_liquidity_swap(bad).unwrap();
        store.insert_liquidity_swap(healthy).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "broadcast_claim"), 1);
        assert_eq!(event_count(&events, "watch_claim"), 1);
    }

    #[test]
    fn recovery_guards_are_pruned_for_terminal_continuations() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut state = LiquidityActorState {
            store,
            payment: TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            chain: TestLiquidityChain::new_with_label(events, "runtime_client"),
            provider_pubkey: deterministic_provider_pubkey(),
            provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            watched_payout_swaps: HashSet::new(),
            payout_validation_retries: HashMap::new(),
            definitive_payout_validation_failures: HashSet::new(),
            active_payment_swaps: HashSet::new(),
            watched_claim_swaps: HashSet::new(),
            active_refund_swaps: HashSet::new(),
            job_cancellation: CancellationToken::new(),
            jobs: TaskTracker::new(),
        };
        let swap_id = [22u8; 32].into();
        state.watched_payout_swaps.insert(swap_id);
        state.active_payment_swaps.insert(swap_id);
        state.watched_claim_swaps.insert(swap_id);
        state.active_refund_swaps.insert(swap_id);

        state.prune_recovery_guards(swap_id);

        assert!(state.watched_payout_swaps.is_empty());
        assert!(state.active_payment_swaps.is_empty());
        assert!(state.watched_claim_swaps.is_empty());
        assert!(state.active_refund_swaps.is_empty());
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

            self.client_store
                .insert_loop_out_quote(quote.clone(), now_ms)
                .unwrap();
            create_client_loop_out(
                &self.client_store,
                quote.clone(),
                now_ms,
                Some(self.chain.outpoint.clone()),
            )
            .unwrap();
            let actor = spawn_test_liquidity_actor(
                self.provider_store.clone(),
                self.payment.clone(),
                self.chain.clone(),
            )
            .await;
            accept_provider_loop_out(
                &self.provider_store,
                &mut self.chain,
                quote.clone(),
                now_ms,
                actor,
            )
            .await
            .unwrap();
            mark_provider_payout_locked(&self.provider_store, quote.quote_id, now_ms + 1).unwrap();
            let client_actor = spawn_test_liquidity_actor(
                self.client_store.clone(),
                self.payment.clone(),
                self.chain.clone(),
            )
            .await;
            client_actor
                .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
                .unwrap();
            wait_for_event(&self.events, "client_transition_claim_pending").await;
            mark_provider_payment_settled(&self.provider_store, quote.quote_id, now_ms + 2)
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

    fn deterministic_provider_funding_lock_script() -> ckb_types::packed::Script {
        let sk = SecretKey::from_slice(&[42; 32]).unwrap();
        let pubkey_hash = ckb_hash::blake2b_256(sk.public_key(SECP256K1).serialize());
        ckb_types::packed::Script::new_builder()
            .args(ckb_types::bytes::Bytes::from(pubkey_hash[0..20].to_vec()).pack())
            .build()
    }

    fn valid_client_invoice(amount: u128, payment_hash: Hash256) -> String {
        let (private_key, public_key) = gen_deterministic_secp256k1_keypair_tuple();
        InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(amount))
            .payment_hash(payment_hash)
            .payee_pub_key(public_key)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .expect("invoice")
            .to_string()
    }

    fn test_loop_out_quote(expires_at: u64) -> LoopOutQuoteTerms {
        let sk = SecretKey::from_slice(&[42; 32]).unwrap();
        LoopOutQuoteTerms {
            quote_id: [1u8; 32].into(),
            swap_kind: LiquiditySwapKind::LoopOut,
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
            payment_hash: HashAlgorithm::CkbHash.hash([4u8; 32]).into(),
            payment_preimage: None,
            expires_at,
            payout_deadline: expires_at + 10_000,
            refund_after_lock_time: expires_at + 20_000,
            claimant_lock: Default::default(),
            refund_lock: Default::default(),
            client_invoice: None,
        }
    }

    fn import_liquidity_quote_params(
        terms: &LoopOutQuoteTerms,
    ) -> fiber_json_types::ImportLiquidityQuoteParams {
        fiber_json_types::ImportLiquidityQuoteParams {
            quote: crate::liquidity::quote::liquidity_quote_envelope_from_terms(terms),
            max_provider_fee: terms.provider_fee,
            max_routing_fee: terms.routing_fee_limit,
        }
    }

    async fn import_liquidity_quote(
        actor: &ActorRef<LiquidityActorMessage>,
        params: fiber_json_types::ImportLiquidityQuoteParams,
    ) -> Result<fiber_json_types::LiquidityQuoteEnvelope, LiquidityLoopOutError> {
        ractor::call!(actor.clone(), |reply| {
            LiquidityActorMessage::ImportLiquidityQuote(params, reply)
        })
        .unwrap()
    }

    async fn spawn_import_liquidity_quote_actor(
        harness: &RuntimeActorHarness,
    ) -> (ActorRef<LiquidityActorMessage>, tokio::task::JoinHandle<()>) {
        ractor::Actor::spawn(
            None,
            LiquidityActor::<_, _, _>(std::marker::PhantomData),
            LiquidityActorArguments {
                store: harness.store.clone(),
                payment: harness.payment.clone(),
                chain: harness.chain.clone(),
                provider_pubkey: deterministic_provider_pubkey(),
                provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            },
        )
        .await
        .unwrap()
    }

    async fn stop_liquidity_actor(
        actor: ActorRef<LiquidityActorMessage>,
        handle: tokio::task::JoinHandle<()>,
    ) {
        actor.stop(None);
        handle.await.unwrap();
    }

    fn importable_loop_out_quote(expires_at: u64) -> LoopOutQuoteTerms {
        let mut terms = test_loop_out_quote(expires_at);
        terms.claimant_lock = script("import-claimant");
        terms.refund_lock = script("import-refund");
        terms
    }

    fn assert_same_envelope(
        actual: &fiber_json_types::LiquidityQuoteEnvelope,
        expected: &fiber_json_types::LiquidityQuoteEnvelope,
    ) {
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[tokio::test]
    async fn import_liquidity_quote_persists_first_quote() {
        let harness = RuntimeActorHarness::new_client();
        let terms = importable_loop_out_quote(now_ms() + 60_000);
        let (actor, handle) = spawn_import_liquidity_quote_actor(&harness).await;

        let envelope = import_liquidity_quote(&actor, import_liquidity_quote_params(&terms)).await;
        stop_liquidity_actor(actor, handle).await;
        let envelope = envelope.unwrap();

        assert_same_envelope(
            &envelope,
            &crate::liquidity::quote::liquidity_quote_envelope_from_terms(&terms),
        );
        assert_eq!(
            harness.store.get_loop_out_quote(&terms.quote_id).unwrap(),
            Some(terms)
        );
        assert_eq!(*harness.store.quote_writes.borrow(), 1);
    }

    #[tokio::test]
    async fn import_liquidity_quote_is_idempotent_for_identical_terms() {
        let harness = RuntimeActorHarness::new_client();
        let terms = importable_loop_out_quote(now_ms() + 60_000);
        let params = import_liquidity_quote_params(&terms);
        let expected = crate::liquidity::quote::liquidity_quote_envelope_from_terms(&terms);
        let (actor, handle) = spawn_import_liquidity_quote_actor(&harness).await;

        let (first, second) = tokio::join!(
            import_liquidity_quote(&actor, params.clone()),
            import_liquidity_quote(&actor, params),
        );
        stop_liquidity_actor(actor, handle).await;
        let first = first.unwrap();
        let second = second.unwrap();

        assert_same_envelope(&first, &expected);
        assert_same_envelope(&second, &expected);
        assert_eq!(
            harness.store.get_loop_out_quote(&terms.quote_id).unwrap(),
            Some(terms)
        );
        assert_eq!(*harness.store.quote_writes.borrow(), 1);
    }

    #[tokio::test]
    async fn import_liquidity_quote_rejects_conflicting_terms() {
        let harness = RuntimeActorHarness::new_client();
        let terms = importable_loop_out_quote(now_ms() + 60_000);
        let (actor, handle) = spawn_import_liquidity_quote_actor(&harness).await;
        let first = import_liquidity_quote(&actor, import_liquidity_quote_params(&terms)).await;
        let mut conflicting = terms.clone();
        conflicting.amount += 1;

        let error =
            import_liquidity_quote(&actor, import_liquidity_quote_params(&conflicting)).await;
        stop_liquidity_actor(actor, handle).await;
        first.unwrap();
        let error = error.unwrap_err();

        assert!(error.to_string().contains("conflict"));
        assert!(error.to_string().contains(&format!("{:?}", terms.quote_id)));
        assert_eq!(
            harness.store.get_loop_out_quote(&terms.quote_id).unwrap(),
            Some(terms)
        );
        assert_eq!(*harness.store.quote_writes.borrow(), 1);
    }

    #[tokio::test]
    async fn import_liquidity_quote_rejects_invalid_before_persistence() {
        let cases = [
            {
                let terms = importable_loop_out_quote(now_ms());
                import_liquidity_quote_params(&terms)
            },
            {
                let mut params =
                    import_liquidity_quote_params(&importable_loop_out_quote(now_ms() + 60_000));
                params.max_provider_fee -= 1;
                params
            },
            {
                let mut params =
                    import_liquidity_quote_params(&importable_loop_out_quote(now_ms() + 60_000));
                params.max_routing_fee -= 1;
                params
            },
        ];

        for params in cases {
            let harness = RuntimeActorHarness::new_client();
            let quote_id = params.quote.quote_id.into();
            let (actor, handle) = spawn_import_liquidity_quote_actor(&harness).await;

            let error = import_liquidity_quote(&actor, params).await;
            stop_liquidity_actor(actor, handle).await;
            error.unwrap_err();

            assert_eq!(harness.store.get_loop_out_quote(&quote_id).unwrap(), None);
            assert_eq!(*harness.store.quote_writes.borrow(), 0);
        }
    }

    fn test_loop_in_quote(expires_at: u64) -> LoopOutQuoteTerms {
        LoopOutQuoteTerms {
            quote_id: [2u8; 32].into(),
            swap_kind: LiquiditySwapKind::LoopIn,
            routing_fee_limit: 0,
            payout_deadline: expires_at,
            refund_after_lock_time: expires_at + 20_000,
            client_invoice: Some("lnbc-client-invoice".to_string()),
            ..test_loop_out_quote(expires_at)
        }
    }

    #[tokio::test]
    async fn loop_in_accept_persists_before_client_lock_broadcast() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let response = call_loop_in(actor, quote.quote_id).await.unwrap();

        assert_eq!(response.state, "OnchainLockPending");
        assert_eq!(
            events.borrow().as_slice(),
            ["client_insert_swap", "broadcast_loop_in_lock"]
        );
    }

    #[tokio::test]
    async fn loop_in_rejects_loop_out_quote_before_side_effects() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let error = call_loop_in(actor, quote.quote_id).await.unwrap_err();

        assert!(error.to_string().contains("loop in quote"));
        assert!(events.borrow().is_empty());
    }

    #[tokio::test]
    async fn loop_in_accept_uses_explicit_quote_kind_not_sentinel_fields() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = LoopOutQuoteTerms {
            quote_id: [33u8; 32].into(),
            swap_kind: LiquiditySwapKind::LoopIn,
            routing_fee_limit: 7,
            payout_deadline: now_ms() + 10_000,
            ..test_loop_out_quote(now_ms() + 60_000)
        };
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let response = call_loop_in(actor, quote.quote_id).await.unwrap();

        assert_eq!(response.state, "OnchainLockPending");
    }

    #[tokio::test]
    async fn loop_out_rejects_loop_in_quote_before_side_effects() {
        let harness = RuntimeActorHarness::new_client();
        let quote = test_loop_in_quote(now_ms() + 60_000);
        harness.store_quote(quote.clone());

        let error = harness
            .call_loop_out(quote.quote_id, quote.provider_fee, quote.routing_fee_limit)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("loop out quote"));
        assert!(harness.events().is_empty());
    }

    #[tokio::test]
    async fn loop_in_chain_failure_does_not_leave_pending_swap() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        chain.fail_next_loop_in_lock();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain,
        )
        .await;

        let error = call_loop_in(actor, quote.quote_id).await.unwrap_err();

        assert!(error.to_string().contains("loop in lock unavailable"));
        assert!(store.get_liquidity_swap(&quote.quote_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn loop_in_broadcast_failure_marks_swap_failed() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        chain.fail_next_loop_in_broadcast();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain,
        )
        .await;

        let error = call_loop_in(actor, quote.quote_id).await.unwrap_err();

        assert!(error.to_string().contains("loop in broadcast failed"));
        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::Failed);
        assert_eq!(
            swap.failure_reason,
            Some("loop in broadcast failed".to_string())
        );
    }

    #[tokio::test]
    async fn loop_in_post_persistence_broadcast_failure_preserves_pending_swap() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client")
            .with_store(store.clone());
        chain.fail_next_loop_in_broadcast_after_persisting_lock();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain,
        )
        .await;

        let error = call_loop_in(actor, quote.quote_id).await.unwrap_err();

        assert!(error.to_string().contains("loop in broadcast failed"));
        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::OnchainLockPending);
        assert_eq!(
            swap.failure_reason,
            Some("loop in broadcast failed".to_string())
        );
        assert!(store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn loop_in_retry_after_persisted_broadcast_failure_reaches_chain_retry() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client")
            .with_store(store.clone());
        chain.fail_next_loop_in_broadcast_after_persisting_lock();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain,
        )
        .await;

        let first_error = call_loop_in(actor.clone(), quote.quote_id)
            .await
            .unwrap_err();
        assert!(first_error.to_string().contains("loop in broadcast failed"));

        let retry = call_loop_in(actor, quote.quote_id).await.unwrap();

        assert_eq!(retry.state, "OnchainLockPending");
        assert_eq!(event_count(&events, "client_insert_swap"), 1);
        assert_eq!(event_count(&events, "broadcast_loop_in_lock"), 2);
    }

    #[tokio::test]
    async fn loop_in_accept_passes_funding_tx_to_chain_adapter() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;

        call_loop_in_with_funding_tx(actor, quote.quote_id, "local-wallet")
            .await
            .unwrap();

        assert_eq!(
            chain.loop_in_funding_txs(),
            vec!["local-wallet".to_string()]
        );
    }

    #[tokio::test]
    async fn resume_non_terminal_scans_loop_in_swaps() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        call_resume_non_terminal(actor).await;

        assert_eq!(
            store.listed_swap_kinds(),
            vec![LiquiditySwapKind::LoopOut, LiquiditySwapKind::LoopIn]
        );
    }

    #[tokio::test]
    async fn resume_non_terminal_does_not_run_loop_out_refund_for_loop_in_refund_pending() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut swap = recovery_swap(34, LiquiditySwapState::RefundPending);
        swap.role = LiquiditySwapRole::Provider;
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        store.insert_liquidity_swap(swap).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 0);
        assert_eq!(event_count(&events, "broadcast_refund"), 0);
    }

    #[tokio::test]
    async fn loop_in_lock_pending_recovery_watches_existing_lock_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut swap = recovery_swap(35, LiquiditySwapState::OnchainLockPending);
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.role = LiquiditySwapRole::Client;
        swap.onchain_outpoint = Some(OutPoint::new(Byte32::from_slice(&[35u8; 32]).unwrap(), 35));
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: swap.swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [36u8; 32].into(),
                outpoint: swap.onchain_outpoint.clone(),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_loop_in_lock"), 1);
        assert_eq!(event_count(&events, "watch_payout"), 0);
        assert_eq!(event_count(&events, "broadcast_refund"), 0);
    }

    #[tokio::test]
    async fn loop_in_lock_pending_recovery_watches_planned_lock_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut swap = recovery_swap(36, LiquiditySwapState::OnchainLockPending);
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.role = LiquiditySwapRole::Client;
        swap.onchain_outpoint = Some(OutPoint::new(Byte32::from_slice(&[36u8; 32]).unwrap(), 36));
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: swap.swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [37u8; 32].into(),
                outpoint: swap.onchain_outpoint.clone(),
                status: LiquidityChainTxStatus::Planned,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_loop_in_lock"), 1);
        assert_eq!(event_count(&events, "watch_payout"), 0);
    }

    #[tokio::test]
    async fn loop_in_lock_pending_recovery_fails_rejected_client_lock_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut swap = recovery_swap(43, LiquiditySwapState::OnchainLockPending);
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.role = LiquiditySwapRole::Client;
        swap.onchain_outpoint = Some(OutPoint::new(Byte32::from_slice(&[43u8; 32]).unwrap(), 43));
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: swap.swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [44u8; 32].into(),
                outpoint: swap.onchain_outpoint.clone(),
                status: LiquidityChainTxStatus::Rejected,
                failure_reason: Some("rejected".to_string()),
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_loop_in_lock"), 0);
        assert_eq!(
            store
                .get_liquidity_swap(&swap.swap_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Failed
        );
    }

    #[tokio::test]
    async fn provider_loop_in_lock_pending_recovery_watches_existing_lock_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut swap = recovery_swap(37, LiquiditySwapState::OnchainLockPending);
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.role = LiquiditySwapRole::Provider;
        swap.onchain_outpoint = Some(OutPoint::new(Byte32::from_slice(&[37u8; 32]).unwrap(), 37));
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: swap.swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [38u8; 32].into(),
                outpoint: swap.onchain_outpoint.clone(),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_loop_in_lock"), 1);
        assert_eq!(event_count(&events, "broadcast_loop_in_lock"), 0);
        assert_eq!(event_count(&events, "watch_payout"), 0);
    }

    #[tokio::test]
    async fn provider_loop_in_lock_pending_recovery_fails_rejected_lock_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut swap = recovery_swap(45, LiquiditySwapState::OnchainLockPending);
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.role = LiquiditySwapRole::Provider;
        swap.onchain_outpoint = Some(OutPoint::new(Byte32::from_slice(&[45u8; 32]).unwrap(), 45));
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: swap.swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [46u8; 32].into(),
                outpoint: swap.onchain_outpoint.clone(),
                status: LiquidityChainTxStatus::Rejected,
                failure_reason: Some("rejected".to_string()),
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_loop_in_lock"), 0);
        assert_eq!(
            store
                .get_liquidity_swap(&swap.swap_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Failed
        );
    }

    #[tokio::test]
    async fn loop_in_lock_confirmed_marks_payout_tx_record_confirmed() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_liquidity_swap(loop_in_record(&quote, LiquiditySwapRole::Client, now_ms()))
            .unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [38u8; 32].into(),
                outpoint: Some(OutPoint::new(Byte32::from_slice(&[38u8; 32]).unwrap(), 38)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::LoopInLockConfirmed(quote.quote_id))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn late_loop_in_lock_rejection_does_not_downgrade_confirmed_payout_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = LoopOutQuoteTerms {
            quote_id: [39u8; 32].into(),
            ..test_loop_in_quote(now_ms() + 60_000)
        };
        store
            .insert_liquidity_swap(loop_in_record(&quote, LiquiditySwapRole::Client, now_ms()))
            .unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [39u8; 32].into(),
                outpoint: Some(OutPoint::new(Byte32::from_slice(&[39u8; 32]).unwrap(), 39)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::LoopInLockConfirmed(quote.quote_id))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        actor
            .send_message(LiquidityActorMessage::ChainTxRejected(
                quote.quote_id,
                LiquidityChainTxRole::Payout,
                "late rejected".to_string(),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn provider_loop_in_payment_waits_for_onchain_lock_confirmation() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        store
            .insert_liquidity_swap(loop_in_record(
                &quote,
                LiquiditySwapRole::Provider,
                now_ms(),
            ))
            .unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [40u8; 32].into(),
                outpoint: Some(OutPoint::new(Byte32::from_slice(&[40u8; 32]).unwrap(), 40)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let (payment, _settle) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(events.clone(), vec![]);
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            payment,
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(event_count(&events, "send_payment"), 0);

        actor
            .send_message(LiquidityActorMessage::LoopInLockConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;

        let events = events.borrow().clone();
        let locked = events
            .iter()
            .position(|event| *event == "transition_other")
            .expect("on-chain lock transition");
        let in_flight = events
            .iter()
            .position(|event| *event == "provider_transition_payment_in_flight")
            .expect("payment in-flight transition");
        let send = events
            .iter()
            .position(|event| *event == "send_payment")
            .expect("payment send");
        assert!(locked < in_flight);
        assert!(in_flight < send);
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentInFlight
        );
    }

    #[tokio::test]
    async fn provider_loop_in_payment_uses_persisted_client_invoice_not_provider_target() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut quote = test_loop_in_quote(now_ms() + 60_000);
        quote.client_invoice = Some("lnbc-client-invoice".to_string());
        quote.routing_fee_limit = 19;
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
        swap.state = LiquiditySwapState::OnchainLocked;
        store.insert_liquidity_swap(swap).unwrap();
        let (payment, _settle) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(events.clone(), vec![]);
        let actor = spawn_test_liquidity_actor(
            store,
            payment.clone(),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        call_resume_non_terminal(actor).await;
        wait_for_event(&events, "send_payment").await;

        let request = payment.requests().pop().expect("payment request");
        assert_eq!(request.invoice, Some("lnbc-client-invoice".to_string()));
        assert_ne!(request.target_pubkey, Some(quote.provider));
        assert_eq!(request.max_fee_amount, 19);
    }

    #[tokio::test]
    async fn provider_loop_in_onchain_locked_recovery_fails_closed_without_client_invoice() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let mut quote = test_loop_in_quote(now_ms() + 60_000);
        quote.client_invoice = None;
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
        swap.state = LiquiditySwapState::OnchainLocked;
        store.insert_liquidity_swap(swap).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
        )
        .await;

        call_resume_non_terminal(actor).await;

        assert_eq!(event_count(&events, "send_payment"), 0);
        let failure_reason = store
            .get_liquidity_swap(&quote.quote_id)
            .unwrap()
            .unwrap()
            .failure_reason
            .unwrap();
        assert!(failure_reason.contains("client invoice"));
    }

    #[tokio::test]
    async fn provider_loop_in_claim_persists_preimage_before_chain_claim() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let mut swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
        swap.state = LiquiditySwapState::PaymentInFlight;
        store.insert_liquidity_swap(swap).unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_provider")
                .with_store(store.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::PaymentSettled(
                quote.quote_id,
                [4u8; 32].into(),
            ))
            .unwrap();
        wait_for_event(&events, "broadcast_claim").await;

        let persisted = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(persisted.payment_preimage, Some([4u8; 32].into()));
        assert_eq!(persisted.state, LiquiditySwapState::ClaimPending);
        let events = events.borrow().clone();
        let preimage = events
            .iter()
            .position(|event| *event == "persist_preimage")
            .expect("preimage persisted");
        let claim = events
            .iter()
            .position(|event| *event == "broadcast_claim")
            .expect("claim broadcast");
        assert!(preimage < claim);
    }

    #[tokio::test]
    async fn provider_loop_in_claim_confirmation_marks_success_and_claim_tx_confirmed() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        let mut swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
        swap.state = LiquiditySwapState::ClaimPending;
        swap.payment_preimage = Some([4u8; 32].into());
        store.insert_liquidity_swap(swap).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Claim,
                tx_hash: [41u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::ClaimConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "provider_transition_success").await;

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::Success
        );
    }

    #[test]
    fn loop_in_provider_claim_confirmation_uses_direction_aware_helper() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events, "provider");
        let quote = test_loop_in_quote(now_ms() + 60_000);
        let mut swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
        swap.state = LiquiditySwapState::ClaimPending;
        store.insert_liquidity_swap(swap).unwrap();

        mark_loop_in_provider_claim_confirmed(&store, quote.quote_id, now_ms()).unwrap();

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
    async fn liquidity_actor_loop_out_persists_before_payment_and_claim() {
        let harness = RuntimeActorHarness::new_client();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let response = harness
            .call_loop_out_with_outpoint(
                quote.quote_id,
                quote.provider_fee,
                quote.routing_fee_limit,
                test_json_payout_outpoint(),
            )
            .await
            .unwrap();

        assert_eq!(response.swap_id, quote.quote_id.into());
        assert_eq!(response.state, "PayoutPending");
        assert_eq!(
            harness.events(),
            vec![
                "client_insert_created",
                "client_transition_quoted",
                "client_transition_payout_pending",
                "persist_outpoint",
                "persist_payout_tx",
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
                "persist_outpoint",
                "persist_payout_tx",
                "watch_payout",
                "payout_confirmed",
                "validate_observed_loop_out_payout",
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
                "register_invoice",
            ]
        );
    }

    #[tokio::test]
    async fn provider_accept_loop_out_returns_payout_outpoint() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let response = harness.call_provider_accept(quote.quote_id).await.unwrap();

        let persisted = harness
            .store
            .get_liquidity_swap(&quote.quote_id)
            .unwrap()
            .unwrap();
        let expected: ckb_jsonrpc_types::OutPoint = persisted
            .onchain_outpoint
            .expect("provider swap persists payout outpoint")
            .into();
        assert_eq!(response.payout_outpoint, Some(expected));
    }

    #[tokio::test]
    async fn client_loop_out_persists_payout_outpoint_before_watching() {
        let harness = RuntimeActorHarness::new_client();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let packed_outpoint = OutPoint::new(Byte32::from_slice(&[42u8; 32]).unwrap(), 1);
        let json_outpoint: ckb_jsonrpc_types::OutPoint = packed_outpoint.clone().into();

        let response = harness
            .call_loop_out_with_outpoint(
                quote.quote_id,
                quote.provider_fee,
                quote.routing_fee_limit,
                json_outpoint,
            )
            .await
            .unwrap();

        assert_eq!(response.swap_id, quote.quote_id.into());
        assert_eq!(
            harness
                .store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .onchain_outpoint,
            Some(packed_outpoint.clone())
        );
        let record = harness
            .chain_tx_record(quote.quote_id, LiquidityChainTxRole::Payout)
            .expect("client persists a payout chain tx record before watching");
        assert_eq!(record.role, LiquidityChainTxRole::Payout);
        assert_eq!(record.status, LiquidityChainTxStatus::Planned);
        assert_eq!(record.tx_hash, Hash256::from(packed_outpoint.tx_hash()));
        assert_eq!(record.outpoint, Some(packed_outpoint));
        assert_eq!(
            harness.events(),
            vec![
                "client_insert_created",
                "client_transition_quoted",
                "client_transition_payout_pending",
                "persist_outpoint",
                "persist_payout_tx",
                "watch_payout",
            ]
        );
    }

    #[tokio::test]
    async fn provider_payout_persists_tx_identity_before_send_tx() {
        let harness = RuntimeActorHarness::new_provider_with_realistic_ckb_watcher();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        harness.call_provider_accept(quote.quote_id).await.unwrap();

        assert_eq!(
            harness.events(),
            vec![
                "provider_insert_created",
                "provider_transition_quoted",
                "provider_transition_payout_pending",
                "persist_payout_tx",
                "provider_persist_outpoint",
                "send_tx",
                "create_tx_tracer",
                "watch_payout",
                "register_invoice",
            ]
        );
    }

    #[tokio::test]
    async fn provider_payout_send_failure_keeps_tx_identity_recoverable() {
        let harness = RuntimeActorHarness::new_provider_with_failing_send_tx();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let error = harness
            .call_provider_accept(quote.quote_id)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("chain operation failed"));
        assert!(harness
            .chain_tx_record(quote.quote_id, LiquidityChainTxRole::Payout)
            .is_some());
    }

    #[tokio::test]
    async fn provider_quote_loop_out_returns_complete_envelope() {
        let harness = RuntimeActorHarness::new_provider_with_asset();

        let quote = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: script_hex(&script("provider-quote-claimant")),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        assert_eq!(quote.asset.asset_id, "ckb");
        assert_eq!(quote.amount, 1000);
        assert!(quote.provider_fee <= 100);
        assert!(quote.routing_fee_limit <= 50);

        let persisted_quote = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(persisted_quote.quote_id, quote.quote_id.into());
        assert_eq!(
            serde_json::to_value(&quote).unwrap(),
            serde_json::to_value(liquidity_quote_envelope_from_terms(&persisted_quote)).unwrap()
        );
        assert_eq!(persisted_quote.asset.asset_id, quote.asset.asset_id);
        assert_eq!(persisted_quote.amount, quote.amount);
        assert_eq!(persisted_quote.provider_fee, quote.provider_fee);
        assert_eq!(persisted_quote.routing_fee_limit, quote.routing_fee_limit);
    }

    #[tokio::test]
    async fn provider_loop_out_quote_payment_hash_is_blake2b_of_fresh_preimage() {
        let harness = RuntimeActorHarness::new_provider_with_asset();

        let quote = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: script_hex(&script("provider-quote-preimage-claimant")),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        let preimage = persisted
            .payment_preimage
            .expect("provider quote persists its preimage");
        assert_ne!(preimage, Hash256::default());
        let expected: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
        assert_eq!(persisted.payment_hash, expected);
    }

    #[tokio::test]
    async fn two_provider_quotes_with_same_params_get_distinct_quote_ids_and_preimages() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let params = || ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: 1000,
            claimant_lock: script_hex(&script("provider-quote-uniqueness-claimant")),
            max_provider_fee: 100,
            max_routing_fee: 50,
            expires_after_seconds: 60,
        };

        let first = harness.call_provider_quote(params()).await.unwrap();
        let second = harness.call_provider_quote(params()).await.unwrap();

        assert_ne!(first.quote_id, second.quote_id);

        let first_preimage = harness
            .store
            .get_loop_out_quote(&first.quote_id.into())
            .unwrap()
            .unwrap()
            .payment_preimage
            .expect("first quote preimage");
        let second_preimage = harness
            .store
            .get_loop_out_quote(&second.quote_id.into())
            .unwrap()
            .unwrap()
            .payment_preimage
            .expect("second quote preimage");
        assert_ne!(first_preimage, second_preimage);
    }

    #[tokio::test]
    async fn provider_identity_loop_out_quote_uses_actor_pubkey() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let provider_pubkey = Pubkey::from(
            SecretKey::from_slice(&[43; 32])
                .unwrap()
                .public_key(SECP256K1),
        );
        assert_ne!(provider_pubkey, deterministic_provider_pubkey());
        let (actor, handle) = harness
            .spawn_actor_with_provider_pubkey(provider_pubkey)
            .await;

        let quote = ractor::call!(actor, |reply| {
            LiquidityActorMessage::ProviderQuoteLoopOut(
                ProviderQuoteLoopOutParams {
                    asset_id: "ckb".to_string(),
                    amount: 1_000,
                    claimant_lock: script_hex(&script("provider-identity-loop-out-claimant")),
                    max_provider_fee: 100,
                    max_routing_fee: 50,
                    expires_after_seconds: 60,
                },
                reply,
            )
        })
        .unwrap()
        .unwrap();
        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        let envelope = liquidity_quote_envelope_from_terms(&persisted);

        assert_eq!(persisted.provider, provider_pubkey);
        assert_eq!(
            fiber_types::Pubkey::try_from(envelope.provider_pubkey).unwrap(),
            provider_pubkey
        );
        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn provider_quote_loop_out_rejects_malformed_claimant_before_side_effects() {
        let harness = RuntimeActorHarness::new_provider_with_asset();

        let error = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: "not-hex".to_string(),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("claimant_lock"));
        assert!(harness.store.quotes.borrow().is_empty());
        assert_eq!(*harness.store.quote_writes.borrow(), 0);
        assert!(harness.events().is_empty());
        assert!(harness.chain.payout_locks.borrow().is_empty());
    }

    #[tokio::test]
    async fn provider_loop_out_quote_persists_final_scripts() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let claimant_lock = script("quote-final-claimant");
        let provider_funding_lock = deterministic_provider_funding_lock_script();

        let quote = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: script_hex(&claimant_lock),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(persisted.claimant_lock, claimant_lock);
        assert_eq!(persisted.refund_lock, provider_funding_lock);
    }

    #[tokio::test]
    async fn loop_out_quote_refund_lock_is_provider_funding_script_not_client_input() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let provider_funding_lock = deterministic_provider_funding_lock_script();

        let quote = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: script_hex(&script("client-claimant")),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(persisted.refund_lock, provider_funding_lock);
        assert_eq!(persisted.claimant_lock, script("client-claimant"));
    }

    #[tokio::test]
    async fn loop_in_quote_claimant_lock_is_provider_funding_script_not_client_input() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let provider_funding_lock = deterministic_provider_funding_lock_script();
        let client_refund_lock = script("client-refund");

        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice: valid_client_invoice(100, [42u8; 32].into()),
                refund_lock: script_hex(&client_refund_lock),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(persisted.claimant_lock, provider_funding_lock);
        assert_eq!(persisted.refund_lock, client_refund_lock);
    }

    #[tokio::test]
    async fn provider_loop_out_accept_does_not_mutate_quote() {
        let harness = RuntimeActorHarness::new_provider();
        let mut quote = harness.loop_out_quote_terms();
        quote.claimant_lock = script("immutable-claimant");
        quote.refund_lock = script("immutable-refund");
        harness.store_quote(quote.clone());
        let writes_before_accept = *harness.store.quote_writes.borrow();

        harness.call_provider_accept(quote.quote_id).await.unwrap();

        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted, quote);
        assert_eq!(*harness.store.quote_writes.borrow(), writes_before_accept);
    }

    #[tokio::test]
    async fn quote_loop_out_uses_local_provider_quote_path() {
        let harness = RuntimeActorHarness::new_provider_with_asset();

        let quote = harness
            .call_quote(QuoteLoopOutParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: script_hex(&script("local-quote-claimant")),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        assert_eq!(quote.asset.asset_id, "ckb");
        assert_eq!(quote.amount, 1000);
        assert!(quote.provider_fee <= 100);
        assert!(quote.routing_fee_limit <= 50);
        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&quote).unwrap(),
            serde_json::to_value(liquidity_quote_envelope_from_terms(&persisted)).unwrap()
        );
    }

    #[tokio::test]
    async fn quote_loop_in_returns_complete_envelope_with_invoice_identity_asset_and_scripts() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let client_invoice = valid_client_invoice(100, [44u8; 32].into());
        let provider_funding_lock = deterministic_provider_funding_lock_script();
        let refund_lock = script("loop-in-refund");

        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice: client_invoice.clone(),
                refund_lock: script_hex(&refund_lock),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        assert_eq!(quote.routing_fee_limit, 17);
        assert_eq!(quote.claimant_lock, script_hex(&provider_funding_lock));
        assert_eq!(quote.refund_lock, script_hex(&refund_lock));
        assert_eq!(quote.client_invoice, Some(client_invoice));
        assert_eq!(quote.asset.asset_id, "ckb");
        assert_eq!(
            fiber_types::Pubkey::try_from(quote.provider_pubkey).unwrap(),
            deterministic_provider_pubkey()
        );
        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        assert_eq!(persisted.routing_fee_limit, 17);
        assert_eq!(persisted.claimant_lock, provider_funding_lock);
        assert_eq!(persisted.refund_lock, refund_lock);
    }

    #[tokio::test]
    async fn provider_identity_loop_in_quote_uses_actor_pubkey() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let provider_pubkey = Pubkey::from(
            SecretKey::from_slice(&[43; 32])
                .unwrap()
                .public_key(SECP256K1),
        );
        assert_ne!(provider_pubkey, deterministic_provider_pubkey());
        let (actor, handle) = harness
            .spawn_actor_with_provider_pubkey(provider_pubkey)
            .await;

        let quote = ractor::call!(actor, |reply| LiquidityActorMessage::QuoteLoopIn(
            QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice: valid_client_invoice(100, [43u8; 32].into()),
                refund_lock: script_hex(&script("provider-identity-loop-in-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            },
            reply,
        ))
        .unwrap()
        .unwrap();
        let persisted = harness
            .store
            .get_loop_out_quote(&quote.quote_id.into())
            .unwrap()
            .unwrap();
        let envelope = liquidity_quote_envelope_from_terms(&persisted);

        assert_eq!(persisted.provider, provider_pubkey);
        assert_eq!(
            fiber_types::Pubkey::try_from(envelope.provider_pubkey).unwrap(),
            provider_pubkey
        );
        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn provider_quote_loop_out_rejects_when_provider_mode_disabled() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        harness.store.set_provider_mode(false).unwrap();
        assert!(!harness.store.get_provider_mode().unwrap());

        let result = harness
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 100,
                claimant_lock: script_hex(&script("disabled-claimant")),
                max_provider_fee: 10,
                max_routing_fee: 5,
                expires_after_seconds: 60,
            })
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("provider mode is disabled"));
    }

    #[tokio::test]
    async fn provider_mode_set_enabled_returns_status_enabled_true() {
        let harness = RuntimeActorHarness::new("client");
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        let status = call_set_provider_mode(&actor, true).await.unwrap();

        assert!(status.enabled);
        assert!(harness.store.get_provider_mode().unwrap());
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_mode_repeated_enable_performs_single_store_write() {
        let harness = RuntimeActorHarness::new("client");
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        let first = call_set_provider_mode(&actor, true).await.unwrap();
        let second = call_set_provider_mode(&actor, true).await.unwrap();

        assert!(first.enabled);
        assert!(second.enabled);
        assert_eq!(*harness.store.provider_mode_writes.borrow(), 1);
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_mode_disable_returns_status_disabled() {
        let harness = RuntimeActorHarness::new_provider();
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        let status = call_set_provider_mode(&actor, false).await.unwrap();

        assert!(!status.enabled);
        assert!(!harness.store.get_provider_mode().unwrap());
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_mode_disabled_gates_loop_out_accept() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        harness.store.set_provider_mode(false).unwrap();

        let error = harness
            .call_provider_accept([1u8; 32].into())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("provider mode is disabled"));
    }

    #[tokio::test]
    async fn provider_mode_disabled_gates_loop_in_quote() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        harness.store.set_provider_mode(false).unwrap();

        let error = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice: valid_client_invoice(100, [43u8; 32].into()),
                refund_lock: script_hex(&script("disabled-loop-in-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("provider mode is disabled"));
    }

    #[tokio::test]
    async fn provider_mode_disabled_gates_loop_in_accept() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        harness.store.set_provider_mode(false).unwrap();

        let error = harness
            .call_provider_accept_loop_in([1u8; 32].into(), [46u8; 32].into(), 2)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("provider mode is disabled"));
    }

    #[tokio::test]
    async fn provider_mode_disabled_does_not_gate_resume_non_terminal() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut swap = recovery_swap(31, LiquiditySwapState::PayoutPending);
        swap.onchain_outpoint = None;
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store
            .insert_loop_out_quote(
                LoopOutQuoteTerms {
                    quote_id: [31u8; 32].into(),
                    payment_hash: HashAlgorithm::CkbHash.hash([4u8; 32]).into(),
                    ..test_loop_out_quote(now_ms() + 60_000)
                },
                now_ms(),
            )
            .unwrap();
        store.set_provider_mode(false).unwrap();
        let actor = spawn_test_liquidity_actor(
            store,
            TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
            TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
        )
        .await;

        let resumed = call_resume_non_terminal(actor).await;

        assert_eq!(resumed, 1);
        assert_eq!(event_count(&events, "watch_payout"), 1);
    }

    #[tokio::test]
    async fn provider_accept_loop_in_persists_observed_lock_and_watches_it() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let client_invoice = valid_client_invoice(100, [45u8; 32].into());
        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice,
                refund_lock: script_hex(&script("loop-in-client-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();
        let quote_id: Hash256 = quote.quote_id.into();
        let lock_tx_hash: Hash256 = [46u8; 32].into();

        harness
            .call_provider_accept_loop_in(quote_id, lock_tx_hash, 2)
            .await
            .unwrap();

        let swap = harness
            .store
            .get_liquidity_swap(&quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(swap.swap_kind, LiquiditySwapKind::LoopIn);
        assert_eq!(swap.role, LiquiditySwapRole::Provider);
        assert_eq!(swap.state, LiquiditySwapState::OnchainLockPending);
        let tx = harness
            .store
            .get_liquidity_chain_tx(&quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(tx.status, LiquidityChainTxStatus::Broadcast);
        assert_eq!(tx.tx_hash, lock_tx_hash);
        assert!(tx.outpoint.is_some());
        assert_eq!(
            event_count(&harness.events, "validate_observed_loop_in_lock"),
            1
        );
        assert_eq!(event_count(&harness.events, "watch_loop_in_lock"), 1);
    }

    #[tokio::test]
    async fn provider_accept_loop_in_response_hides_client_lock_outpoint() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice: valid_client_invoice(100, [55u8; 32].into()),
                refund_lock: script_hex(&script("loop-in-hidden-outpoint-client-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();
        let quote_id: Hash256 = quote.quote_id.into();
        let lock_tx_hash: Hash256 = [56u8; 32].into();

        let response = harness
            .call_provider_accept_loop_in(quote_id, lock_tx_hash, 2)
            .await
            .unwrap();

        let swap = harness
            .store
            .get_liquidity_swap(&quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(swap.swap_kind, LiquiditySwapKind::LoopIn);
        assert!(
            swap.onchain_outpoint.is_some(),
            "provider loop in persists the observed client lock outpoint"
        );
        assert_eq!(
            response.payout_outpoint, None,
            "loop in responses must not leak the client lock outpoint as a payout outpoint"
        );
    }

    #[tokio::test]
    async fn provider_accept_loop_in_is_idempotent() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice: valid_client_invoice(100, [52u8; 32].into()),
                refund_lock: script_hex(&script("loop-in-idempotent-client-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();
        let quote_id: Hash256 = quote.quote_id.into();
        let lock_tx_hash: Hash256 = [53u8; 32].into();
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        let first_response = call_provider_accept_loop_in(&actor, quote_id, lock_tx_hash, 2)
            .await
            .unwrap();
        harness
            .store
            .update_liquidity_swap_state(
                &quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::OnchainLocked,
                    updated_at: now_ms(),
                    reason: None,
                },
            )
            .unwrap();
        let original_swap = harness
            .store
            .get_liquidity_swap(&quote_id)
            .unwrap()
            .unwrap();
        let original_tx = harness
            .chain_tx_record(quote_id, LiquidityChainTxRole::Payout)
            .unwrap();
        let events_after_first_accept = harness.events();
        let second_response = call_provider_accept_loop_in(&actor, quote_id, lock_tx_hash, 2)
            .await
            .unwrap();

        assert_eq!(second_response.swap_id, first_response.swap_id);
        assert_eq!(second_response.state, "OnchainLocked");
        assert_eq!(second_response.payment_hash, first_response.payment_hash);
        assert_eq!(second_response.created_at, first_response.created_at);
        assert_eq!(harness.events(), events_after_first_accept);
        assert_eq!(event_count(&harness.events, "provider_insert_created"), 1);
        assert_eq!(event_count(&harness.events, "provider_persist_outpoint"), 1);
        assert_eq!(event_count(&harness.events, "persist_payout_tx"), 1);
        assert_eq!(
            event_count(&harness.events, "validate_observed_loop_in_lock"),
            1
        );
        assert_eq!(event_count(&harness.events, "watch_loop_in_lock"), 1);
        assert_eq!(event_count(&harness.events, "send_payment"), 0);
        assert_eq!(*harness.store.quote_writes.borrow(), 1);

        let error = call_provider_accept_loop_in(&actor, quote_id, [54u8; 32].into(), 3)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not match accept params"));
        assert_eq!(
            harness.store.get_liquidity_swap(&quote_id).unwrap(),
            Some(original_swap)
        );
        assert_eq!(
            harness.chain_tx_record(quote_id, LiquidityChainTxRole::Payout),
            Some(original_tx)
        );
        assert_eq!(harness.events(), events_after_first_accept);
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_accept_loop_in_rejects_invalid_observed_lock_before_persistence() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        store
            .upsert_liquidity_asset(test_loop_out_quote(now_ms() + 60_000).asset)
            .unwrap();
        store.set_provider_mode(true).unwrap();
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_provider")
            .with_store(store.clone());
        chain.reject_observed_loop_in_lock();
        let harness = RuntimeActorHarness {
            events: events.clone(),
            store: store.clone(),
            chain,
            payment: TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
        };
        let client_invoice = valid_client_invoice(100, [47u8; 32].into());
        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice,
                refund_lock: script_hex(&script("loop-in-client-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();
        let quote_id: Hash256 = quote.quote_id.into();

        let error = harness
            .call_provider_accept_loop_in(quote_id, [48u8; 32].into(), 2)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("observed loop in lock"));
        assert!(store.get_liquidity_swap(&quote_id).unwrap().is_none());
        assert!(store
            .get_liquidity_chain_tx(&quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn provider_accept_loop_in_rejects_mismatched_duplicate_before_mutating_outpoint() {
        let harness = RuntimeActorHarness::new_provider_with_asset();
        let client_invoice = valid_client_invoice(100, [49u8; 32].into());
        let quote = harness
            .call_quote_loop_in(QuoteLoopInParams {
                provider: "local".to_string(),
                asset_id: "ckb".to_string(),
                amount: 100,
                client_invoice,
                refund_lock: script_hex(&script("loop-in-client-refund")),
                max_provider_fee: 100,
                max_routing_fee: 17,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();
        let quote_id: Hash256 = quote.quote_id.into();
        let first_hash: Hash256 = [50u8; 32].into();
        harness
            .call_provider_accept_loop_in(quote_id, first_hash, 2)
            .await
            .unwrap();
        let original_outpoint = harness
            .store
            .get_liquidity_swap(&quote_id)
            .unwrap()
            .unwrap()
            .onchain_outpoint;

        let error = harness
            .call_provider_accept_loop_in(quote_id, [51u8; 32].into(), 3)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("does not match accept params"));
        let swap = harness
            .store
            .get_liquidity_swap(&quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(swap.onchain_outpoint, original_outpoint);
        let tx = harness
            .store
            .get_liquidity_chain_tx(&quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(tx.tx_hash, first_hash);
    }

    #[tokio::test]
    async fn manual_loop_out_runtime_harness_reaches_success_only_after_claim_confirmation() {
        let client = RuntimeActorHarness::new_client();
        let provider = RuntimeActorHarness::new_provider_with_asset();

        let mut quote = provider
            .call_provider_quote(ProviderQuoteLoopOutParams {
                asset_id: "ckb".to_string(),
                amount: 1000,
                claimant_lock: script_hex(&script("manual-claimant")),
                max_provider_fee: 100,
                max_routing_fee: 50,
                expires_after_seconds: 60,
            })
            .await
            .unwrap();

        let quote_id = quote.quote_id.into();
        provider.use_fake_payment_preimage_for_quote(&mut quote);
        let provider_response = provider.call_provider_accept(quote_id).await.unwrap();
        client.import_provider_quote(&provider, quote_id);
        client
            .call_loop_out_with_outpoint(
                quote_id,
                quote.provider_fee,
                quote.routing_fee_limit,
                provider_response.payout_outpoint.unwrap(),
            )
            .await
            .unwrap();

        provider.confirm_payout(quote_id).await.unwrap().unwrap();
        provider.call_payment_settled(quote_id).await;
        client.confirm_payout(quote_id).await.unwrap().unwrap();

        assert_ne!(client.swap_state(quote_id), LiquiditySwapState::Success);
        assert_ne!(provider.swap_state(quote_id), LiquiditySwapState::Success);

        client.call_claim_confirmed(quote_id).await;
        assert_eq!(client.swap_state(quote_id), LiquiditySwapState::Success);
        assert_ne!(provider.swap_state(quote_id), LiquiditySwapState::Success);

        provider.call_provider_claim_observed(quote_id).await;

        assert_eq!(client.swap_state(quote_id), LiquiditySwapState::Success);
        assert_eq!(provider.swap_state(quote_id), LiquiditySwapState::Success);
    }

    #[tokio::test]
    async fn provider_observes_incoming_payment_settlement_after_payout() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());
        harness.call_provider_accept(quote.quote_id).await.unwrap();

        harness
            .confirm_payout(quote.quote_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            harness.swap_state(quote.quote_id),
            LiquiditySwapState::PaymentInFlight
        );
        wait_for_event(&harness.events, "reload_provider_payment").await;

        let actor = harness.spawn_actor().await;
        actor
            .send_message(LiquidityActorMessage::PaymentSettled(
                quote.quote_id,
                [4u8; 32].into(),
            ))
            .unwrap();
        wait_for_event(&harness.events, "watch_provider_claim").await;
        ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
            .unwrap()
            .unwrap();

        assert_eq!(
            harness.swap_state(quote.quote_id),
            LiquiditySwapState::PaymentSettled
        );
        assert_eq!(event_count(&harness.events, "watch_provider_claim"), 1);
        assert_eq!(event_count(&harness.events, "watch_claim"), 0);
    }

    #[tokio::test]
    async fn provider_payment_observation_survives_initial_in_flight_polls() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());
        harness.payment.reload_statuses.borrow_mut().extend([
            LoopOutPaymentStatus::InFlight,
            LoopOutPaymentStatus::InFlight,
            LoopOutPaymentStatus::Settled(quote.payment_hash),
        ]);
        harness.call_provider_accept(quote.quote_id).await.unwrap();

        harness
            .confirm_payout(quote.quote_id)
            .await
            .unwrap()
            .unwrap();
        wait_for_event_count(&harness.events, "reload_provider_payment", 3).await;

        assert_eq!(
            harness.swap_state(quote.quote_id),
            LiquiditySwapState::PaymentSettled
        );
    }

    #[tokio::test]
    async fn provider_accept_loop_out_uses_quoted_lock_scripts() {
        let harness = RuntimeActorHarness::new_provider();
        let mut quote = harness.loop_out_quote_terms();
        let claimant_lock = script("claimant-submitted");
        let refund_lock = script("refund-submitted");
        quote.claimant_lock = claimant_lock.clone();
        quote.refund_lock = refund_lock.clone();
        harness.store_quote(quote.clone());

        harness.call_provider_accept(quote.quote_id).await.unwrap();

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
    async fn provider_accept_loop_out_registers_invoice_with_preimage() {
        let harness = RuntimeActorHarness::new_provider();
        let preimage: Hash256 = [7u8; 32].into();
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
        let mut quote = harness.loop_out_quote_terms();
        quote.amount = 1_000;
        quote.provider_fee = 1;
        quote.routing_fee_limit = 100;
        quote.payment_hash = payment_hash;
        quote.payment_preimage = Some(preimage);
        harness.store_quote(quote.clone());
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        call_provider_accept_loop_out(&actor, quote.quote_id)
            .await
            .unwrap();

        assert_eq!(harness.payment.registered_invoices(), vec![payment_hash]);
        assert_eq!(harness.payment.registered_invoice_amounts(), vec![1_001]);
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_accept_loop_out_reregisters_invoice_on_idempotent_accept() {
        let harness = RuntimeActorHarness::new_provider();
        let preimage: Hash256 = [7u8; 32].into();
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
        let mut quote = harness.loop_out_quote_terms();
        quote.payment_hash = payment_hash;
        quote.payment_preimage = Some(preimage);
        harness.store_quote(quote.clone());
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        harness.payment.fail_next_registration();
        let error = call_provider_accept_loop_out(&actor, quote.quote_id)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("registration failed"));
        assert_eq!(harness.payment.registered_invoices(), Vec::<Hash256>::new());

        let response = call_provider_accept_loop_out(&actor, quote.quote_id)
            .await
            .unwrap();
        assert_eq!(response.swap_id, quote.quote_id.into());
        assert_eq!(harness.payment.registered_invoices(), vec![payment_hash]);
        assert_eq!(event_count(&harness.events, "reserve_payout"), 1);
        assert_eq!(event_count(&harness.events, "broadcast_payout"), 1);
        assert_eq!(event_count(&harness.events, "watch_payout"), 1);
        assert_eq!(harness.chain.payout_locks.borrow().len(), 1);
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_accept_loop_out_is_idempotent() {
        let harness = RuntimeActorHarness::new_provider();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());
        let (actor, handle) = harness.spawn_actor_with_handle().await;

        let first_response = call_provider_accept_loop_out(&actor, quote.quote_id)
            .await
            .unwrap();
        let second_response = call_provider_accept_loop_out(&actor, quote.quote_id)
            .await
            .unwrap();

        assert_eq!(second_response.swap_id, first_response.swap_id);
        assert_eq!(second_response.state, first_response.state);
        assert_eq!(second_response.payment_hash, first_response.payment_hash);
        assert_eq!(second_response.created_at, first_response.created_at);
        assert_eq!(event_count(&harness.events, "provider_insert_created"), 1);
        assert_eq!(event_count(&harness.events, "reserve_payout"), 1);
        assert_eq!(event_count(&harness.events, "broadcast_payout"), 1);
        assert_eq!(event_count(&harness.events, "watch_payout"), 1);
        assert_eq!(event_count(&harness.events, "send_payment"), 0);
        assert_eq!(event_count(&harness.events, "register_invoice"), 2);
        assert_eq!(harness.chain.payout_locks.borrow().len(), 1);
        assert_eq!(*harness.store.quote_writes.borrow(), 1);
        stop_liquidity_actor(actor, handle).await;
    }

    #[tokio::test]
    async fn provider_accept_loop_out_rejects_missing_quote_before_side_effects() {
        let harness = RuntimeActorHarness::new_provider();
        let error = harness
            .call_provider_accept([9u8; 32].into())
            .await
            .unwrap_err();

        let error_text = error.to_string();
        assert!(error_text.contains("quote"));
        assert!(error_text.contains("not found") || error_text.contains("missing"));
        assert!(harness.events().is_empty());
        assert!(harness.chain.payout_locks.borrow().is_empty());
    }

    #[tokio::test]
    async fn provider_accept_loop_out_rejects_conflicting_swap_without_changing_locks() {
        let harness = RuntimeActorHarness::new_provider();
        let mut quote = harness.loop_out_quote_terms();
        let first_claimant_lock = script("claimant-first");
        let first_refund_lock = script("refund-first");
        quote.claimant_lock = first_claimant_lock.clone();
        quote.refund_lock = first_refund_lock.clone();
        harness.store_quote(quote.clone());

        harness.call_provider_accept(quote.quote_id).await.unwrap();
        let events_after_first_accept = harness.events();
        harness
            .store
            .swaps
            .borrow_mut()
            .get_mut(&quote.quote_id)
            .unwrap()
            .role = LiquiditySwapRole::Client;

        let error = harness
            .call_provider_accept(quote.quote_id)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("conflicts"));
        assert!(error.to_string().contains("expected provider loop out"));
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

        let error = harness
            .call_loop_out([9u8; 32].into(), u128::MAX, u128::MAX)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("quote"));
        assert!(harness.events().is_empty());
    }

    #[tokio::test]
    async fn loop_out_rejects_provider_fee_above_execution_cap_before_side_effects() {
        let harness = RuntimeActorHarness::new_client();
        let mut quote = harness.loop_out_quote_terms();
        quote.provider_fee = 2;
        harness.store_quote(quote.clone());

        let error = harness
            .call_loop_out(quote.quote_id, 1, quote.routing_fee_limit)
            .await
            .unwrap_err();

        assert_eq!(error, LiquidityLoopOutError::ProviderFeeTooHigh);
        assert!(harness.events().is_empty());
        assert!(harness.store.swaps.borrow().is_empty());
        assert!(harness.store.chain_txs.borrow().is_empty());
        assert!(harness.store.signed_txs.borrow().is_empty());
        assert!(harness.chain.payout_locks.borrow().is_empty());
        assert!(harness.payment.requests().is_empty());
        assert!(harness.payment.registered_invoices().is_empty());
    }

    #[tokio::test]
    async fn loop_out_rejects_routing_fee_above_execution_cap_before_side_effects() {
        let harness = RuntimeActorHarness::new_client();
        let mut quote = harness.loop_out_quote_terms();
        quote.routing_fee_limit = 2;
        harness.store_quote(quote.clone());

        let error = harness
            .call_loop_out(quote.quote_id, quote.provider_fee, 1)
            .await
            .unwrap_err();

        assert_eq!(error, LiquidityLoopOutError::RoutingFeeTooHigh);
        assert!(harness.events().is_empty());
        assert!(harness.store.swaps.borrow().is_empty());
        assert!(harness.store.chain_txs.borrow().is_empty());
        assert!(harness.store.signed_txs.borrow().is_empty());
        assert!(harness.chain.payout_locks.borrow().is_empty());
        assert!(harness.payment.requests().is_empty());
        assert!(harness.payment.registered_invoices().is_empty());
    }

    #[tokio::test]
    async fn loop_out_accepts_execution_fee_caps_at_exact_quote_boundaries() {
        let harness = RuntimeActorHarness::new_client();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());

        let response = harness
            .call_loop_out(quote.quote_id, quote.provider_fee, quote.routing_fee_limit)
            .await
            .unwrap();

        assert_eq!(response.swap_id, quote.quote_id.into());
        assert_eq!(response.state, "PayoutPending");
    }

    #[tokio::test]
    async fn liquidity_actor_duplicate_continuation_does_not_stop_actor() {
        let harness = RuntimeActorHarness::new_client();
        let quote = harness.loop_out_quote_terms();
        harness.store_quote(quote.clone());
        harness
            .call_loop_out_with_outpoint(
                quote.quote_id,
                quote.provider_fee,
                quote.routing_fee_limit,
                test_json_payout_outpoint(),
            )
            .await
            .unwrap();

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
                payout_outpoint: Some(test_json_payout_outpoint()),
            },
            reply
        ))
        .unwrap()
        .unwrap();
        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;

        let events_before_settlement = events.borrow().clone();
        let validation_index = events_before_settlement
            .iter()
            .position(|event| *event == "validate_observed_loop_out_payout")
            .unwrap();
        let payout_locked_index = events_before_settlement
            .iter()
            .position(|event| *event == "client_transition_payout_locked")
            .unwrap();
        let payment_index = events_before_settlement
            .iter()
            .position(|event| *event == "send_payment")
            .unwrap();
        assert!(validation_index < payout_locked_index);
        assert!(payout_locked_index < payment_index);

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
    async fn liquidity_actor_stop_cancels_blocked_payment_job_before_termination() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let (payment, release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(events.clone(), vec![]);
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let (actor, handle) = ractor::Actor::spawn(
            None,
            LiquidityActor::<_, _, _>(std::marker::PhantomData),
            LiquidityActorArguments {
                store,
                payment,
                chain,
                provider_pubkey: deterministic_provider_pubkey(),
                provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            },
        )
        .await
        .unwrap();

        ractor::call!(actor.clone(), |reply| LiquidityActorMessage::LoopOut(
            LoopOutParams {
                quote_id: quote.quote_id.into(),
                max_provider_fee: 1,
                max_routing_fee: 1,
                payout_outpoint: Some(test_json_payout_outpoint()),
            },
            reply
        ))
        .unwrap()
        .unwrap();
        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;

        actor.stop(Some("test actor-owned job cancellation".to_string()));
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("liquidity actor stop timed out")
            .expect("liquidity actor task panicked");

        assert!(
            release_payment
                .send(Err("late completion".to_string()))
                .is_err(),
            "blocked payment job retained its receiver after actor termination"
        );
        assert_eq!(event_count(&events, "reload_payment"), 0);
    }

    #[tokio::test]
    async fn liquidity_actor_kill_cancels_blocked_payment_job_without_post_stop() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let (payment, release_payment) =
            TestLoopOutPayment::with_pending_result_and_reload_statuses(events.clone(), vec![]);
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let (actor, handle) = ractor::Actor::spawn(
            None,
            LiquidityActor::<_, _, _>(std::marker::PhantomData),
            LiquidityActorArguments {
                store,
                payment,
                chain,
                provider_pubkey: deterministic_provider_pubkey(),
                provider_funding_lock_script: deterministic_provider_funding_lock_script(),
            },
        )
        .await
        .unwrap();

        ractor::call!(actor.clone(), |reply| LiquidityActorMessage::LoopOut(
            LoopOutParams {
                quote_id: quote.quote_id.into(),
                max_provider_fee: 1,
                max_routing_fee: 1,
                payout_outpoint: Some(test_json_payout_outpoint()),
            },
            reply
        ))
        .unwrap()
        .unwrap();
        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "send_payment").await;

        actor.kill();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("killed liquidity actor timed out")
            .expect("liquidity actor task panicked");

        let _ = release_payment.send(Err("late completion".to_string()));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(event_count(&events, "reload_payment"), 0);
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
                payout_outpoint: Some(test_json_payout_outpoint()),
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
                payout_outpoint: Some(test_json_payout_outpoint()),
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
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
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

    #[tokio::test]
    async fn liquidity_actor_payment_settled_rejects_default_preimage_before_persistence() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let payment = TestLoopOutPayment::new_with_label(events.clone(), "runtime");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
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
                Hash256::default(),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PaymentInFlight);
        assert_eq!(swap.payment_preimage, None);
        assert!(events.borrow().is_empty());
    }

    #[tokio::test]
    async fn liquidity_actor_payment_settled_rejects_mismatched_preimage_before_persistence() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_client");
        let payment = TestLoopOutPayment::new_with_label(events.clone(), "runtime");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
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
                [9u8; 32].into(),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PaymentInFlight);
        assert_eq!(swap.payment_preimage, None);
        assert!(events.borrow().is_empty());
    }

    #[tokio::test]
    async fn liquidity_actor_provider_payment_settled_rejects_default_preimage_before_transition() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_provider");
        let payment = TestLoopOutPayment::new_with_label(events.clone(), "runtime");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(store.events()),
            chain.clone(),
        )
        .await;
        accept_provider_loop_out(&store, &mut chain.clone(), quote.clone(), now_ms(), actor)
            .await
            .unwrap();
        mark_provider_payout_locked(&store, quote.quote_id, now_ms()).unwrap();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PaymentSettled(
                quote.quote_id,
                Hash256::default(),
            ))
            .unwrap();
        actor
            .send_message(LiquidityActorMessage::ProviderClaimObserved(quote.quote_id))
            .unwrap();
        ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
            .unwrap()
            .unwrap();

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PayoutLocked);
        assert!(!events
            .borrow()
            .contains(&"provider_transition_payment_settled"));
        assert!(!events.borrow().contains(&"provider_transition_success"));
    }

    #[tokio::test]
    async fn liquidity_actor_provider_payment_settled_rejects_mismatched_preimage_before_transition(
    ) {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "provider");
        let chain = TestLiquidityChain::new_with_label(events.clone(), "runtime_provider");
        let payment = TestLoopOutPayment::new_with_label(events.clone(), "runtime");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(store.events()),
            chain.clone(),
        )
        .await;
        accept_provider_loop_out(&store, &mut chain.clone(), quote.clone(), now_ms(), actor)
            .await
            .unwrap();
        mark_provider_payout_locked(&store, quote.quote_id, now_ms()).unwrap();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PaymentSettled(
                quote.quote_id,
                [9u8; 32].into(),
            ))
            .unwrap();
        actor
            .send_message(LiquidityActorMessage::ProviderClaimObserved(quote.quote_id))
            .unwrap();
        ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
            .unwrap()
            .unwrap();

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PayoutLocked);
        assert!(!events
            .borrow()
            .contains(&"provider_transition_payment_settled"));
        assert!(!events.borrow().contains(&"provider_transition_success"));
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
            Some(RecoveryAction::BroadcastClaim)
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
    fn persist_client_preimage_rejects_invalid_state_without_writing_preimage() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms, None).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms + 1).unwrap();
        events.borrow_mut().clear();

        assert_eq!(
            persist_client_loop_out_payment_preimage(
                &store,
                quote.quote_id,
                [4u8; 32].into(),
                now_ms + 2,
            ),
            Err(LiquidityLoopOutError::InvalidStateTransition {
                from: LiquiditySwapState::PayoutLocked,
                to: LiquiditySwapState::PaymentSettled,
            })
        );

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PayoutLocked);
        assert_eq!(swap.payment_preimage, None);
        assert!(events.borrow().is_empty());
    }

    #[tokio::test]
    async fn authorized_client_payment_rejects_mismatched_preimage_before_persistence() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut payment = TestLoopOutPayment::new(events.clone());
        payment.preimage = [9u8; 32].into();
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        store.insert_loop_out_quote(quote.clone(), now_ms).unwrap();
        create_client_loop_out(
            &store,
            quote.clone(),
            now_ms,
            Some(OutPoint::new(Byte32::from_slice(&[31u8; 32]).unwrap(), 0)),
        )
        .unwrap();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            payment,
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "payment_send").await;
        call_resume_non_terminal_result(actor).await.unwrap();

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PaymentInFlight);
        assert_eq!(swap.payment_preimage, None);
        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 1);
        assert_eq!(event_count(&events, "payment_send"), 1);
    }

    #[tokio::test]
    async fn provider_accept_persists_payout_pending_before_chain_broadcast() {
        let store = TestLiquidityStore::default();
        let mut chain = TestLiquidityChain::new(store.events());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(store.events()),
            chain.clone(),
        )
        .await;
        accept_provider_loop_out(&store, &mut chain, quote, now_ms, actor)
            .await
            .unwrap();

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

    #[tokio::test]
    async fn provider_accept_loop_out_rejects_expired_quote_before_order_creation() {
        let store = TestLiquidityStore::default();
        let mut chain = TestLiquidityChain::new(store.events());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms);

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(store.events()),
            chain.clone(),
        )
        .await;
        assert_eq!(
            accept_provider_loop_out(&store, &mut chain, quote, now_ms, actor).await,
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

    #[tokio::test]
    async fn loop_out_client_claim_broadcast_waits_for_confirmation_before_success() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms, None).unwrap();
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

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;
        claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4, actor)
            .await
            .unwrap();

        let swap_after_claim_broadcast =
            store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(
            swap_after_claim_broadcast.state,
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

    #[tokio::test]
    async fn loop_out_provider_claim_observed_requires_payment_settled_state() {
        let store = TestLiquidityStore::default();
        let mut chain = TestLiquidityChain::new(store.events());
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(store.events()),
            chain.clone(),
        )
        .await;
        accept_provider_loop_out(&store, &mut chain, quote.clone(), now_ms, actor)
            .await
            .unwrap();

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

    #[test]
    fn provider_loop_out_duplicate_claim_observed_at_success_is_harmless() {
        let store = TestLiquidityStore::default();
        let mut swap = recovery_swap(42, LiquiditySwapState::Success);
        swap.role = LiquiditySwapRole::Provider;
        store.insert_liquidity_swap(swap.clone()).unwrap();

        mark_provider_claim_observed(&store, swap.swap_id, now_ms()).unwrap();

        assert_eq!(
            store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap(),
            swap
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

    #[tokio::test]
    async fn loop_out_client_claim_requires_persisted_preimage_before_broadcast() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms, None).unwrap();
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

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;
        assert_eq!(
            claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4, actor).await,
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

    #[tokio::test]
    async fn loop_out_client_claim_rejects_default_preimage_before_broadcast() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms, None).unwrap();
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
                    payment_preimage: Some(Hash256::default()),
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

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;
        let error = claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4, actor)
            .await
            .expect_err("default preimage must not be claimable");

        assert!(error.to_string().contains("preimage"));
        assert!(events.borrow().is_empty());
        assert_eq!(chain.claim_preimages, Vec::<Hash256>::new());
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentSettled
        );
    }

    #[tokio::test]
    async fn loop_out_client_claim_rejects_mismatched_preimage_before_broadcast() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms, None).unwrap();
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
                    payment_preimage: Some([9u8; 32].into()),
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

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;
        let error = claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4, actor)
            .await
            .expect_err("mismatched preimage must not be claimable");

        assert!(error.to_string().contains("payment hash"));
        assert!(events.borrow().is_empty());
        assert_eq!(chain.claim_preimages, Vec::<Hash256>::new());
        assert_eq!(
            store
                .get_liquidity_swap(&quote.quote_id)
                .unwrap()
                .unwrap()
                .state,
            LiquiditySwapState::PaymentSettled
        );
    }

    #[tokio::test]
    async fn loop_out_client_claim_failure_persists_pending_for_retry() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let mut chain = TestLiquidityChain::new_with_label(events.clone(), "chain");
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        create_client_loop_out(&store, quote.clone(), now_ms, None).unwrap();
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

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;
        assert_eq!(
            claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 4, actor).await,
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

        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            chain.clone(),
        )
        .await;
        assert_eq!(
            claim_client_loop_out(&store, &mut chain, quote.quote_id, now_ms + 5, actor).await,
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
        let payment = TestLoopOutPayment::new(events.clone());
        let now_ms = 1_000;
        let quote = LoopOutQuoteTerms {
            amount: u128::MAX,
            provider_fee: 1,
            ..test_loop_out_quote(now_ms + 60_000)
        };

        store.insert_loop_out_quote(quote.clone(), now_ms).unwrap();
        create_client_loop_out(
            &store,
            quote.clone(),
            now_ms,
            Some(OutPoint::new(Byte32::from_slice(&[32u8; 32]).unwrap(), 0)),
        )
        .unwrap();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            payment,
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "validate_observed_loop_out_payout").await;
        assert_eq!(call_resume_non_terminal(actor).await, 0);

        assert_eq!(event_count(&events, "payment_send"), 0);
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
    async fn claim_confirmed_marks_claim_tx_record_confirmed() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms() + 1).unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::PaymentInFlight,
                    updated_at: now_ms() + 2,
                    reason: None,
                },
            )
            .unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::PaymentSettled,
                    updated_at: now_ms() + 3,
                    reason: None,
                },
            )
            .unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::ClaimPending,
                    updated_at: now_ms() + 4,
                    reason: None,
                },
            )
            .unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Claim,
                tx_hash: [8u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms() + 4,
                updated_at: now_ms() + 4,
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::ClaimConfirmed(quote.quote_id))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
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
    async fn payout_confirmed_marks_payout_tx_record_confirmed() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [13u8; 32].into(),
                outpoint: Some(OutPoint::new(Byte32::from_slice(&[13u8; 32]).unwrap(), 13)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn payout_confirmed_is_idempotent_after_client_success() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        let outpoint = OutPoint::new(Byte32::from_slice(&[13u8; 32]).unwrap(), 13);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint)).unwrap();
        store
            .swaps
            .borrow_mut()
            .get_mut(&quote.quote_id)
            .unwrap()
            .state = LiquiditySwapState::Success;
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.reject_observed_loop_out_payout_with("late invalid payout");
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.state, LiquiditySwapState::Success);
        assert_eq!(swap.failure_reason, None);
        assert!(payment_requests.borrow().is_empty());
        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 0);
    }

    #[tokio::test]
    async fn every_client_payout_validation_error_stays_pending_without_payment() {
        let validation_errors = [
            "observed loop out payout cell not found or already spent",
            "liquidity-lock contract mismatch",
            "lock args length mismatch",
            "payment_hash mismatch",
            "claimant_lock_hash mismatch",
            "refund_lock_hash mismatch",
            "refund_after_lock_time mismatch",
            "amount mismatch",
            "asset_type_hash mismatch",
            "capacity below amount",
            "capacity below requirement",
            "unexpected type script",
            "UDT type script mismatch",
            "UDT data length mismatch",
            "UDT amount mismatch",
            "UDT capacity below requirement",
        ];

        // The chain matrix exercises the concrete cells producing these errors. This boundary
        // table proves every adapter error takes the same persistence and no-payment path.
        for validation_error in validation_errors {
            let events = Shared::new(Vec::new());
            let store = TestLiquidityStore::new(events.clone(), "client");
            let quote = test_loop_out_quote(now_ms() + 60_000);
            let outpoint = OutPoint::new(Byte32::from_slice(&[13u8; 32]).unwrap(), 13);
            store
                .insert_loop_out_quote(quote.clone(), now_ms())
                .unwrap();
            create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint.clone()))
                .unwrap();
            store
                .update_liquidity_chain_tx_status(
                    &quote.quote_id,
                    LiquidityChainTxRole::Payout,
                    LiquidityChainTxStatus::Broadcast,
                    None,
                    now_ms(),
                )
                .unwrap();
            let mut chain = TestLiquidityChain::new(events.clone());
            chain.reject_observed_loop_out_payout_with(validation_error);
            let actor = spawn_test_liquidity_actor(
                store.clone(),
                TestLoopOutPayment::new(events.clone()),
                chain,
            )
            .await;

            actor
                .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
                .unwrap();
            wait_for_event(&events, "validate_observed_loop_out_payout").await;
            ractor::call!(actor, LiquidityActorMessage::ResumeNonTerminal)
                .unwrap()
                .unwrap();

            let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
            let payout_tx = store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
                .unwrap()
                .unwrap();
            assert_eq!(swap.state, LiquiditySwapState::PayoutPending);
            assert!(swap.failure_reason.unwrap().contains(validation_error));
            assert_eq!(payout_tx.status, LiquidityChainTxStatus::Confirmed);
            assert!(!events.borrow().contains(&"client_transition_payout_locked"));
            assert!(!events.borrow().contains(&"send_payment"));
        }
    }

    #[tokio::test]
    async fn transient_payout_validation_retries_then_pays_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        let outpoint = OutPoint::new(Byte32::from_slice(&[15u8; 32]).unwrap(), 0);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint)).unwrap();
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Transient(
                "temporary ckb rpc failure".to_string(),
            )),
            Ok(()),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 2).await;
        wait_for_event(&events, "payment_send").await;
        call_resume_non_terminal_result(actor.clone())
            .await
            .unwrap();

        let payout_tx = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(payout_tx.status, LiquidityChainTxStatus::Confirmed);
        assert_eq!(payout_tx.failure_reason, None);
        assert_eq!(payment_requests.borrow().len(), 1);

        actor
            .send_message(LiquidityActorMessage::RetryPayoutValidation(
                quote.quote_id,
                ClientPayoutAuthorization::PayoutConfirmation,
                PAYOUT_VALIDATION_MAX_ATTEMPTS,
                1,
            ))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();
        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 2);
        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn quote_lookup_error_after_retry_timer_recovers_without_restart() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        let outpoint = OutPoint::new(Byte32::from_slice(&[21u8; 32]).unwrap(), 0);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint)).unwrap();
        store.set_quote_lookup_results(vec![
            Ok(()),
            Err("temporary quote lookup failure".to_string()),
            Ok(()),
        ]);
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Transient(
                "temporary ckb rpc failure".to_string(),
            )),
            Ok(()),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let actor = spawn_test_liquidity_actor(store, payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 2).await;
        wait_for_event(&events, "payment_send").await;
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn recovered_client_payout_locked_definitive_validation_failure_does_not_pay() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 61);
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.reject_observed_loop_out_payout_with(
            "observed loop out payout cell not found or already spent",
        );
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 0);
        wait_for_event(&events, "validate_observed_loop_out_payout").await;
        call_resume_non_terminal_result(actor).await.unwrap();

        let recovered = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
        let payout = store
            .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, LiquiditySwapState::PayoutLocked);
        assert!(recovered
            .failure_reason
            .unwrap()
            .contains("not found or already spent"));
        assert_eq!(payout.status, LiquidityChainTxStatus::Confirmed);
        assert!(payout
            .failure_reason
            .unwrap()
            .contains("not found or already spent"));
        assert!(payment_requests.borrow().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn definitive_validation_retries_failed_swap_context_write_before_stopping() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 67);
        store.set_swap_failure_write_results(vec![
            Err("temporary swap write failure".to_string()),
            Ok(()),
        ]);
        let validation_error = "payout amount mismatch";
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Definitive(
                validation_error.to_string(),
            )),
            Err(PayoutValidationError::Definitive(
                validation_error.to_string(),
            )),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 1);
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 2).await;
        assert_eq!(call_resume_non_terminal(actor).await, 0);
        tokio::time::sleep(PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL * 2).await;

        let persisted_swap = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
        let payout = store
            .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert!(persisted_swap
            .failure_reason
            .as_deref()
            .unwrap()
            .contains(validation_error));
        assert!(payout
            .failure_reason
            .as_deref()
            .unwrap()
            .contains(validation_error));
        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 2);
        assert!(payment_requests.borrow().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn definitive_validation_retries_failed_chain_context_write_before_stopping() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 68);
        store.set_chain_failure_write_results(vec![
            Err("temporary chain write failure".to_string()),
            Ok(()),
        ]);
        let validation_error = "payout lock args mismatch";
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Definitive(
                validation_error.to_string(),
            )),
            Err(PayoutValidationError::Definitive(
                validation_error.to_string(),
            )),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 1);
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 2).await;
        assert_eq!(call_resume_non_terminal(actor).await, 0);
        tokio::time::sleep(PAYOUT_VALIDATION_PERIODIC_RETRY_INTERVAL * 2).await;

        let persisted_swap = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
        let payout = store
            .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert!(persisted_swap
            .failure_reason
            .as_deref()
            .unwrap()
            .contains(validation_error));
        assert!(payout
            .failure_reason
            .as_deref()
            .unwrap()
            .contains(validation_error));
        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 2);
        assert!(payment_requests.borrow().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn valid_retry_cleans_chain_first_partial_definitive_persistence_before_payment() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 69);
        let unrelated_reason = "historical payment recovery warning".to_string();
        store
            .update_liquidity_swap(
                &swap.swap_id,
                LiquiditySwapUpdate {
                    failure_reason: Some(unrelated_reason.clone()),
                    updated_at: now_ms(),
                    ..Default::default()
                },
            )
            .unwrap();
        store.set_definitive_failure_write_results(vec![
            Ok(()),
            Err("temporary second persistence write failure".to_string()),
        ]);
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Definitive(
                "payout amount mismatch".to_string(),
            )),
            Ok(()),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        assert_eq!(call_resume_non_terminal(actor).await, 1);
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 2).await;
        wait_for_event(&events, "payment_send").await;

        let persisted_swap = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
        let payout = store
            .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(persisted_swap.failure_reason, Some(unrelated_reason));
        assert_eq!(payout.failure_reason, None);
        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn restart_revalidates_definitive_failure_and_clears_matching_context_on_success() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 65);
        let mut invalid_chain = TestLiquidityChain::new(events.clone());
        invalid_chain.reject_observed_loop_out_payout_with("payout amount mismatch");
        let first_actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            invalid_chain,
        )
        .await;

        call_resume_non_terminal(first_actor.clone()).await;
        wait_for_event(&events, "validate_observed_loop_out_payout").await;
        first_actor.stop(Some("simulate restart".to_string()));
        events.borrow_mut().clear();

        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let restarted_actor = spawn_test_liquidity_actor(
            store.clone(),
            payment,
            TestLiquidityChain::new(events.clone()),
        )
        .await;
        assert_eq!(call_resume_non_terminal(restarted_actor).await, 1);
        wait_for_event(&events, "payment_send").await;

        let recovered = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
        let payout = store
            .get_liquidity_chain_tx(&swap.swap_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.failure_reason, None);
        assert_eq!(payout.failure_reason, None);
        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn successful_validation_preserves_unrelated_swap_failure_context() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 66);
        let unrelated_reason = "historical payment recovery warning".to_string();
        store
            .update_liquidity_swap(
                &swap.swap_id,
                LiquiditySwapUpdate {
                    failure_reason: Some(unrelated_reason.clone()),
                    updated_at: now_ms(),
                    ..Default::default()
                },
            )
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        assert_eq!(call_resume_non_terminal(actor).await, 1);
        wait_for_event(&events, "payment_send").await;

        assert_eq!(
            store
                .get_liquidity_swap(&swap.swap_id)
                .unwrap()
                .unwrap()
                .failure_reason,
            Some(unrelated_reason)
        );
    }

    #[tokio::test]
    async fn recovered_client_payout_locked_validates_and_pays_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 62);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor =
            spawn_test_liquidity_actor(store, payment, TestLiquidityChain::new(events.clone()))
                .await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 1);
        assert_eq!(call_resume_non_terminal(actor.clone()).await, 0);
        wait_for_event(&events, "payment_send").await;
        actor
            .send_message(LiquidityActorMessage::RetryPayoutValidation(
                swap.swap_id,
                ClientPayoutAuthorization::PayoutLockedRecovery,
                PAYOUT_VALIDATION_MAX_ATTEMPTS,
                1,
            ))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 1);
        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn recovered_client_payout_locked_transient_burst_then_periodic_success_pays_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (_swap, _) = insert_recovered_client_payout_locked(&store, 63);
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Transient("temporary 1".to_string())),
            Err(PayoutValidationError::Transient("temporary 2".to_string())),
            Err(PayoutValidationError::Transient("temporary 3".to_string())),
            Err(PayoutValidationError::Transient("temporary 4".to_string())),
            Ok(()),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store, payment, chain).await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 1);
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 5).await;
        wait_for_event(&events, "payment_send").await;
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn transient_payout_validation_exhaustion_retries_periodically_then_pays_once() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        let outpoint = OutPoint::new(Byte32::from_slice(&[16u8; 32]).unwrap(), 0);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint)).unwrap();
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![
            Err(PayoutValidationError::Transient(
                "temporary ckb rpc failure 0".to_string(),
            )),
            Err(PayoutValidationError::Transient(
                "temporary ckb rpc failure 1".to_string(),
            )),
            Err(PayoutValidationError::Transient(
                "temporary ckb rpc failure 2".to_string(),
            )),
            Err(PayoutValidationError::Transient(
                "temporary ckb rpc failure 3".to_string(),
            )),
            Ok(()),
        ]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event_count(&events, "validate_observed_loop_out_payout", 5).await;
        wait_for_event(&events, "payment_send").await;

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        let payout_tx = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_ne!(swap.state, LiquiditySwapState::PayoutPending);
        assert_eq!(payout_tx.status, LiquidityChainTxStatus::Confirmed);
        assert_eq!(payout_tx.failure_reason, None);
        assert_eq!(payment_requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn terminal_progression_stops_queued_payout_validation_retry() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        let outpoint = OutPoint::new(Byte32::from_slice(&[16u8; 32]).unwrap(), 1);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint)).unwrap();
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![Err(PayoutValidationError::Transient(
            "temporary ckb rpc failure".to_string(),
        ))]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "validate_observed_loop_out_payout").await;
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::RefundPending,
                    updated_at: now_ms(),
                    reason: None,
                },
            )
            .unwrap();
        tokio::time::sleep(PAYOUT_VALIDATION_RETRY_INTERVAL * 3).await;
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 1);
        assert!(payment_requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn recovered_payout_locked_terminal_progression_stops_queued_validation_retry() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let (swap, _) = insert_recovered_client_payout_locked(&store, 64);
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![Err(PayoutValidationError::Transient(
            "temporary ckb rpc failure".to_string(),
        ))]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        events.borrow_mut().clear();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        assert_eq!(call_resume_non_terminal(actor.clone()).await, 1);
        wait_for_event(&events, "validate_observed_loop_out_payout").await;
        store
            .update_liquidity_swap_state(
                &swap.swap_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::RefundPending,
                    updated_at: now_ms(),
                    reason: None,
                },
            )
            .unwrap();
        tokio::time::sleep(PAYOUT_VALIDATION_RETRY_INTERVAL * 3).await;

        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 1);
        assert!(payment_requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn definitive_payout_validation_failure_does_not_retry() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        let outpoint = OutPoint::new(Byte32::from_slice(&[17u8; 32]).unwrap(), 0);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), Some(outpoint)).unwrap();
        let mut chain = TestLiquidityChain::new(events.clone());
        chain.set_observed_loop_out_payout_results(vec![Err(PayoutValidationError::Definitive(
            "payout amount mismatch".to_string(),
        ))]);
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let actor = spawn_test_liquidity_actor(store.clone(), payment, chain).await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "validate_observed_loop_out_payout").await;
        tokio::time::sleep(PAYOUT_VALIDATION_RETRY_INTERVAL * 2).await;

        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        let payout_tx = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .unwrap();
        assert_eq!(swap.state, LiquiditySwapState::PayoutPending);
        assert!(swap
            .failure_reason
            .unwrap()
            .contains("payout amount mismatch"));
        assert_eq!(payout_tx.status, LiquidityChainTxStatus::Confirmed);
        assert!(payout_tx
            .failure_reason
            .unwrap()
            .contains("payout amount mismatch"));
        assert_eq!(event_count(&events, "validate_observed_loop_out_payout"), 1);
        assert!(payment_requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn late_payout_rejection_does_not_downgrade_confirmed_payout_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        store
            .insert_loop_out_quote(quote.clone(), now_ms())
            .unwrap();
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [14u8; 32].into(),
                outpoint: Some(OutPoint::new(Byte32::from_slice(&[14u8; 32]).unwrap(), 14)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        actor
            .send_message(LiquidityActorMessage::ChainTxRejected(
                quote.quote_id,
                LiquidityChainTxRole::Payout,
                "late rejected".to_string(),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn duplicate_claim_confirmed_repairs_broadcast_claim_tx_record() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let quote = test_loop_out_quote(now_ms() + 60_000);
        create_client_loop_out(&store, quote.clone(), now_ms(), None).unwrap();
        mark_client_payout_locked(&store, quote.quote_id, now_ms() + 1).unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::PaymentInFlight,
                    updated_at: now_ms() + 2,
                    reason: None,
                },
            )
            .unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::PaymentSettled,
                    updated_at: now_ms() + 3,
                    reason: None,
                },
            )
            .unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::ClaimPending,
                    updated_at: now_ms() + 4,
                    reason: None,
                },
            )
            .unwrap();
        store
            .update_liquidity_swap_state(
                &quote.quote_id,
                LiquidityStateTransition {
                    state: LiquiditySwapState::Success,
                    updated_at: now_ms() + 5,
                    reason: None,
                },
            )
            .unwrap();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Claim,
                tx_hash: [9u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: now_ms() + 2,
                updated_at: now_ms() + 2,
            })
            .unwrap();
        let actor = spawn_test_liquidity_actor(
            store.clone(),
            TestLoopOutPayment::new(events.clone()),
            TestLiquidityChain::new(events.clone()),
        )
        .await;

        actor
            .send_message(LiquidityActorMessage::ClaimConfirmed(quote.quote_id))
            .unwrap();
        call_resume_non_terminal_result(actor).await.unwrap();

        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn loop_out_payment_request_targets_quote_provider() {
        let events = Shared::new(Vec::new());
        let store = TestLiquidityStore::new(events.clone(), "client");
        let payment = TestLoopOutPayment::new(events.clone());
        let payment_requests = payment.requests.clone();
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);

        store.insert_loop_out_quote(quote.clone(), now_ms).unwrap();
        create_client_loop_out(
            &store,
            quote.clone(),
            now_ms,
            Some(OutPoint::new(Byte32::from_slice(&[33u8; 32]).unwrap(), 0)),
        )
        .unwrap();
        let actor =
            spawn_test_liquidity_actor(store, payment, TestLiquidityChain::new(events.clone()))
                .await;

        actor
            .send_message(LiquidityActorMessage::PayoutConfirmed(quote.quote_id))
            .unwrap();
        wait_for_event(&events, "payment_send").await;

        assert_eq!(payment_requests.borrow().len(), 1);
        assert_eq!(
            payment_requests.borrow()[0].target_pubkey,
            Some(quote.provider)
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
