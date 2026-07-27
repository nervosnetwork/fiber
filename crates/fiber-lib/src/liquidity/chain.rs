//! Chain adapter boundary for loop-out liquidity operations.

use std::collections::HashMap;

use async_trait::async_trait;
use ckb_types::{
    bytes::Bytes,
    core::tx_pool::TxStatus,
    core::TransactionView,
    packed,
    prelude::{Builder, Entity, Pack},
};
use fiber_types::{Hash256, HashAlgorithm, LiquidityChainTxRole, LiquiditySwapState};
use ractor::{ActorRef, RpcReplyPort};

use crate::ckb::contracts::get_udt_cell_deps;
use crate::ckb::{
    CkbChainMessage, CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult, FundingRequest, FundingTx,
};
use crate::liquidity::actor::LiquidityActorMessage;
use crate::liquidity::quote::loop_in_gross_onchain_amount;
use crate::liquidity::store::{LiquidityStore, LiquiditySwapRecord, LiquiditySwapRole};
use crate::liquidity::tx::{
    build_liquidity_lock_claim_witness, build_liquidity_lock_output,
    build_liquidity_lock_refund_witness, build_liquidity_lock_script, LiquidityLockBuildError,
    LiquidityLockOutputParams, LiquidityLockScriptArtifact,
};
use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};
use crate::now_timestamp_as_millis_u64;

#[cfg(not(test))]
const CKB_SEND_TX_TIMEOUT_MS: u64 = 8000;
#[cfg(test)]
const CKB_SEND_TX_TIMEOUT_MS: u64 = 50;
const DEFAULT_LIQUIDITY_PAYOUT_FEE_RATE: u64 = 1000;

/// Chain claim request for a client Loop Out payout.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LoopOutClaimRequest {
    /// Local swap identifier being claimed.
    pub swap_id: Hash256,
    /// Persisted payment preimage required to unlock the claim path.
    pub payment_preimage: Hash256,
}

/// Restart-time chain action for a persisted Loop Out swap.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ChainResumeAction {
    /// Watch the already persisted payout lock outpoint.
    WatchPayout(packed::OutPoint),
    /// Rebuild and broadcast the payout lock because no outpoint is persisted yet.
    RebuildAndBroadcastPayout,
}

impl ChainResumeAction {
    /// Plan the chain action for a persisted `PayoutPending` Loop Out record.
    pub fn for_payout_pending(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        if record.state != LiquiditySwapState::PayoutPending {
            return Err(LiquidityLoopOutError::InvalidStateTransition {
                from: record.state,
                to: LiquiditySwapState::PayoutPending,
            });
        }

        Ok(match &record.onchain_outpoint {
            Some(outpoint) => Self::WatchPayout(outpoint.clone()),
            None => Self::RebuildAndBroadcastPayout,
        })
    }
}

/// Restart-safe claim plan for a client Loop Out payout.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LoopOutClaimPlan {
    /// Local swap identifier being claimed.
    pub swap_id: Hash256,
    /// Persisted non-default payment preimage required by the claim path.
    pub payment_preimage: Hash256,
}

impl LoopOutClaimPlan {
    /// Validate that a payment preimage is non-default and matches the expected payment hash.
    pub fn validate_payment_preimage(
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<(), LiquidityLoopOutError> {
        if payment_preimage == Hash256::default() {
            return Err(LiquidityLoopOutError::Chain(
                "cannot claim loop out payout with default payment preimage".to_string(),
            ));
        }
        let expected_payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(payment_preimage).into();
        if expected_payment_hash != payment_hash {
            return Err(LiquidityLoopOutError::Chain(
                "cannot claim loop out payout: payment preimage does not match payment hash"
                    .to_string(),
            ));
        }

        Ok(())
    }

    /// Build a claim plan from a persisted swap record.
    pub fn from_record(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        let Some(payment_preimage) = record.payment_preimage else {
            return Err(LiquidityLoopOutError::Chain(
                "cannot claim loop out payout without payment preimage".to_string(),
            ));
        };
        Self::validate_payment_preimage(record.payment_hash, payment_preimage)?;

        Ok(Self {
            swap_id: record.swap_id,
            payment_preimage,
        })
    }
}

impl From<LoopOutClaimPlan> for LoopOutClaimRequest {
    fn from(plan: LoopOutClaimPlan) -> Self {
        Self {
            swap_id: plan.swap_id,
            payment_preimage: plan.payment_preimage,
        }
    }
}

/// Pure transaction plan for a Loop Out payout transaction.
#[derive(Debug, Clone)]
pub struct LoopOutPayoutTxPlan {
    /// Local swap identifier being paid out.
    pub swap_id: Hash256,
    /// Fully built transaction to broadcast in a later chain integration step.
    pub transaction: TransactionView,
    /// Transaction hash derived from `transaction`.
    pub tx_hash: Hash256,
    /// Payout lock output outpoint derived from transaction hash and output index.
    pub outpoint: packed::OutPoint,
}

impl LoopOutPayoutTxPlan {
    /// Build a payout transaction plan and derive its persisted identity.
    pub fn new(swap_id: Hash256, transaction: TransactionView, output_index: u32) -> Self {
        let tx_hash = transaction.hash();
        let outpoint = packed::OutPoint::new(tx_hash.clone(), output_index);
        Self {
            swap_id,
            transaction,
            tx_hash: tx_hash.into(),
            outpoint,
        }
    }
}

/// Pure transaction plan for claiming a Loop Out payout.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutClaimTxPlan {
    /// Local swap identifier being claimed.
    pub swap_id: Hash256,
    /// Persisted payout lock outpoint to spend.
    pub payout_outpoint: packed::OutPoint,
    /// Validated payment preimage required by the claim path.
    pub payment_preimage: Hash256,
}

impl LoopOutClaimTxPlan {
    /// Build a claim transaction plan from a persisted swap record.
    pub fn from_record(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        let claim = LoopOutClaimPlan::from_record(record)?;
        let payout_outpoint = record.onchain_outpoint.clone().ok_or_else(|| {
            LiquidityLoopOutError::Chain("cannot build claim without payout outpoint".to_string())
        })?;

        Ok(Self {
            swap_id: claim.swap_id,
            payout_outpoint,
            payment_preimage: claim.payment_preimage,
        })
    }
}

/// Pure transaction plan for refunding an expired provider Loop Out payout.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutRefundTxPlan {
    /// Local swap identifier being refunded.
    pub swap_id: Hash256,
    /// Persisted payout lock outpoint to spend.
    pub payout_outpoint: packed::OutPoint,
    /// Absolute lock time after which the refund path is valid.
    pub refund_after_lock_time: u64,
}

impl LoopOutRefundTxPlan {
    /// Build a refund transaction plan from a persisted provider refund-pending swap record.
    pub fn from_record(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        if record.role != LiquiditySwapRole::Provider {
            return Err(LiquidityLoopOutError::Chain(
                "cannot build refund for non-provider loop out record".to_string(),
            ));
        }
        if record.state != LiquiditySwapState::RefundPending {
            return Err(LiquidityLoopOutError::Chain(
                "cannot build refund unless provider swap is refund pending".to_string(),
            ));
        }
        let payout_outpoint = record.onchain_outpoint.clone().ok_or_else(|| {
            LiquidityLoopOutError::Chain("cannot build refund without payout outpoint".to_string())
        })?;

        Ok(Self {
            swap_id: record.swap_id,
            payout_outpoint,
            refund_after_lock_time: record.refund_after_lock_time,
        })
    }
}

/// Chain boundary required by Loop Out liquidity workflows.
#[async_trait]
pub trait LiquidityChainWatcher {
    /// Adapter-specific error returned by chain operations.
    type Error;

    /// Reserve the payout lock outpoint for the accepted quote before broadcast.
    async fn reserve_payout_lock_outpoint(
        &mut self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<packed::OutPoint, Self::Error>;

    /// Broadcast the payout lock transaction for the accepted quote and outpoint.
    async fn broadcast_payout_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Broadcast the client Loop In lock transaction for the accepted quote.
    async fn broadcast_loop_in_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Schedule payout lock watching and report completion back to `myself`.
    async fn watch_payout_lock(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Broadcast the claim transaction for a paid Loop Out swap.
    async fn broadcast_claim(
        &mut self,
        request: LoopOutClaimRequest,
        _myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Schedule client claim watching and report completion back to `myself`.
    async fn watch_claim(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Broadcast the provider refund transaction for an expired payout lock.
    async fn broadcast_refund(&mut self, record: &LiquiditySwapRecord) -> Result<(), Self::Error>;

    /// Schedule provider refund watching and report completion back to `myself`.
    async fn watch_refund(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;
}

/// CKB-backed liquidity chain watcher boundary.
pub struct CkbLiquidityChainWatcher<S> {
    ckb_chain_actor: ActorRef<CkbChainMessage>,
    store: S,
    liquidity_lock_artifact: Option<LiquidityLockScriptArtifact>,
    #[allow(dead_code)]
    liquidity_lock_cell_deps: Vec<packed::CellDep>,
    pending_payout_txs: HashMap<Hash256, TransactionView>,
}

impl<S> CkbLiquidityChainWatcher<S> {
    /// Create a liquidity chain watcher backed by the CKB chain actor.
    pub fn new(ckb_chain_actor: ActorRef<CkbChainMessage>, store: S) -> Self {
        Self {
            ckb_chain_actor,
            store,
            liquidity_lock_artifact: None,
            liquidity_lock_cell_deps: Vec::new(),
            pending_payout_txs: HashMap::new(),
        }
    }

    /// Create a liquidity chain watcher with the deployed liquidity-lock script artifact.
    pub fn new_with_liquidity_lock_artifact(
        ckb_chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        liquidity_lock_artifact: LiquidityLockScriptArtifact,
    ) -> Self {
        Self {
            ckb_chain_actor,
            store,
            liquidity_lock_artifact: Some(liquidity_lock_artifact),
            liquidity_lock_cell_deps: Vec::new(),
            pending_payout_txs: HashMap::new(),
        }
    }

    /// Create a liquidity chain watcher from the configured base liquidity-lock script.
    pub fn new_with_liquidity_lock_script(
        ckb_chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        liquidity_lock_script: packed::Script,
        liquidity_lock_cell_deps: Vec<packed::CellDep>,
    ) -> Self {
        Self {
            ckb_chain_actor,
            store,
            liquidity_lock_artifact: Some(LiquidityLockScriptArtifact {
                code_hash: liquidity_lock_script.code_hash(),
                hash_type: liquidity_lock_script.hash_type(),
            }),
            liquidity_lock_cell_deps,
            pending_payout_txs: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn send_and_trace(
        &self,
        swap_id: Hash256,
        role: LiquidityChainTxRole,
        transaction: TransactionView,
        liquidity_actor: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let tx_hash: Hash256 = transaction.hash().into();
        let send_result = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::SendTx,
            CKB_SEND_TX_TIMEOUT_MS,
            transaction
        )
        .map_err(|error| {
            LiquidityLoopOutError::Chain(format!("send tx actor call failed: {error}"))
        })?;
        send_result.map_err(|error| {
            LiquidityLoopOutError::Chain(format!(
                "send tx failed for liquidity tx {tx_hash}: {error}"
            ))
        })?;

        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::tracer_callback_for(swap_id, role, liquidity_actor),
            }))
            .map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "create tx tracer failed for liquidity tx {tx_hash}: {error}"
                ))
            })?;

        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn tracer_callback_for(
        swap_id: Hash256,
        role: LiquidityChainTxRole,
        liquidity_actor: ActorRef<LiquidityActorMessage>,
    ) -> RpcReplyPort<CkbTxTracingResult> {
        let (sender, receiver) = tokio::sync::oneshot::channel::<CkbTxTracingResult>();
        tokio::spawn(async move {
            let Ok(result) = receiver.await else {
                return;
            };
            let Some(message) = Self::continuation_for_tracing_result(swap_id, role, &result)
            else {
                return;
            };
            let _ = liquidity_actor.send_message(message);
        });
        RpcReplyPort::from(sender)
    }

    fn payout_tracer_callback_for(
        swap_id: Hash256,
        tx_hash: Hash256,
        ckb_chain_actor: ActorRef<CkbChainMessage>,
        liquidity_actor: ActorRef<LiquidityActorMessage>,
    ) -> RpcReplyPort<CkbTxTracingResult> {
        let (sender, receiver) = tokio::sync::oneshot::channel::<CkbTxTracingResult>();
        tokio::spawn(async move {
            let Ok(result) = receiver.await else {
                return;
            };
            let TxStatus::Committed(block_number, ..) = result.tx_status else {
                return;
            };
            let _ = ckb_chain_actor
                .send_message(CkbChainMessage::CommitFundingTx(tx_hash, block_number));
            let _ = liquidity_actor.send_message(LiquidityActorMessage::PayoutConfirmed(swap_id));
        });
        RpcReplyPort::from(sender)
    }

    fn continuation_for_tracing_result(
        swap_id: Hash256,
        role: LiquidityChainTxRole,
        result: &CkbTxTracingResult,
    ) -> Option<LiquidityActorMessage> {
        Some(match &result.tx_status {
            TxStatus::Committed(..) => match role {
                LiquidityChainTxRole::Payout => LiquidityActorMessage::PayoutConfirmed(swap_id),
                LiquidityChainTxRole::Claim => LiquidityActorMessage::ClaimConfirmed(swap_id),
                LiquidityChainTxRole::Refund => LiquidityActorMessage::RefundConfirmed(swap_id),
            },
            TxStatus::Rejected(reason) => {
                LiquidityActorMessage::ChainTxRejected(swap_id, role, reason.clone())
            }
            _ => return None,
        })
    }
}

impl<S> CkbLiquidityChainWatcher<S>
where
    S: LiquidityStore,
{
    fn missing_payout_builder() -> LiquidityLoopOutError {
        LiquidityLoopOutError::Chain(
            "cannot build provider payout transaction: deployed liquidity-lock script artifact is not configured"
                .to_string(),
        )
    }

    fn payout_output_params(quote: &LoopOutQuoteTerms) -> LiquidityLockOutputParams {
        LiquidityLockOutputParams {
            payment_hash: quote.payment_hash.into(),
            claimant_lock: quote.claimant_lock.clone(),
            refund_lock: quote.refund_lock.clone(),
            refund_after_lock_time: quote.refund_after_lock_time,
            amount: quote.amount,
            asset_type_script: quote.asset.udt_type_script.clone().map(Into::into),
            capacity: quote.capacity_requirement_ckb,
        }
    }

    fn build_payout_funding_request(
        &self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<FundingRequest, LiquidityLoopOutError> {
        let artifact = self
            .liquidity_lock_artifact
            .as_ref()
            .ok_or_else(Self::missing_payout_builder)?;
        let params = Self::payout_output_params(quote);

        let local_reserved_ckb_amount = if params.asset_type_script.is_some() {
            quote.capacity_requirement_ckb
        } else {
            let amount = u64::try_from(quote.amount).map_err(|_| {
                LiquidityLoopOutError::Chain(
                    "cannot build provider payout transaction: CKB amount overflows u64"
                        .to_string(),
                )
            })?;
            quote.capacity_requirement_ckb.checked_sub(amount).ok_or_else(|| {
                LiquidityLoopOutError::Chain(
                    "cannot build provider payout transaction: capacity requirement below CKB amount"
                        .to_string(),
                )
            })?
        };

        Ok(FundingRequest {
            script: build_liquidity_lock_script(artifact, &params),
            udt_type_script: params.asset_type_script,
            local_amount: quote.amount,
            funding_fee_rate: DEFAULT_LIQUIDITY_PAYOUT_FEE_RATE,
            remote_amount: 0,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount: 0,
        })
    }

    fn payout_outpoint_for_signed_tx(
        tx: &TransactionView,
        liquidity_lock_script: &packed::Script,
    ) -> Result<packed::OutPoint, LiquidityLoopOutError> {
        let output_index = tx
            .outputs()
            .into_iter()
            .position(|output| output.lock() == *liquidity_lock_script)
            .ok_or_else(|| {
                LiquidityLoopOutError::Chain(
                    "signed provider payout transaction does not contain liquidity-lock output"
                        .to_string(),
                )
            })?;
        let output_index = u32::try_from(output_index).map_err(|_| {
            LiquidityLoopOutError::Chain(
                "signed provider payout transaction output index overflows u32".to_string(),
            )
        })?;

        Ok(packed::OutPoint::new(tx.hash(), output_index))
    }
}

#[async_trait]
impl<S> LiquidityChainWatcher for CkbLiquidityChainWatcher<S>
where
    S: LiquidityStore + Send + Sync,
{
    type Error = LiquidityLoopOutError;

    async fn reserve_payout_lock_outpoint(
        &mut self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<packed::OutPoint, Self::Error> {
        let request = self.build_payout_funding_request(quote)?;
        let liquidity_lock_script = request.script.clone();
        let funded_tx = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::Fund,
            CKB_SEND_TX_TIMEOUT_MS,
            FundingTx::new(),
            request
        )
        .map_err(|error| LiquidityLoopOutError::Chain(format!("fund actor call failed: {error}")))?
        .map_err(|error| LiquidityLoopOutError::Chain(format!("fund payout tx failed: {error}")))?;
        let funded_tx_hash: Option<Hash256> = funded_tx.as_ref().map(|tx| tx.hash().into());
        let signed_tx = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::Sign,
            CKB_SEND_TX_TIMEOUT_MS,
            funded_tx
        )
        .map_err(|error| LiquidityLoopOutError::Chain(format!("sign actor call failed: {error}")))?
        .map_err(|error| {
            if let Some(funded_tx_hash) = funded_tx_hash {
                let _ = self
                    .ckb_chain_actor
                    .send_message(CkbChainMessage::RemoveFundingTx(funded_tx_hash));
            }
            LiquidityLoopOutError::Chain(format!("sign payout tx failed: {error}"))
        })?;
        let tx = signed_tx.as_ref().cloned().ok_or_else(|| {
            LiquidityLoopOutError::Chain("signed provider payout funding tx is empty".to_string())
        })?;
        let outpoint = Self::payout_outpoint_for_signed_tx(&tx, &liquidity_lock_script)?;
        let now = now_timestamp_as_millis_u64();
        self.store
            .insert_liquidity_chain_tx(fiber_types::LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: tx.hash().into(),
                outpoint: Some(outpoint.clone()),
                status: fiber_types::LiquidityChainTxStatus::Planned,
                failure_reason: None,
                created_at: now,
                updated_at: now,
            })
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        if let Some(funded_tx_hash) = funded_tx_hash {
            if funded_tx_hash != tx.hash().into() {
                self.ckb_chain_actor
                    .send_message(CkbChainMessage::RemoveFundingTx(funded_tx_hash))
                    .map_err(|error| {
                        LiquidityLoopOutError::Chain(format!(
                            "remove unsigned payout funding tx reservation failed: {error}"
                        ))
                    })?;
                self.ckb_chain_actor
                    .send_message(CkbChainMessage::AddFundingTx(signed_tx))
                    .map_err(|error| {
                        LiquidityLoopOutError::Chain(format!(
                            "add signed payout funding tx reservation failed: {error}"
                        ))
                    })?;
            }
        }
        self.pending_payout_txs.insert(quote.quote_id, tx);

        Ok(outpoint)
    }

    async fn broadcast_payout_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        self.liquidity_lock_artifact
            .as_ref()
            .ok_or_else(Self::missing_payout_builder)?;
        let record = self
            .store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Chain(
                    "cannot broadcast payout without persisted transaction identity".to_string(),
                )
            })?;
        if record.outpoint.as_ref() != Some(outpoint) {
            return Err(LiquidityLoopOutError::Chain(
                "cannot broadcast payout: requested outpoint does not match persisted payout record"
                    .to_string(),
            ));
        }
        let tx = self.pending_payout_txs.get(&quote.quote_id).cloned().ok_or_else(|| {
            LiquidityLoopOutError::Chain(
                "cannot broadcast payout after restart without pending signed transaction; refusing to rebuild unsafely"
                    .to_string(),
            )
        })?;
        let tx_hash: Hash256 = tx.hash().into();
        if tx_hash != record.tx_hash {
            return Err(LiquidityLoopOutError::Chain(
                "cannot broadcast payout: pending transaction hash does not match persisted payout record"
                    .to_string(),
            ));
        }

        self.store
            .update_liquidity_chain_tx_status(
                &quote.quote_id,
                LiquidityChainTxRole::Payout,
                fiber_types::LiquidityChainTxStatus::Broadcast,
                None,
                now_timestamp_as_millis_u64(),
            )
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        let send_result = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::SendTx,
            CKB_SEND_TX_TIMEOUT_MS,
            tx.clone()
        )
        .map_err(|error| {
            LiquidityLoopOutError::Chain(format!("send tx actor call failed: {error}"))
        })?;
        if let Err(error) = send_result {
            let failure_reason = format!("send tx failed for liquidity tx {tx_hash}: {error}");
            self.store
                .update_liquidity_chain_tx_status(
                    &quote.quote_id,
                    LiquidityChainTxRole::Payout,
                    fiber_types::LiquidityChainTxStatus::Rejected,
                    Some(failure_reason.clone()),
                    now_timestamp_as_millis_u64(),
                )
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
            return Err(LiquidityLoopOutError::Chain(failure_reason));
        }

        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::payout_tracer_callback_for(
                    quote.quote_id,
                    tx_hash,
                    self.ckb_chain_actor.clone(),
                    myself,
                ),
            }))
            .map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "create tx tracer failed for liquidity tx {tx_hash}: {error}"
                ))
            })?;
        self.pending_payout_txs.remove(&quote.quote_id);

        Ok(())
    }

    async fn watch_payout_lock(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let record = self
            .store
            .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Payout)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Chain(
                    "cannot watch payout without persisted transaction identity".to_string(),
                )
            })?;
        if !matches!(
            record.status,
            fiber_types::LiquidityChainTxStatus::Broadcast
                | fiber_types::LiquidityChainTxStatus::Confirmed
        ) {
            return Err(LiquidityLoopOutError::Chain(format!(
                "cannot watch payout transaction with non-broadcast status {:?}",
                record.status
            )));
        }
        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash: record.tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::payout_tracer_callback_for(
                    swap_id,
                    record.tx_hash,
                    self.ckb_chain_actor.clone(),
                    myself,
                ),
            }))
            .map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "create tx tracer failed for liquidity tx {}: {error}",
                    record.tx_hash
                ))
            })?;
        Ok(())
    }

    async fn broadcast_loop_in_lock(
        &mut self,
        _quote: &LoopOutQuoteTerms,
        _myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        Err(LiquidityLoopOutError::Chain(
            "loop in lock broadcast is not wired to CKB yet".to_string(),
        ))
    }

    async fn broadcast_claim(
        &mut self,
        request: LoopOutClaimRequest,
        _myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let swap = self
            .store
            .get_liquidity_swap(&request.swap_id)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!(
                    "liquidity swap not found: {:?}",
                    request.swap_id
                ))
            })?;
        let quote = self
            .store
            .get_loop_out_quote(&swap.quote_id)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!(
                    "loop out quote not found: {:?}",
                    swap.quote_id
                ))
            })?;
        let plan = LoopOutClaimTxPlan::from_record(&swap)?;
        if plan.payment_preimage != request.payment_preimage {
            return Err(LiquidityLoopOutError::Chain(
                "claim request preimage does not match persisted swap preimage".to_string(),
            ));
        }
        let mut claim_cell_deps = self.liquidity_lock_cell_deps.clone();
        if let Some(udt_type_script) = &quote.asset.udt_type_script {
            let udt_type_script: packed::Script = udt_type_script.clone().into();
            let udt_cell_deps = get_udt_cell_deps(&udt_type_script).await.map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "cannot resolve UDT cell deps for claim transaction: {error}"
                ))
            })?;
            claim_cell_deps.extend(udt_cell_deps.into_iter());
        }
        let tx = build_loop_out_claim_transaction(
            &quote,
            &plan.payout_outpoint,
            plan.payment_preimage,
            &claim_cell_deps,
        )?;
        let tx_hash: Hash256 = tx.hash().into();
        let now = now_timestamp_as_millis_u64();
        if let Some(existing) = self
            .store
            .get_liquidity_chain_tx(&request.swap_id, LiquidityChainTxRole::Claim)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
        {
            if matches!(
                existing.status,
                fiber_types::LiquidityChainTxStatus::Broadcast
                    | fiber_types::LiquidityChainTxStatus::Confirmed
            ) {
                return Ok(());
            }
            if existing.tx_hash != tx_hash {
                return Err(LiquidityLoopOutError::Chain(
                    "rebuilt claim transaction hash does not match persisted claim record"
                        .to_string(),
                ));
            }
            self.store
                .update_liquidity_chain_tx_status(
                    &request.swap_id,
                    LiquidityChainTxRole::Claim,
                    fiber_types::LiquidityChainTxStatus::Planned,
                    None,
                    now,
                )
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        } else {
            self.store
                .insert_liquidity_chain_tx(fiber_types::LiquidityChainTxRecord {
                    swap_id: request.swap_id,
                    role: LiquidityChainTxRole::Claim,
                    tx_hash,
                    outpoint: None,
                    status: fiber_types::LiquidityChainTxStatus::Planned,
                    failure_reason: None,
                    created_at: now,
                    updated_at: now,
                })
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        }
        let send_result = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::SendTx,
            CKB_SEND_TX_TIMEOUT_MS,
            tx
        );
        let send_result = match send_result {
            Ok(send_result) => send_result,
            Err(error) => {
                let failure_reason = format!("send tx actor call failed: {error}");
                self.store
                    .update_liquidity_chain_tx_status(
                        &request.swap_id,
                        LiquidityChainTxRole::Claim,
                        fiber_types::LiquidityChainTxStatus::Rejected,
                        Some(failure_reason.clone()),
                        now_timestamp_as_millis_u64(),
                    )
                    .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
                return Err(LiquidityLoopOutError::Chain(failure_reason));
            }
        };
        if let Err(error) = send_result {
            let failure_reason = format!("send tx failed for liquidity tx {tx_hash}: {error}");
            self.store
                .update_liquidity_chain_tx_status(
                    &request.swap_id,
                    LiquidityChainTxRole::Claim,
                    fiber_types::LiquidityChainTxStatus::Rejected,
                    Some(failure_reason.clone()),
                    now_timestamp_as_millis_u64(),
                )
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
            return Err(LiquidityLoopOutError::Chain(failure_reason));
        }
        self.store
            .update_liquidity_chain_tx_status(
                &request.swap_id,
                LiquidityChainTxRole::Claim,
                fiber_types::LiquidityChainTxStatus::Broadcast,
                None,
                now_timestamp_as_millis_u64(),
            )
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        Ok(())
    }

    async fn watch_claim(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let record = self
            .store
            .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Claim)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Chain(
                    "cannot watch claim without persisted transaction identity".to_string(),
                )
            })?;
        if !matches!(
            record.status,
            fiber_types::LiquidityChainTxStatus::Broadcast
                | fiber_types::LiquidityChainTxStatus::Confirmed
        ) {
            return Err(LiquidityLoopOutError::Chain(format!(
                "cannot watch claim transaction with non-broadcast status {:?}",
                record.status
            )));
        }
        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash: record.tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::tracer_callback_for(swap_id, LiquidityChainTxRole::Claim, myself),
            }))
            .map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "create tx tracer failed for liquidity tx {}: {error}",
                    record.tx_hash
                ))
            })?;
        Ok(())
    }

    async fn broadcast_refund(&mut self, record: &LiquiditySwapRecord) -> Result<(), Self::Error> {
        let quote = self
            .store
            .get_loop_out_quote(&record.quote_id)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!(
                    "loop out quote not found: {:?}",
                    record.quote_id
                ))
            })?;
        let plan = LoopOutRefundTxPlan::from_record(record)?;
        let mut refund_cell_deps = self.liquidity_lock_cell_deps.clone();
        if let Some(udt_type_script) = &quote.asset.udt_type_script {
            let udt_type_script: packed::Script = udt_type_script.clone().into();
            let udt_cell_deps = get_udt_cell_deps(&udt_type_script).await.map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "cannot resolve UDT cell deps for refund transaction: {error}"
                ))
            })?;
            refund_cell_deps.extend(udt_cell_deps.into_iter());
        }
        let tx = build_loop_out_refund_transaction(
            &quote,
            &plan.payout_outpoint,
            plan.refund_after_lock_time,
            &refund_cell_deps,
        )?;
        let tx_hash: Hash256 = tx.hash().into();
        let now = now_timestamp_as_millis_u64();
        if let Some(existing) = self
            .store
            .get_liquidity_chain_tx(&record.swap_id, LiquidityChainTxRole::Refund)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
        {
            if matches!(
                existing.status,
                fiber_types::LiquidityChainTxStatus::Broadcast
                    | fiber_types::LiquidityChainTxStatus::Confirmed
            ) {
                return Ok(());
            }
            if existing.tx_hash != tx_hash {
                return Err(LiquidityLoopOutError::Chain(
                    "rebuilt refund transaction hash does not match persisted refund record"
                        .to_string(),
                ));
            }
            self.store
                .update_liquidity_chain_tx_status(
                    &record.swap_id,
                    LiquidityChainTxRole::Refund,
                    fiber_types::LiquidityChainTxStatus::Planned,
                    None,
                    now,
                )
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        } else {
            self.store
                .insert_liquidity_chain_tx(fiber_types::LiquidityChainTxRecord {
                    swap_id: record.swap_id,
                    role: LiquidityChainTxRole::Refund,
                    tx_hash,
                    outpoint: None,
                    status: fiber_types::LiquidityChainTxStatus::Planned,
                    failure_reason: None,
                    created_at: now,
                    updated_at: now,
                })
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        }
        let send_result = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::SendTx,
            CKB_SEND_TX_TIMEOUT_MS,
            tx
        );
        let send_result = match send_result {
            Ok(send_result) => send_result,
            Err(error) => {
                let failure_reason = format!("send tx actor call failed: {error}");
                self.store
                    .update_liquidity_chain_tx_status(
                        &record.swap_id,
                        LiquidityChainTxRole::Refund,
                        fiber_types::LiquidityChainTxStatus::Rejected,
                        Some(failure_reason.clone()),
                        now_timestamp_as_millis_u64(),
                    )
                    .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
                return Err(LiquidityLoopOutError::Chain(failure_reason));
            }
        };
        if let Err(error) = send_result {
            let failure_reason = format!("send tx failed for liquidity tx {tx_hash}: {error}");
            self.store
                .update_liquidity_chain_tx_status(
                    &record.swap_id,
                    LiquidityChainTxRole::Refund,
                    fiber_types::LiquidityChainTxStatus::Rejected,
                    Some(failure_reason.clone()),
                    now_timestamp_as_millis_u64(),
                )
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
            return Err(LiquidityLoopOutError::Chain(failure_reason));
        }
        self.store
            .update_liquidity_chain_tx_status(
                &record.swap_id,
                LiquidityChainTxRole::Refund,
                fiber_types::LiquidityChainTxStatus::Broadcast,
                None,
                now_timestamp_as_millis_u64(),
            )
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        Ok(())
    }

    async fn watch_refund(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let record = self
            .store
            .get_liquidity_chain_tx(&swap_id, LiquidityChainTxRole::Refund)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Chain(
                    "cannot watch refund without persisted transaction identity".to_string(),
                )
            })?;
        if !matches!(
            record.status,
            fiber_types::LiquidityChainTxStatus::Broadcast
                | fiber_types::LiquidityChainTxStatus::Confirmed
        ) {
            return Err(LiquidityLoopOutError::Chain(format!(
                "cannot watch refund transaction with non-broadcast status {:?}",
                record.status
            )));
        }
        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash: record.tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::tracer_callback_for(swap_id, LiquidityChainTxRole::Refund, myself),
            }))
            .map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "create tx tracer failed for liquidity tx {}: {error}",
                    record.tx_hash
                ))
            })?;
        Ok(())
    }
}

/// Request to build the on-chain payout output for a loop-out swap.
#[derive(Debug, Clone)]
pub struct LoopOutPayoutRequest {
    /// CKB-hash of the 32-byte payment preimage.
    pub payment_hash: [u8; 32],
    /// Lock allowed to claim the payout with the payment preimage.
    pub claimant_lock: packed::Script,
    /// Lock allowed to refund the payout after the timeout.
    pub refund_lock: packed::Script,
    /// Absolute `since` value required before refund is valid.
    pub refund_after_lock_time: u64,
    /// Raw CKB/UDT amount protected by the liquidity-lock script.
    pub amount: u128,
    /// UDT type script for UDT payouts, absent for CKB payouts.
    pub asset_type_script: Option<packed::Script>,
    /// Cell capacity in shannons.
    pub capacity: u64,
}

/// Build the lock output and data pair for a loop-out payout cell.
pub fn build_loop_out_payout_output(
    artifact: &LiquidityLockScriptArtifact,
    request: &LoopOutPayoutRequest,
) -> Result<(packed::CellOutput, packed::Bytes), LiquidityLockBuildError> {
    build_liquidity_lock_output(
        artifact,
        &LiquidityLockOutputParams {
            payment_hash: request.payment_hash,
            claimant_lock: request.claimant_lock.clone(),
            refund_lock: request.refund_lock.clone(),
            refund_after_lock_time: request.refund_after_lock_time,
            amount: request.amount,
            asset_type_script: request.asset_type_script.clone(),
            capacity: request.capacity,
        },
    )
}

fn loop_in_quote_with_gross_amount(
    quote: &LoopOutQuoteTerms,
) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError> {
    let mut quote = quote.clone();
    quote.amount = loop_in_gross_onchain_amount(&quote)?;
    Ok(quote)
}

fn build_loop_in_terminal_output(
    quote: &LoopOutQuoteTerms,
    lock: packed::Script,
    gross_amount: u128,
) -> (packed::CellOutput, packed::Bytes) {
    let mut output = packed::CellOutput::new_builder()
        .capacity(quote.capacity_requirement_ckb.max(1))
        .lock(lock);
    let output_data = if let Some(udt_type_script) = &quote.asset.udt_type_script {
        let udt_type_script: packed::Script = udt_type_script.clone().into();
        output = output.type_(Some(udt_type_script).pack());
        Bytes::from(gross_amount.to_le_bytes().to_vec()).pack()
    } else {
        Bytes::new().pack()
    };

    (output.build(), output_data)
}

/// Build the client-funded liquidity-lock output for a Loop In swap.
pub fn build_loop_in_client_lock_output(
    artifact: &LiquidityLockScriptArtifact,
    quote: &LoopOutQuoteTerms,
) -> Result<(packed::CellOutput, packed::Bytes), LiquidityLoopOutError> {
    let gross_quote = loop_in_quote_with_gross_amount(quote)?;
    build_loop_out_payout_output(
        artifact,
        &LoopOutPayoutRequest {
            payment_hash: gross_quote.payment_hash.into(),
            claimant_lock: gross_quote.claimant_lock,
            refund_lock: gross_quote.refund_lock,
            refund_after_lock_time: gross_quote.refund_after_lock_time,
            amount: gross_quote.amount,
            asset_type_script: gross_quote.asset.udt_type_script.map(Into::into),
            capacity: gross_quote.capacity_requirement_ckb,
        },
    )
    .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))
}

/// Build a provider claim transaction spending a Loop In client lock.
pub fn build_loop_in_provider_claim_transaction(
    quote: &LoopOutQuoteTerms,
    client_lock_outpoint: &packed::OutPoint,
    payment_preimage: Hash256,
    liquidity_lock_cell_deps: &[packed::CellDep],
) -> Result<TransactionView, LiquidityLoopOutError> {
    let gross_amount = loop_in_gross_onchain_amount(quote)?;
    let (output, output_data) =
        build_loop_in_terminal_output(quote, quote.claimant_lock.clone(), gross_amount);
    let cell_deps = liquidity_lock_cell_deps.to_vec();

    Ok(TransactionView::new_advanced_builder()
        .input(packed::CellInput::new(client_lock_outpoint.clone(), 0))
        .output(output)
        .output_data(output_data)
        .set_cell_deps(cell_deps)
        .witness(build_liquidity_lock_claim_witness(payment_preimage.into()))
        .build())
}

/// Build a client refund transaction spending an expired Loop In client lock.
pub fn build_loop_in_client_refund_transaction(
    quote: &LoopOutQuoteTerms,
    client_lock_outpoint: &packed::OutPoint,
    refund_after_lock_time: u64,
    liquidity_lock_cell_deps: &[packed::CellDep],
) -> Result<TransactionView, LiquidityLoopOutError> {
    if refund_after_lock_time != quote.refund_after_lock_time {
        return Err(LiquidityLoopOutError::Chain(
            "loop in refund lock time does not match quote".to_string(),
        ));
    }

    let gross_amount = loop_in_gross_onchain_amount(quote)?;
    let (output, output_data) =
        build_loop_in_terminal_output(quote, quote.refund_lock.clone(), gross_amount);
    let cell_deps = liquidity_lock_cell_deps.to_vec();

    Ok(TransactionView::new_advanced_builder()
        .input(packed::CellInput::new(
            client_lock_outpoint.clone(),
            quote.refund_after_lock_time,
        ))
        .output(output)
        .output_data(output_data)
        .set_cell_deps(cell_deps)
        .witness(build_liquidity_lock_refund_witness())
        .build())
}

/// Build a claim transaction spending one liquidity-lock payout cell.
pub fn build_loop_out_claim_transaction(
    quote: &LoopOutQuoteTerms,
    payout_outpoint: &packed::OutPoint,
    payment_preimage: Hash256,
    liquidity_lock_cell_deps: &[packed::CellDep],
) -> Result<TransactionView, LiquidityLoopOutError> {
    let payout_capacity = quote
        .capacity_requirement_ckb
        .checked_sub(quote.onchain_fee_estimate_ckb)
        .filter(|capacity| {
            quote.asset.udt_type_script.is_some() || u128::from(*capacity) >= quote.amount
        })
        .unwrap_or(quote.capacity_requirement_ckb);
    let mut output = packed::CellOutput::new_builder()
        .capacity(payout_capacity)
        .lock(quote.claimant_lock.clone());
    let output_data = if let Some(udt_type_script) = &quote.asset.udt_type_script {
        let udt_type_script: packed::Script = udt_type_script.clone().into();
        output = output.type_(Some(udt_type_script).pack());
        Bytes::from(quote.amount.to_le_bytes().to_vec()).pack()
    } else {
        Bytes::new().pack()
    };
    let cell_deps = liquidity_lock_cell_deps.to_vec();

    Ok(TransactionView::new_advanced_builder()
        .input(packed::CellInput::new(payout_outpoint.clone(), 0))
        .output(output.build())
        .output_data(output_data)
        .set_cell_deps(cell_deps)
        .witness(build_liquidity_lock_claim_witness(payment_preimage.into()))
        .build())
}

/// Build a refund transaction spending one expired liquidity-lock payout cell.
pub fn build_loop_out_refund_transaction(
    quote: &LoopOutQuoteTerms,
    payout_outpoint: &packed::OutPoint,
    refund_after_lock_time: u64,
    liquidity_lock_cell_deps: &[packed::CellDep],
) -> Result<TransactionView, LiquidityLoopOutError> {
    let payout_capacity = quote
        .capacity_requirement_ckb
        .checked_sub(quote.onchain_fee_estimate_ckb)
        .filter(|capacity| {
            quote.asset.udt_type_script.is_some() || u128::from(*capacity) >= quote.amount
        })
        .unwrap_or(quote.capacity_requirement_ckb);
    let mut output = packed::CellOutput::new_builder()
        .capacity(payout_capacity)
        .lock(quote.refund_lock.clone());
    let output_data = if let Some(udt_type_script) = &quote.asset.udt_type_script {
        let udt_type_script: packed::Script = udt_type_script.clone().into();
        output = output.type_(Some(udt_type_script).pack());
        Bytes::from(quote.amount.to_le_bytes().to_vec()).pack()
    } else {
        Bytes::new().pack()
    };
    let cell_deps = liquidity_lock_cell_deps.to_vec();

    Ok(TransactionView::new_advanced_builder()
        .input(packed::CellInput::new(
            payout_outpoint.clone(),
            refund_after_lock_time,
        ))
        .output(output.build())
        .output_data(output_data)
        .set_cell_deps(cell_deps)
        .witness(build_liquidity_lock_refund_witness())
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ckb_types::prelude::{Builder, Entity, Pack, PackVec};
    use ckb_types::{
        bytes::Bytes,
        core::{tx_pool::TxStatus, TransactionView},
        packed, H256,
    };
    use fiber_types::{
        Hash256, HashAlgorithm, LiquidityAsset, LiquidityAssetKind, LiquidityChainTxRecord,
        LiquidityChainTxRole, LiquidityChainTxStatus, LiquiditySwapState, Pubkey,
    };
    use ractor::{Actor, ActorProcessingErr};
    use secp256k1::{SecretKey, SECP256K1};
    use tokio::sync::mpsc;

    use crate::ckb::{CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult, FundingTx};
    use crate::liquidity::store::{
        LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
        LiquiditySwapKind, LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapRole,
        LiquiditySwapUpdate,
    };

    struct AsyncLiquidityChainWatcher;

    #[async_trait]
    impl LiquidityChainWatcher for AsyncLiquidityChainWatcher {
        type Error = LiquidityLoopOutError;

        async fn reserve_payout_lock_outpoint(
            &mut self,
            _quote: &LoopOutQuoteTerms,
        ) -> Result<packed::OutPoint, Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn broadcast_payout_lock(
            &mut self,
            _quote: &LoopOutQuoteTerms,
            _outpoint: &packed::OutPoint,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn watch_payout_lock(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn broadcast_loop_in_lock(
            &mut self,
            _quote: &LoopOutQuoteTerms,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn broadcast_claim(
            &mut self,
            _request: LoopOutClaimRequest,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn watch_claim(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn broadcast_refund(
            &mut self,
            _record: &LiquiditySwapRecord,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn watch_refund(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }
    }

    #[test]
    fn liquidity_chain_watcher_accepts_async_implementations() {
        fn assert_chain_watcher<T: LiquidityChainWatcher>() {}

        assert_chain_watcher::<AsyncLiquidityChainWatcher>();
    }

    #[derive(Clone, Default)]
    struct NoopLiquidityStore {
        quotes: Arc<Mutex<HashMap<Hash256, LoopOutQuoteTerms>>>,
        swaps: Arc<Mutex<HashMap<Hash256, LiquiditySwapRecord>>>,
        chain_txs: Arc<Mutex<HashMap<(Hash256, LiquidityChainTxRole), LiquidityChainTxRecord>>>,
    }

    impl LiquidityStore for NoopLiquidityStore {
        fn insert_loop_out_quote(
            &self,
            quote: LoopOutQuoteTerms,
            _created_at: u64,
        ) -> Result<(), LiquidityStoreError> {
            self.quotes.lock().unwrap().insert(quote.quote_id, quote);
            Ok(())
        }

        fn get_loop_out_quote(
            &self,
            quote_id: &Hash256,
        ) -> Result<Option<LoopOutQuoteTerms>, LiquidityStoreError> {
            Ok(self.quotes.lock().unwrap().get(quote_id).cloned())
        }

        fn insert_liquidity_swap(
            &self,
            swap: LiquiditySwapRecord,
        ) -> Result<(), LiquidityStoreError> {
            self.swaps.lock().unwrap().insert(swap.swap_id, swap);
            Ok(())
        }

        fn get_liquidity_swap(
            &self,
            swap_id: &Hash256,
        ) -> Result<Option<LiquiditySwapRecord>, LiquidityStoreError> {
            Ok(self.swaps.lock().unwrap().get(swap_id).cloned())
        }

        fn list_liquidity_swaps(
            &self,
            _filter: LiquiditySwapFilter,
        ) -> Result<LiquiditySwapPage, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn list_liquidity_swaps_by_states(
            &self,
            _states: &[LiquiditySwapState],
            _swap_kind: LiquiditySwapKind,
        ) -> Result<Vec<LiquiditySwapRecord>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn update_liquidity_swap_state(
            &self,
            _swap_id: &Hash256,
            _transition: LiquidityStateTransition,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn update_liquidity_swap(
            &self,
            _swap_id: &Hash256,
            _update: LiquiditySwapUpdate,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn insert_liquidity_chain_tx(
            &self,
            record: LiquidityChainTxRecord,
        ) -> Result<(), LiquidityStoreError> {
            self.chain_txs
                .lock()
                .unwrap()
                .insert((record.swap_id, record.role), record);
            Ok(())
        }

        fn get_liquidity_chain_tx(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
        ) -> Result<Option<LiquidityChainTxRecord>, LiquidityStoreError> {
            Ok(self
                .chain_txs
                .lock()
                .unwrap()
                .get(&(*swap_id, role))
                .cloned())
        }

        fn update_liquidity_chain_tx_status(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
            status: LiquidityChainTxStatus,
            failure_reason: Option<String>,
            updated_at: u64,
        ) -> Result<(), LiquidityStoreError> {
            let mut chain_txs = self.chain_txs.lock().unwrap();
            let record = chain_txs
                .get_mut(&(*swap_id, role))
                .ok_or_else(|| LiquidityStoreError::Backend("missing chain tx".to_string()))?;
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
                .lock()
                .unwrap()
                .values()
                .filter(|record| statuses.contains(&record.status))
                .cloned()
                .collect())
        }

        fn upsert_liquidity_asset(
            &self,
            _asset: LiquidityAsset,
        ) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn get_liquidity_asset(
            &self,
            _asset_id: &str,
        ) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }
    }

    #[derive(Debug, Clone, Eq, PartialEq)]
    enum MockCkbEvent {
        Fund {
            script: packed::Script,
            local_amount: u128,
            local_reserved_ckb_amount: u64,
        },
        Sign,
        RemoveFundingTx(Hash256),
        AddFundingTx(Hash256),
        SendTx,
        CreateTxTracer(CkbTxTracingMask),
        CommitFundingTx(Hash256),
    }

    struct PayoutMockCkbActor;

    struct PayoutMockCkbActorArgs {
        events: Arc<Mutex<Vec<MockCkbEvent>>>,
        funded_tx: TransactionView,
        signed_tx: TransactionView,
        send_error: bool,
    }

    struct PayoutMockCkbActorState {
        events: Arc<Mutex<Vec<MockCkbEvent>>>,
        funded_tx: TransactionView,
        signed_tx: TransactionView,
        send_error: bool,
    }

    #[async_trait::async_trait]
    impl Actor for PayoutMockCkbActor {
        type Msg = CkbChainMessage;
        type State = PayoutMockCkbActorState;
        type Arguments = PayoutMockCkbActorArgs;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            args: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(PayoutMockCkbActorState {
                events: args.events,
                funded_tx: args.funded_tx,
                signed_tx: args.signed_tx,
                send_error: args.send_error,
            })
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            match message {
                CkbChainMessage::Fund(_, request, reply) => {
                    state.events.lock().unwrap().push(MockCkbEvent::Fund {
                        script: request.script,
                        local_amount: request.local_amount,
                        local_reserved_ckb_amount: request.local_reserved_ckb_amount,
                    });
                    let _ = reply.send(Ok(FundingTx::from(state.funded_tx.clone())));
                }
                CkbChainMessage::Sign(_, reply) => {
                    state.events.lock().unwrap().push(MockCkbEvent::Sign);
                    let _ = reply.send(Ok(FundingTx::from(state.signed_tx.clone())));
                }
                CkbChainMessage::SendTx(_, reply) => {
                    state.events.lock().unwrap().push(MockCkbEvent::SendTx);
                    let result = if state.send_error {
                        Err(ckb_sdk::RpcError::Other(anyhow::anyhow!(
                            "mock payout send error"
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                CkbChainMessage::RemoveFundingTx(tx_hash) => {
                    state
                        .events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::RemoveFundingTx(tx_hash));
                }
                CkbChainMessage::AddFundingTx(tx) => {
                    state
                        .events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::AddFundingTx(
                            tx.as_ref().expect("signed tx exists").hash().into(),
                        ));
                }
                CkbChainMessage::CommitFundingTx(tx_hash, _) => {
                    state
                        .events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::CommitFundingTx(tx_hash));
                }
                CkbChainMessage::CreateTxTracer(CkbTxTracer { mask, callback, .. }) => {
                    state
                        .events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::CreateTxTracer(mask));
                    let _ = callback.send(committed_result(state.signed_tx.hash().into()));
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct MockCkbActor;

    #[async_trait::async_trait]
    impl Actor for MockCkbActor {
        type Msg = CkbChainMessage;
        type State = Arc<Mutex<Vec<MockCkbEvent>>>;
        type Arguments = Arc<Mutex<Vec<MockCkbEvent>>>;

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
                CkbChainMessage::SendTx(_, reply) => {
                    events.lock().unwrap().push(MockCkbEvent::SendTx);
                    let _ = reply.send(Ok(()));
                }
                CkbChainMessage::CreateTxTracer(CkbTxTracer { mask, .. }) => {
                    events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::CreateTxTracer(mask));
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct NoReplyCkbActor;

    struct NoReplyCkbActorState {
        events: Arc<Mutex<Vec<MockCkbEvent>>>,
        _replies: Vec<ractor::RpcReplyPort<Result<(), ckb_sdk::RpcError>>>,
    }

    #[async_trait::async_trait]
    impl Actor for NoReplyCkbActor {
        type Msg = CkbChainMessage;
        type State = NoReplyCkbActorState;
        type Arguments = Arc<Mutex<Vec<MockCkbEvent>>>;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            events: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(NoReplyCkbActorState {
                events,
                _replies: Vec::new(),
            })
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            match message {
                CkbChainMessage::SendTx(_, reply) => {
                    state.events.lock().unwrap().push(MockCkbEvent::SendTx);
                    state._replies.push(reply);
                }
                CkbChainMessage::CreateTxTracer(CkbTxTracer { mask, .. }) => {
                    state
                        .events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::CreateTxTracer(mask));
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct SendErrorCkbActor;

    #[async_trait::async_trait]
    impl Actor for SendErrorCkbActor {
        type Msg = CkbChainMessage;
        type State = Arc<Mutex<Vec<MockCkbEvent>>>;
        type Arguments = Arc<Mutex<Vec<MockCkbEvent>>>;

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
                CkbChainMessage::SendTx(_, reply) => {
                    events.lock().unwrap().push(MockCkbEvent::SendTx);
                    let _ = reply.send(Err(ckb_sdk::RpcError::Other(anyhow::anyhow!(
                        "mock send tx error"
                    ))));
                }
                CkbChainMessage::CreateTxTracer(CkbTxTracer { mask, .. }) => {
                    events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::CreateTxTracer(mask));
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct MockLiquidityActor;

    #[async_trait::async_trait]
    impl Actor for MockLiquidityActor {
        type Msg = LiquidityActorMessage;
        type State = mpsc::UnboundedSender<LiquidityActorMessage>;
        type Arguments = mpsc::UnboundedSender<LiquidityActorMessage>;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            sender: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(sender)
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            sender: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            let _ = sender.send(message);
            Ok(())
        }
    }

    async fn spawn_mock_liquidity_actor() -> (
        ActorRef<LiquidityActorMessage>,
        mpsc::UnboundedReceiver<LiquidityActorMessage>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (actor, _handle) = ractor::Actor::spawn(None, MockLiquidityActor, sender)
            .await
            .unwrap();
        (actor, receiver)
    }

    async fn wait_for_mock_events(events: &Arc<Mutex<Vec<MockCkbEvent>>>, len: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if events.lock().unwrap().len() == len {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mock ckb actor records expected events");
    }

    fn script(args: &'static str) -> packed::Script {
        packed::Script::new_builder()
            .args(Bytes::from(args).pack())
            .build()
    }

    fn test_outpoint(index: u32) -> packed::OutPoint {
        packed::OutPoint::new(
            packed::Byte32::from_slice(&[index as u8; 32]).unwrap(),
            index,
        )
    }

    fn test_loop_out_quote_terms() -> LoopOutQuoteTerms {
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
            payment_hash: HashAlgorithm::CkbHash.hash([4u8; 32]).into(),
            expires_at: 20_000,
            payout_deadline: 30_000,
            refund_after_lock_time: 40_000,
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
        }
    }

    fn test_loop_in_quote_terms() -> LoopOutQuoteTerms {
        test_loop_out_quote_terms()
    }

    fn test_loop_in_udt_quote_terms() -> (LoopOutQuoteTerms, packed::Script) {
        let mut quote = test_loop_in_quote_terms();
        let udt_type_script = script("loop-in-udt");
        quote.asset = LiquidityAsset {
            asset_id: "udt".to_string(),
            kind: LiquidityAssetKind::Udt,
            udt_type_script: Some(udt_type_script.clone().into()),
            min_amount: 1,
            max_amount: 1_000,
            available_capacity: 2_000,
            base_fee: 1,
            proportional_fee_ppm: 0,
            enabled: true,
        };
        quote.amount = 100;
        quote.provider_fee = 7;
        (quote, udt_type_script)
    }

    fn test_transaction_with_liquidity_output(
        quote: &LoopOutQuoteTerms,
        output_index: u32,
    ) -> TransactionView {
        let outputs: Vec<_> = (0..=output_index)
            .map(|index| {
                packed::CellOutput::new_builder()
                    .capacity(quote.capacity_requirement_ckb + u64::from(index))
                    .lock(script("liquidity-output"))
                    .build()
            })
            .collect();
        let outputs_data: Vec<packed::Bytes> =
            outputs.iter().map(|_| Bytes::new().pack()).collect();

        TransactionView::new_advanced_builder()
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build()
    }

    fn liquidity_lock_artifact() -> LiquidityLockScriptArtifact {
        LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        }
    }

    fn liquidity_lock_script_for_quote(quote: &LoopOutQuoteTerms) -> packed::Script {
        crate::liquidity::tx::build_liquidity_lock_script(
            &liquidity_lock_artifact(),
            &LiquidityLockOutputParams {
                payment_hash: quote.payment_hash.into(),
                claimant_lock: quote.claimant_lock.clone(),
                refund_lock: quote.refund_lock.clone(),
                refund_after_lock_time: quote.refund_after_lock_time,
                amount: quote.amount,
                asset_type_script: quote.asset.udt_type_script.clone().map(Into::into),
                capacity: quote.capacity_requirement_ckb,
            },
        )
    }

    fn test_funding_transaction_with_script(
        quote: &LoopOutQuoteTerms,
        output_index: u32,
    ) -> TransactionView {
        let liquidity_script = liquidity_lock_script_for_quote(quote);
        let udt_type_script: Option<packed::Script> =
            quote.asset.udt_type_script.clone().map(Into::into);
        let outputs: Vec<_> = (0..=output_index)
            .map(|index| {
                let lock = if index == output_index {
                    liquidity_script.clone()
                } else {
                    script("change")
                };
                packed::CellOutput::new_builder()
                    .capacity(quote.capacity_requirement_ckb + u64::from(index))
                    .lock(lock)
                    .type_(udt_type_script.clone().pack())
                    .build()
            })
            .collect();
        let outputs_data: Vec<packed::Bytes> = outputs
            .iter()
            .map(|_| {
                if quote.asset.udt_type_script.is_some() {
                    Bytes::from(quote.amount.to_le_bytes().to_vec()).pack()
                } else {
                    Bytes::new().pack()
                }
            })
            .collect();

        TransactionView::new_advanced_builder()
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build()
    }

    fn committed_result(tx_hash: Hash256) -> CkbTxTracingResult {
        CkbTxTracingResult {
            tx_hash,
            tx_status: TxStatus::Committed(1, H256::default(), 0),
        }
    }

    async fn assert_callback_maps_committed_status(
        role: LiquidityChainTxRole,
        expected: impl FnOnce(Hash256) -> LiquidityActorMessage,
    ) {
        let swap_id: Hash256 = [7u8; 32].into();
        let tx_hash: Hash256 = [8u8; 32].into();
        let (liquidity_actor, mut receiver) = spawn_mock_liquidity_actor().await;
        assert!(
            CkbLiquidityChainWatcher::<NoopLiquidityStore>::continuation_for_tracing_result(
                swap_id,
                role,
                &CkbTxTracingResult::unknown(tx_hash)
            )
            .is_none()
        );
        assert!(matches!(
            CkbLiquidityChainWatcher::<NoopLiquidityStore>::continuation_for_tracing_result(
                swap_id,
                role,
                &CkbTxTracingResult {
                    tx_hash,
                    tx_status: TxStatus::Rejected("rejected".to_string()),
                }
            ),
            Some(LiquidityActorMessage::ChainTxRejected(
                actual_swap_id,
                actual_role,
                reason,
            )) if actual_swap_id == swap_id && actual_role == role && reason == "rejected"
        ));

        CkbLiquidityChainWatcher::<NoopLiquidityStore>::tracer_callback_for(
            swap_id,
            role,
            liquidity_actor,
        )
        .send(committed_result(tx_hash))
        .expect("callback accepts committed status");

        let message = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .expect("liquidity actor receives continuation")
            .expect("mock actor is alive");
        match (message, expected(swap_id)) {
            (
                LiquidityActorMessage::PayoutConfirmed(actual),
                LiquidityActorMessage::PayoutConfirmed(expected),
            )
            | (
                LiquidityActorMessage::ClaimConfirmed(actual),
                LiquidityActorMessage::ClaimConfirmed(expected),
            )
            | (
                LiquidityActorMessage::RefundConfirmed(actual),
                LiquidityActorMessage::RefundConfirmed(expected),
            ) => {
                assert_eq!(actual, expected);
            }
            (actual, expected) => panic!("unexpected message {actual:?}, expected {expected:?}"),
        }
    }

    fn test_swap_record_with_outpoint(outpoint: packed::OutPoint) -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            swap_id: [1u8; 32].into(),
            quote_id: [2u8; 32].into(),
            role: LiquiditySwapRole::Client,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: "ckb".to_string(),
            state: LiquiditySwapState::PayoutPending,
            payment_hash: [3u8; 32].into(),
            payment_preimage: None,
            amount: 1000,
            onchain_outpoint: Some(outpoint),
            payout_deadline: Some(2000),
            refund_after_lock_time: 3000,
            expires_at: 4000,
            failure_reason: None,
            created_at: 5000,
            updated_at: 6000,
        }
    }

    fn test_provider_refund_pending_record(outpoint: packed::OutPoint) -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            role: LiquiditySwapRole::Provider,
            state: LiquiditySwapState::RefundPending,
            onchain_outpoint: Some(outpoint),
            ..test_swap_record_with_outpoint(test_outpoint(8))
        }
    }

    fn test_swap_record_without_outpoint() -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            onchain_outpoint: None,
            ..test_swap_record_with_outpoint(test_outpoint(7))
        }
    }

    fn test_swap_record_without_preimage() -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            state: LiquiditySwapState::PaymentSettled,
            payment_preimage: None,
            ..test_swap_record_without_outpoint()
        }
    }

    fn test_swap_record_with_preimage(preimage: Hash256) -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            payment_hash: HashAlgorithm::CkbHash.hash(preimage).into(),
            payment_preimage: Some(preimage),
            ..test_swap_record_without_preimage()
        }
    }

    fn test_swap_record_with_mismatched_preimage(preimage: Hash256) -> LiquiditySwapRecord {
        LiquiditySwapRecord {
            payment_hash: HashAlgorithm::CkbHash.hash([8u8; 32]).into(),
            payment_preimage: Some(preimage),
            ..test_swap_record_without_preimage()
        }
    }

    #[test]
    fn payout_builder_reuses_liquidity_lock_output_builder() {
        let artifact = crate::liquidity::tx::LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let request = LoopOutPayoutRequest {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 1000,
            asset_type_script: None,
            capacity: 2000,
        };

        let (output, data) = build_loop_out_payout_output(&artifact, &request).unwrap();

        assert_eq!(u64::from(output.capacity()), 2000);
        assert!(data.raw_data().is_empty());
    }

    #[test]
    fn payout_builder_forwards_udt_type_and_amount_data() {
        let artifact = crate::liquidity::tx::LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let udt_type_script = script("udt");
        let request = LoopOutPayoutRequest {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 1000,
            asset_type_script: Some(udt_type_script.clone()),
            capacity: 2000,
        };

        let (output, data) = build_loop_out_payout_output(&artifact, &request).unwrap();

        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(data.raw_data().as_ref(), 1000u128.to_le_bytes());
    }

    #[test]
    fn loop_in_client_lock_output_uses_provider_claim_and_client_refund_locks() {
        let artifact = liquidity_lock_artifact();
        let quote = test_loop_in_quote_terms();

        let (output, data) = build_loop_in_client_lock_output(&artifact, &quote)
            .expect("loop in client lock output");

        assert_eq!(u64::from(output.capacity()), quote.capacity_requirement_ckb);
        assert_eq!(output.lock().code_hash(), artifact.code_hash);
        assert_eq!(
            output.lock(),
            build_liquidity_lock_script(
                &artifact,
                &LiquidityLockOutputParams {
                    payment_hash: quote.payment_hash.into(),
                    claimant_lock: quote.claimant_lock.clone(),
                    refund_lock: quote.refund_lock.clone(),
                    refund_after_lock_time: quote.refund_after_lock_time,
                    amount: quote.amount + quote.provider_fee,
                    asset_type_script: None,
                    capacity: quote.capacity_requirement_ckb,
                },
            )
        );
        assert!(data.raw_data().is_empty());
    }

    #[test]
    fn loop_in_client_lock_output_uses_gross_udt_amount() {
        let artifact = liquidity_lock_artifact();
        let (quote, udt_type_script) = test_loop_in_udt_quote_terms();

        let (output, data) =
            build_loop_in_client_lock_output(&artifact, &quote).expect("loop in udt lock output");

        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(data.raw_data().as_ref(), 107u128.to_le_bytes());
    }

    #[test]
    fn loop_in_client_lock_output_rejects_gross_overflow() {
        let artifact = liquidity_lock_artifact();
        let mut quote = test_loop_in_quote_terms();
        quote.amount = u128::MAX;
        quote.provider_fee = 1;

        let error = build_loop_in_client_lock_output(&artifact, &quote)
            .expect_err("gross amount should overflow");

        assert_eq!(error, LiquidityLoopOutError::GrossAmountOverflow);
    }

    #[test]
    fn loop_in_provider_claim_spends_client_lock_with_preimage_witness() {
        let quote = test_loop_in_quote_terms();
        let client_lock_outpoint = test_outpoint(22);
        let preimage: Hash256 = [4u8; 32].into();
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(23))
            .dep_type(ckb_types::core::DepType::Code)
            .build();

        let tx = build_loop_in_provider_claim_transaction(
            &quote,
            &client_lock_outpoint,
            preimage,
            &[cell_dep.clone()],
        )
        .expect("loop in provider claim transaction");

        assert_eq!(tx.inputs().len(), 1);
        assert_eq!(
            tx.inputs().get(0).unwrap().previous_output(),
            client_lock_outpoint
        );
        let output = tx.outputs().get(0).unwrap();
        assert_eq!(output.lock(), quote.claimant_lock);
        assert!(output.type_().to_opt().is_none());
        assert!(tx.outputs_data().get(0).unwrap().raw_data().is_empty());
        assert_eq!(
            tx.cell_deps().into_iter().collect::<Vec<_>>(),
            vec![cell_dep]
        );
        assert_eq!(
            tx.witnesses().get(0).unwrap(),
            build_liquidity_lock_claim_witness(preimage.into())
        );
    }

    #[test]
    fn loop_in_client_refund_spends_client_lock_with_since_and_refund_witness() {
        let quote = test_loop_in_quote_terms();
        let client_lock_outpoint = test_outpoint(24);
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(25))
            .dep_type(ckb_types::core::DepType::Code)
            .build();

        let tx = build_loop_in_client_refund_transaction(
            &quote,
            &client_lock_outpoint,
            quote.refund_after_lock_time,
            &[cell_dep.clone()],
        )
        .expect("loop in client refund transaction");

        assert_eq!(tx.inputs().len(), 1);
        let input = tx.inputs().get(0).unwrap();
        assert_eq!(input.previous_output(), client_lock_outpoint);
        assert_eq!(u64::from(input.since()), quote.refund_after_lock_time);
        let output = tx.outputs().get(0).unwrap();
        assert_eq!(output.lock(), quote.refund_lock);
        assert!(output.type_().to_opt().is_none());
        assert!(tx.outputs_data().get(0).unwrap().raw_data().is_empty());
        assert_eq!(
            tx.cell_deps().into_iter().collect::<Vec<_>>(),
            vec![cell_dep]
        );
        assert_eq!(
            tx.witnesses().get(0).unwrap(),
            build_liquidity_lock_refund_witness()
        );
    }

    #[test]
    fn loop_in_provider_claim_udt_output_capacity_remains_nonzero_when_fee_equals_capacity() {
        let (mut quote, udt_type_script) = test_loop_in_udt_quote_terms();
        quote.capacity_requirement_ckb = 1_000;
        quote.onchain_fee_estimate_ckb = 1_000;

        let tx = build_loop_in_provider_claim_transaction(
            &quote,
            &test_outpoint(26),
            [4u8; 32].into(),
            &[],
        )
        .expect("loop in provider claim transaction");

        let output = tx.outputs().get(0).unwrap();
        assert!(u64::from(output.capacity()) > 0);
        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(
            tx.outputs_data().get(0).unwrap().raw_data().as_ref(),
            107u128.to_le_bytes()
        );
    }

    #[test]
    fn loop_in_client_refund_udt_output_capacity_remains_nonzero_when_fee_equals_capacity() {
        let (mut quote, udt_type_script) = test_loop_in_udt_quote_terms();
        quote.capacity_requirement_ckb = 1_000;
        quote.onchain_fee_estimate_ckb = 1_000;

        let tx = build_loop_in_client_refund_transaction(
            &quote,
            &test_outpoint(27),
            quote.refund_after_lock_time,
            &[],
        )
        .expect("loop in client refund transaction");

        let output = tx.outputs().get(0).unwrap();
        assert!(u64::from(output.capacity()) > 0);
        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(
            tx.outputs_data().get(0).unwrap().raw_data().as_ref(),
            107u128.to_le_bytes()
        );
    }

    #[test]
    fn loop_in_client_refund_rejects_mismatched_refund_lock_time() {
        let quote = test_loop_in_quote_terms();

        let error = build_loop_in_client_refund_transaction(
            &quote,
            &test_outpoint(28),
            quote.refund_after_lock_time + 1,
            &[],
        )
        .expect_err("mismatched refund lock time should fail");

        assert!(matches!(
            error,
            LiquidityLoopOutError::Chain(message) if message.contains("refund lock time")
        ));
    }

    #[test]
    fn claim_transaction_spends_payout_to_claimant_with_preimage_witness_and_cell_deps() {
        let quote = test_loop_out_quote_terms();
        let payout_outpoint = test_outpoint(7);
        let preimage: Hash256 = [4u8; 32].into();
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(9))
            .dep_type(ckb_types::core::DepType::Code)
            .build();

        let tx = build_loop_out_claim_transaction(
            &quote,
            &payout_outpoint,
            preimage,
            &[cell_dep.clone()],
        )
        .expect("claim transaction");

        assert_eq!(tx.inputs().len(), 1);
        assert_eq!(
            tx.inputs().get(0).unwrap().previous_output(),
            payout_outpoint
        );
        assert_eq!(tx.outputs().len(), 1);
        let output = tx.outputs().get(0).unwrap();
        assert_eq!(output.lock(), quote.claimant_lock);
        assert!(output.type_().to_opt().is_none());
        assert!(tx.outputs_data().get(0).unwrap().raw_data().is_empty());
        assert_eq!(
            tx.cell_deps().into_iter().collect::<Vec<_>>(),
            vec![cell_dep]
        );
        assert_eq!(
            tx.witnesses().get(0).unwrap(),
            build_liquidity_lock_claim_witness(preimage.into())
        );
    }

    #[test]
    fn claim_transaction_preserves_udt_output_and_appended_cell_deps() {
        let mut quote = test_loop_out_quote_terms();
        let udt_type_script = script("udt");
        quote.asset = LiquidityAsset {
            asset_id: "udt".to_string(),
            kind: LiquidityAssetKind::Udt,
            udt_type_script: Some(udt_type_script.clone().into()),
            min_amount: 1,
            max_amount: 1_000,
            available_capacity: 1_000,
            base_fee: 1,
            proportional_fee_ppm: 0,
            enabled: true,
        };
        let liquidity_lock_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(17))
            .dep_type(ckb_types::core::DepType::Code)
            .build();
        let udt_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(18))
            .dep_type(ckb_types::core::DepType::DepGroup)
            .build();

        let tx = build_loop_out_claim_transaction(
            &quote,
            &test_outpoint(19),
            [4u8; 32].into(),
            &[liquidity_lock_dep.clone(), udt_dep.clone()],
        )
        .expect("UDT claim transaction");

        let output = tx.outputs().get(0).unwrap();
        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(
            tx.outputs_data().get(0).unwrap().raw_data().as_ref(),
            quote.amount.to_le_bytes()
        );
        assert_eq!(
            tx.cell_deps().into_iter().collect::<Vec<_>>(),
            vec![liquidity_lock_dep, udt_dep]
        );
    }

    #[test]
    fn refund_transaction_spends_payout_to_refund_lock_with_since_and_witness() {
        let quote = test_loop_out_quote_terms();
        let payout_outpoint = test_outpoint(20);
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(21))
            .dep_type(ckb_types::core::DepType::Code)
            .build();

        let tx = build_loop_out_refund_transaction(
            &quote,
            &payout_outpoint,
            quote.refund_after_lock_time,
            &[cell_dep.clone()],
        )
        .expect("refund transaction");

        assert_eq!(tx.inputs().len(), 1);
        let input = tx.inputs().get(0).unwrap();
        assert_eq!(input.previous_output(), payout_outpoint);
        assert_eq!(u64::from(input.since()), quote.refund_after_lock_time);
        assert_eq!(tx.outputs().len(), 1);
        let output = tx.outputs().get(0).unwrap();
        assert_eq!(output.lock(), quote.refund_lock);
        assert!(output.type_().to_opt().is_none());
        assert!(tx.outputs_data().get(0).unwrap().raw_data().is_empty());
        assert_eq!(
            tx.cell_deps().into_iter().collect::<Vec<_>>(),
            vec![cell_dep]
        );
        assert_eq!(
            tx.witnesses().get(0).unwrap(),
            build_liquidity_lock_refund_witness()
        );
    }

    #[test]
    fn chain_watcher_broadcast_plan_reuses_persisted_outpoint() {
        let outpoint = test_outpoint(42);
        let record = test_swap_record_with_outpoint(outpoint.clone());

        let action = ChainResumeAction::for_payout_pending(&record).unwrap();

        assert_eq!(action, ChainResumeAction::WatchPayout(outpoint));
    }

    #[test]
    fn chain_watcher_payout_pending_without_outpoint_rebuilds_without_duplicate_outpoint() {
        let record = test_swap_record_without_outpoint();

        let action = ChainResumeAction::for_payout_pending(&record).unwrap();

        assert_eq!(action, ChainResumeAction::RebuildAndBroadcastPayout);
    }

    #[test]
    fn chain_watcher_refuses_claim_without_preimage() {
        let record = test_swap_record_without_preimage();

        let error = LoopOutClaimPlan::from_record(&record).unwrap_err();

        assert!(error.to_string().contains("preimage"));
    }

    #[test]
    fn chain_watcher_claim_plan_uses_non_default_preimage() {
        let preimage: Hash256 = [9u8; 32].into();
        let record = test_swap_record_with_preimage(preimage);

        let plan = LoopOutClaimPlan::from_record(&record).unwrap();

        assert_eq!(plan.swap_id, record.swap_id);
        assert_eq!(plan.payment_preimage, preimage);
    }

    #[test]
    fn chain_watcher_refuses_claim_with_mismatched_preimage() {
        let record = test_swap_record_with_mismatched_preimage([9u8; 32].into());

        let error = LoopOutClaimPlan::from_record(&record).unwrap_err();

        assert!(error.to_string().contains("payment hash"));
    }

    #[test]
    fn payout_plan_derives_outpoint_from_transaction_hash_and_output_index() {
        let quote = test_loop_out_quote_terms();
        let tx = test_transaction_with_liquidity_output(&quote, 1);

        let plan = LoopOutPayoutTxPlan::new(quote.quote_id, tx.clone(), 1);

        assert_eq!(plan.swap_id, quote.quote_id);
        assert_eq!(plan.tx_hash, tx.hash().into());
        assert_eq!(plan.outpoint.tx_hash(), tx.hash());
        assert_eq!(u32::from(plan.outpoint.index()), 1);
    }

    #[tokio::test]
    async fn ckb_watcher_sends_tx_then_registers_tracer() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let watcher = CkbLiquidityChainWatcher::new(ckb_actor, NoopLiquidityStore::default());
        let (liquidity_actor, _receiver) = spawn_mock_liquidity_actor().await;
        let transaction = TransactionView::new_advanced_builder().build();

        watcher
            .send_and_trace(
                [1u8; 32].into(),
                LiquidityChainTxRole::Payout,
                transaction,
                liquidity_actor,
            )
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if events.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mock ckb actor records tracer creation");

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                MockCkbEvent::SendTx,
                MockCkbEvent::CreateTxTracer(
                    CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                )
            ]
        );
    }

    #[tokio::test]
    async fn ckb_watcher_send_tx_timeout_returns_error_without_tracer() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, NoReplyCkbActor, events.clone())
            .await
            .unwrap();
        let watcher = CkbLiquidityChainWatcher::new(ckb_actor, NoopLiquidityStore::default());
        let (liquidity_actor, _receiver) = spawn_mock_liquidity_actor().await;
        let transaction = TransactionView::new_advanced_builder().build();

        let error = watcher
            .send_and_trace(
                [1u8; 32].into(),
                LiquidityChainTxRole::Payout,
                transaction,
                liquidity_actor,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("send tx actor call failed"));
        assert_eq!(*events.lock().unwrap(), vec![MockCkbEvent::SendTx]);
    }

    #[tokio::test]
    async fn ckb_watcher_send_tx_error_returns_error_without_tracer() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, SendErrorCkbActor, events.clone())
            .await
            .unwrap();
        let watcher = CkbLiquidityChainWatcher::new(ckb_actor, NoopLiquidityStore::default());
        let (liquidity_actor, _receiver) = spawn_mock_liquidity_actor().await;
        let transaction = TransactionView::new_advanced_builder().build();

        let error = watcher
            .send_and_trace(
                [1u8; 32].into(),
                LiquidityChainTxRole::Payout,
                transaction,
                liquidity_actor,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("send tx failed"));
        assert_eq!(*events.lock().unwrap(), vec![MockCkbEvent::SendTx]);
    }

    #[tokio::test]
    async fn ckb_watcher_without_liquidity_lock_artifact_fails_before_payout_persistence_or_send() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store.clone());
        let quote = test_loop_out_quote_terms();

        let error = watcher
            .reserve_payout_lock_outpoint(&quote)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("liquidity-lock script artifact"));
        assert!(store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .is_none());
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ckb_watcher_without_liquidity_lock_artifact_does_not_broadcast_payout() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store.clone());
        let quote = test_loop_out_quote_terms();
        let outpoint = test_outpoint(0);

        let error = watcher
            .broadcast_payout_lock(&quote, &outpoint, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("liquidity-lock script artifact"));
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ckb_watcher_reserves_payout_by_funding_and_signing_before_persisting_planned_tx() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_out_quote_terms();
        let funded_tx = test_funding_transaction_with_script(&quote, 0);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let expected_outpoint = packed::OutPoint::new(signed_tx.hash(), 1);
        let (ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events: events.clone(),
                funded_tx: funded_tx.clone(),
                signed_tx: signed_tx.clone(),
                send_error: false,
            },
        )
        .await
        .unwrap();
        let store = NoopLiquidityStore::default();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );

        let outpoint = watcher.reserve_payout_lock_outpoint(&quote).await.unwrap();
        wait_for_mock_events(&events, 4).await;

        assert_eq!(outpoint, expected_outpoint);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                MockCkbEvent::Fund {
                    script: liquidity_lock_script_for_quote(&quote),
                    local_amount: quote.amount,
                    local_reserved_ckb_amount: quote.capacity_requirement_ckb
                        - u64::try_from(quote.amount).unwrap(),
                },
                MockCkbEvent::Sign,
                MockCkbEvent::RemoveFundingTx(funded_tx.hash().into()),
                MockCkbEvent::AddFundingTx(signed_tx.hash().into()),
            ]
        );
        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .expect("payout tx record is persisted");
        assert_eq!(record.swap_id, quote.quote_id);
        assert_eq!(record.role, LiquidityChainTxRole::Payout);
        assert_eq!(record.tx_hash, signed_tx.hash().into());
        assert_eq!(record.outpoint, Some(expected_outpoint));
        assert_eq!(record.status, LiquidityChainTxStatus::Planned);
    }

    #[tokio::test]
    async fn ckb_watcher_broadcasts_reserved_payout_after_marking_record_broadcast() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_out_quote_terms();
        let funded_tx = test_funding_transaction_with_script(&quote, 0);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let expected_outpoint = packed::OutPoint::new(signed_tx.hash(), 1);
        let (ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events: events.clone(),
                funded_tx,
                signed_tx: signed_tx.clone(),
                send_error: false,
            },
        )
        .await
        .unwrap();
        let store = NoopLiquidityStore::default();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );
        let outpoint = watcher.reserve_payout_lock_outpoint(&quote).await.unwrap();
        assert_eq!(outpoint, expected_outpoint);
        wait_for_mock_events(&events, 4).await;
        events.lock().unwrap().clear();
        let (liquidity_actor, mut liquidity_messages) = spawn_mock_liquidity_actor().await;

        watcher
            .broadcast_payout_lock(&quote, &expected_outpoint, liquidity_actor)
            .await
            .unwrap();

        wait_for_mock_events(&events, 3).await;

        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .expect("payout tx record remains persisted");
        assert_eq!(record.status, LiquidityChainTxStatus::Broadcast);
        assert_eq!(record.outpoint, Some(expected_outpoint));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                MockCkbEvent::SendTx,
                MockCkbEvent::CreateTxTracer(
                    CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                ),
                MockCkbEvent::CommitFundingTx(signed_tx.hash().into()),
            ]
        );
        let message =
            tokio::time::timeout(std::time::Duration::from_secs(1), liquidity_messages.recv())
                .await
                .expect("payout tracer callback reaches liquidity actor")
                .expect("liquidity actor receives payout continuation");
        assert!(matches!(
            message,
            LiquidityActorMessage::PayoutConfirmed(swap_id) if swap_id == quote.quote_id
        ));
    }

    #[tokio::test]
    async fn ckb_watcher_payout_send_error_marks_rejected_but_keeps_retryable_transaction() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_out_quote_terms();
        let funded_tx = test_funding_transaction_with_script(&quote, 0);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let expected_outpoint = packed::OutPoint::new(signed_tx.hash(), 1);
        let (ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events: events.clone(),
                funded_tx,
                signed_tx,
                send_error: true,
            },
        )
        .await
        .unwrap();
        let store = NoopLiquidityStore::default();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );
        let outpoint = watcher.reserve_payout_lock_outpoint(&quote).await.unwrap();
        assert_eq!(outpoint, expected_outpoint);
        wait_for_mock_events(&events, 4).await;
        events.lock().unwrap().clear();

        let error = watcher
            .broadcast_payout_lock(
                &quote,
                &expected_outpoint,
                spawn_mock_liquidity_actor().await.0,
            )
            .await
            .expect_err("send failure should surface");
        assert!(error.to_string().contains("send tx failed"));
        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .expect("payout tx record remains persisted");
        assert_eq!(record.status, LiquidityChainTxStatus::Rejected);
        assert!(record
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("send tx failed"));

        let retry_error = watcher
            .broadcast_payout_lock(
                &quote,
                &expected_outpoint,
                spawn_mock_liquidity_actor().await.0,
            )
            .await
            .expect_err("mock still rejects retry");
        assert!(retry_error.to_string().contains("send tx failed"));
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, MockCkbEvent::SendTx))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn ckb_watcher_broadcast_claim_persists_tx_identity_before_send_tx() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let quote = test_loop_out_quote_terms();
        let preimage: Hash256 = [4u8; 32].into();
        let mut swap = test_swap_record_with_outpoint(test_outpoint(11));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.state = LiquiditySwapState::PaymentSettled;
        swap.payment_hash = quote.payment_hash;
        swap.payment_preimage = Some(preimage);
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(12))
            .dep_type(ckb_types::core::DepType::Code)
            .build();
        let store = NoopLiquidityStore::default();
        store.insert_loop_out_quote(quote.clone(), 1).unwrap();
        store.insert_liquidity_swap(swap).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
            ckb_actor,
            store.clone(),
            liquidity_lock_script_for_quote(&quote),
            vec![cell_dep],
        );

        watcher
            .broadcast_claim(
                LoopOutClaimRequest {
                    swap_id: quote.quote_id,
                    payment_preimage: preimage,
                },
                spawn_mock_liquidity_actor().await.0,
            )
            .await
            .unwrap();

        wait_for_mock_events(&events, 1).await;
        assert_eq!(*events.lock().unwrap(), vec![MockCkbEvent::SendTx]);
        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
            .unwrap()
            .expect("claim tx record is persisted");
        assert_eq!(record.role, LiquidityChainTxRole::Claim);
        assert_eq!(record.status, LiquidityChainTxStatus::Broadcast);
        assert!(record.outpoint.is_none());
    }

    #[tokio::test]
    async fn ckb_watcher_watch_claim_uses_existing_tx_record_without_send() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let swap_id: Hash256 = [13u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Claim,
                tx_hash: [14u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store);

        watcher
            .watch_claim(swap_id, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap();

        wait_for_mock_events(&events, 1).await;
        assert_eq!(
            *events.lock().unwrap(),
            vec![MockCkbEvent::CreateTxTracer(
                CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
            )]
        );
    }

    #[tokio::test]
    async fn ckb_watcher_claim_send_error_marks_rejected_and_allows_rebroadcast() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, SendErrorCkbActor, events.clone())
            .await
            .unwrap();
        let quote = test_loop_out_quote_terms();
        let preimage: Hash256 = [4u8; 32].into();
        let mut swap = test_swap_record_with_outpoint(test_outpoint(15));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.state = LiquiditySwapState::ClaimPending;
        swap.payment_hash = quote.payment_hash;
        swap.payment_preimage = Some(preimage);
        let store = NoopLiquidityStore::default();
        store.insert_loop_out_quote(quote.clone(), 1).unwrap();
        store.insert_liquidity_swap(swap).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
            ckb_actor,
            store.clone(),
            liquidity_lock_script_for_quote(&quote),
            vec![],
        );
        let request = LoopOutClaimRequest {
            swap_id: quote.quote_id,
            payment_preimage: preimage,
        };

        let error = watcher
            .broadcast_claim(request, spawn_mock_liquidity_actor().await.0)
            .await
            .expect_err("mock send failure should surface");
        assert!(error.to_string().contains("send tx failed"));
        let rejected = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
            .unwrap()
            .expect("claim tx record remains persisted");
        assert_eq!(rejected.status, LiquidityChainTxStatus::Rejected);

        let retry_error = watcher
            .broadcast_claim(request, spawn_mock_liquidity_actor().await.0)
            .await
            .expect_err("mock send failure should surface again");
        assert!(retry_error.to_string().contains("send tx failed"));
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, MockCkbEvent::SendTx))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn ckb_watcher_claim_send_timeout_marks_rejected_for_recovery_rebroadcast() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, NoReplyCkbActor, events.clone())
            .await
            .unwrap();
        let quote = test_loop_out_quote_terms();
        let preimage: Hash256 = [4u8; 32].into();
        let mut swap = test_swap_record_with_outpoint(test_outpoint(16));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.state = LiquiditySwapState::ClaimPending;
        swap.payment_hash = quote.payment_hash;
        swap.payment_preimage = Some(preimage);
        let store = NoopLiquidityStore::default();
        store.insert_loop_out_quote(quote.clone(), 1).unwrap();
        store.insert_liquidity_swap(swap).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
            ckb_actor,
            store.clone(),
            liquidity_lock_script_for_quote(&quote),
            vec![],
        );

        let error = watcher
            .broadcast_claim(
                LoopOutClaimRequest {
                    swap_id: quote.quote_id,
                    payment_preimage: preimage,
                },
                spawn_mock_liquidity_actor().await.0,
            )
            .await
            .expect_err("send timeout should surface");

        assert!(error.to_string().contains("send tx actor call failed"));
        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
            .unwrap()
            .expect("claim tx record remains persisted");
        assert_eq!(record.status, LiquidityChainTxStatus::Rejected);
        assert!(record
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("send tx actor call failed"));
    }

    #[tokio::test]
    async fn ckb_watcher_broadcast_refund_persists_tx_identity_before_send_tx() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let quote = test_loop_out_quote_terms();
        let mut swap = test_swap_record_with_outpoint(test_outpoint(22));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.role = LiquiditySwapRole::Provider;
        swap.state = LiquiditySwapState::RefundPending;
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(23))
            .dep_type(ckb_types::core::DepType::Code)
            .build();
        let store = NoopLiquidityStore::default();
        store.insert_loop_out_quote(quote.clone(), 1).unwrap();
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
            ckb_actor,
            store.clone(),
            liquidity_lock_script_for_quote(&quote),
            vec![cell_dep],
        );

        watcher.broadcast_refund(&swap).await.unwrap();

        wait_for_mock_events(&events, 1).await;
        assert_eq!(*events.lock().unwrap(), vec![MockCkbEvent::SendTx]);
        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Refund)
            .unwrap()
            .expect("refund tx record is persisted");
        assert_eq!(record.role, LiquidityChainTxRole::Refund);
        assert_eq!(record.status, LiquidityChainTxStatus::Broadcast);
        assert!(record.outpoint.is_none());
    }

    #[tokio::test]
    async fn ckb_watcher_refund_send_error_marks_rejected_and_allows_rebroadcast() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, SendErrorCkbActor, events.clone())
            .await
            .unwrap();
        let quote = test_loop_out_quote_terms();
        let mut swap = test_swap_record_with_outpoint(test_outpoint(26));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.role = LiquiditySwapRole::Provider;
        swap.state = LiquiditySwapState::RefundPending;
        let store = NoopLiquidityStore::default();
        store.insert_loop_out_quote(quote.clone(), 1).unwrap();
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
            ckb_actor,
            store.clone(),
            liquidity_lock_script_for_quote(&quote),
            vec![],
        );

        let error = watcher
            .broadcast_refund(&swap)
            .await
            .expect_err("mock send failure should surface");
        assert!(error.to_string().contains("send tx failed"));
        let rejected = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Refund)
            .unwrap()
            .expect("refund tx record remains persisted");
        assert_eq!(rejected.status, LiquidityChainTxStatus::Rejected);

        let retry_error = watcher
            .broadcast_refund(&swap)
            .await
            .expect_err("mock send failure should surface again");
        assert!(retry_error.to_string().contains("send tx failed"));
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, MockCkbEvent::SendTx))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn ckb_watcher_watch_refund_uses_existing_tx_record_without_send() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let swap_id: Hash256 = [24u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Refund,
                tx_hash: [25u8; 32].into(),
                outpoint: None,
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store);

        watcher
            .watch_refund(swap_id, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap();

        wait_for_mock_events(&events, 1).await;
        assert_eq!(
            *events.lock().unwrap(),
            vec![MockCkbEvent::CreateTxTracer(
                CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
            )]
        );
    }

    #[tokio::test]
    async fn ckb_watcher_refuses_to_watch_rejected_payout_record() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let swap_id: Hash256 = [7u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [8u8; 32].into(),
                outpoint: Some(test_outpoint(0)),
                status: LiquidityChainTxStatus::Rejected,
                failure_reason: Some("send tx failed".to_string()),
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store);

        let error = watcher
            .watch_payout_lock(swap_id, spawn_mock_liquidity_actor().await.0)
            .await
            .expect_err("rejected payout tx is not watchable");

        assert!(error.to_string().contains("non-broadcast status"));
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ckb_watcher_watches_broadcast_payout_record() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_out_quote_terms();
        let funded_tx = test_funding_transaction_with_script(&quote, 0);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let (ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events: events.clone(),
                funded_tx,
                signed_tx: signed_tx.clone(),
                send_error: false,
            },
        )
        .await
        .unwrap();
        let store = NoopLiquidityStore::default();
        let swap_id: Hash256 = [9u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: signed_tx.hash().into(),
                outpoint: Some(test_outpoint(0)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store);

        watcher
            .watch_payout_lock(swap_id, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap();

        wait_for_mock_events(&events, 2).await;
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                MockCkbEvent::CreateTxTracer(
                    CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                ),
                MockCkbEvent::CommitFundingTx(signed_tx.hash().into()),
            ]
        );
    }

    #[tokio::test]
    async fn ckb_watcher_maps_payout_confirmation_to_actor_message() {
        assert_callback_maps_committed_status(
            LiquidityChainTxRole::Payout,
            LiquidityActorMessage::PayoutConfirmed,
        )
        .await;
    }

    #[tokio::test]
    async fn ckb_watcher_maps_claim_confirmation_to_actor_message() {
        assert_callback_maps_committed_status(
            LiquidityChainTxRole::Claim,
            LiquidityActorMessage::ClaimConfirmed,
        )
        .await;
    }

    #[tokio::test]
    async fn ckb_watcher_maps_refund_confirmation_to_actor_message() {
        assert_callback_maps_committed_status(
            LiquidityChainTxRole::Refund,
            LiquidityActorMessage::RefundConfirmed,
        )
        .await;
    }

    #[test]
    fn claim_plan_requires_payout_outpoint_and_valid_preimage() {
        let mut record = test_swap_record_with_outpoint(test_outpoint(7));
        let preimage = [9u8; 32].into();
        record.payment_preimage = Some(preimage);
        record.payment_hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();

        let plan = LoopOutClaimTxPlan::from_record(&record).unwrap();

        assert_eq!(plan.swap_id, record.swap_id);
        assert_eq!(plan.payout_outpoint, record.onchain_outpoint.unwrap());
    }

    #[test]
    fn claim_plan_without_outpoint_fails() {
        let preimage = [9u8; 32].into();
        let record = test_swap_record_with_preimage(preimage);

        let error = LoopOutClaimTxPlan::from_record(&record).unwrap_err();

        assert!(error.to_string().contains("outpoint"));
    }

    #[test]
    fn refund_plan_requires_provider_record_and_payout_outpoint() {
        let record = test_provider_refund_pending_record(test_outpoint(8));

        let plan = LoopOutRefundTxPlan::from_record(&record).unwrap();

        assert_eq!(plan.swap_id, record.swap_id);
        assert_eq!(plan.payout_outpoint, record.onchain_outpoint.unwrap());
    }

    #[test]
    fn refund_plan_requires_refund_pending_state() {
        let mut record = test_provider_refund_pending_record(test_outpoint(8));
        record.state = LiquiditySwapState::PayoutLocked;

        let error = LoopOutRefundTxPlan::from_record(&record).unwrap_err();

        assert!(error.to_string().contains("refund pending"));
    }

    #[test]
    fn refund_plan_for_client_role_fails() {
        let record = LiquiditySwapRecord {
            role: LiquiditySwapRole::Client,
            ..test_provider_refund_pending_record(test_outpoint(8))
        };

        let error = LoopOutRefundTxPlan::from_record(&record).unwrap_err();

        assert!(error.to_string().contains("provider"));
    }
}
