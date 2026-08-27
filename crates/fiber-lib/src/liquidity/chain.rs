//! Chain adapter boundary for loop-out liquidity operations.

use std::collections::HashMap;

use async_trait::async_trait;
use ckb_hash::blake2b_256;
use ckb_types::{
    bytes::Bytes,
    core::tx_pool::TxStatus,
    core::TransactionView,
    packed,
    prelude::{Builder, Entity, IntoTransactionView, Pack},
};
use fiber_types::{
    Hash256, HashAlgorithm, LiquidityAssetKind, LiquidityChainTxRole, LiquiditySwapState,
};
use ractor::{ActorRef, RpcReplyPort};

use crate::ckb::contracts::get_udt_cell_deps;
use crate::ckb::{
    CkbChainMessage, CkbOutPointSpendTracer, CkbOutPointSpendTracingResult, CkbTxTracer,
    CkbTxTracingMask, CkbTxTracingResult, FundingRequest, FundingTx, LiveCell,
};
use crate::liquidity::actor::LiquidityActorMessage;
use crate::liquidity::quote::loop_in_gross_onchain_amount;
use crate::liquidity::store::{
    LiquidityStore, LiquiditySwapKind, LiquiditySwapRecord, LiquiditySwapRole,
};
use crate::liquidity::tx::{
    build_liquidity_lock_claim_witness, build_liquidity_lock_output,
    build_liquidity_lock_refund_witness, build_liquidity_lock_script,
    parse_liquidity_lock_claim_witness, LiquidityLockBuildError, LiquidityLockOutputParams,
    LiquidityLockScriptArtifact,
};
use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};
use crate::now_timestamp_as_millis_u64;

#[cfg(not(test))]
const CKB_SEND_TX_TIMEOUT_MS: u64 = 8000;
#[cfg(test)]
const CKB_SEND_TX_TIMEOUT_MS: u64 = 50;
const DEFAULT_LIQUIDITY_PAYOUT_FEE_RATE: u64 = 1000;
const LOOP_IN_LOCAL_FUNDING_TX_DESCRIPTOR: &str = "local-wallet";

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

/// Pure transaction plan for a provider claim of a Loop In client lock.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopInProviderClaimTxPlan {
    /// Local swap identifier being claimed.
    pub swap_id: Hash256,
    /// Persisted client lock outpoint to spend.
    pub client_lock_outpoint: packed::OutPoint,
    /// Validated payment preimage required by the claim path.
    pub payment_preimage: Hash256,
}

impl LoopInProviderClaimTxPlan {
    /// Build a Loop In provider claim transaction plan from a persisted swap record.
    pub fn from_record(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        if record.swap_kind != LiquiditySwapKind::LoopIn
            || record.role != LiquiditySwapRole::Provider
        {
            return Err(LiquidityLoopOutError::Chain(
                "cannot build loop in provider claim for non-provider loop in record".to_string(),
            ));
        }
        let claim = LoopOutClaimPlan::from_record(record)?;
        let client_lock_outpoint = record.onchain_outpoint.clone().ok_or_else(|| {
            LiquidityLoopOutError::Chain(
                "cannot build loop in provider claim without client lock outpoint".to_string(),
            )
        })?;

        Ok(Self {
            swap_id: claim.swap_id,
            client_lock_outpoint,
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
    /// Build a refund transaction plan from a persisted refund-pending swap record.
    pub fn from_record(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        match (record.swap_kind, record.role) {
            (LiquiditySwapKind::LoopOut, LiquiditySwapRole::Provider)
            | (LiquiditySwapKind::LoopIn, LiquiditySwapRole::Client) => {}
            (LiquiditySwapKind::LoopIn, LiquiditySwapRole::Provider) => {
                return Err(LiquidityLoopOutError::Chain(
                    "cannot build refund for loop in provider record".to_string(),
                ));
            }
            (LiquiditySwapKind::LoopOut, LiquiditySwapRole::Client) => {
                return Err(LiquidityLoopOutError::Chain(
                    "cannot build refund for non-provider loop out record".to_string(),
                ));
            }
        }
        if record.state != LiquiditySwapState::RefundPending {
            return Err(LiquidityLoopOutError::Chain(
                "cannot build refund unless swap is refund pending".to_string(),
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
        funding_tx: &str,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Validate Loop In lock broadcast availability before durable swap creation.
    fn ensure_loop_in_lock_available(&mut self, funding_tx: &str) -> Result<(), Self::Error>;

    /// Validate that an observed Loop In lock cell matches the accepted quote.
    async fn validate_observed_loop_in_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
    ) -> Result<(), Self::Error>;

    /// Validate that an observed Loop Out payout cell matches the accepted quote.
    async fn validate_observed_loop_out_payout(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
    ) -> Result<(), Self::Error>;

    /// Schedule payout lock watching and report completion back to `myself`.
    async fn watch_payout_lock(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Schedule Loop In client lock watching and report completion back to `myself`.
    async fn watch_loop_in_lock(
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

    /// Watch the exact provider Loop Out payout for a committed valid client claim.
    async fn watch_provider_claim(
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

fn validate_liquidity_lock_args(
    args: &[u8],
    quote: &LoopOutQuoteTerms,
    expected_onchain_amount: u128,
    liquidity_lock_code_hash: &packed::Byte32,
    liquidity_lock_hash_type: u8,
    cell_code_hash: &packed::Byte32,
    cell_hash_type: u8,
) -> Result<(), LiquidityLoopOutError> {
    if cell_code_hash != liquidity_lock_code_hash || cell_hash_type != liquidity_lock_hash_type {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock script does not match liquidity-lock contract".to_string(),
        ));
    }
    if args.len() != 152 {
        return Err(LiquidityLoopOutError::Chain(format!(
            "observed lock args length {} does not match expected 152",
            args.len()
        )));
    }
    let expected_payment_hash: [u8; 32] = args[0..32].try_into().unwrap();
    if expected_payment_hash.as_slice() != quote.payment_hash.as_ref() {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock payment_hash mismatch".to_string(),
        ));
    }
    let expected_claimant_hash = blake2b_256(quote.claimant_lock.as_slice());
    if args[32..64] != expected_claimant_hash {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock claimant_lock_hash mismatch".to_string(),
        ));
    }
    let expected_refund_hash = blake2b_256(quote.refund_lock.as_slice());
    if args[64..96] != expected_refund_hash {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock refund_lock_hash mismatch".to_string(),
        ));
    }
    let refund_after: u64 = u64::from_le_bytes(args[96..104].try_into().unwrap());
    if refund_after != quote.refund_after_lock_time {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock refund_after_lock_time mismatch".to_string(),
        ));
    }
    let onchain_amount: u128 = u128::from_le_bytes(args[104..120].try_into().unwrap());
    if onchain_amount != expected_onchain_amount {
        return Err(LiquidityLoopOutError::Chain(format!(
            "observed lock amount mismatch: expected {expected_onchain_amount}, got {onchain_amount}"
        )));
    }
    let asset_type_hash = &args[120..152];
    match quote.asset.kind {
        LiquidityAssetKind::Ckb => {
            if asset_type_hash != [0u8; 32] {
                return Err(LiquidityLoopOutError::Chain(
                    "observed lock asset_type_hash mismatch for CKB asset".to_string(),
                ));
            }
        }
        LiquidityAssetKind::Udt => {
            let expected_udt_hash = if let Some(ref udt) = quote.asset.udt_type_script {
                let udt_script: packed::Script = udt.clone().into();
                blake2b_256(udt_script.as_slice())
            } else {
                return Err(LiquidityLoopOutError::Chain(
                    "UDT asset missing udt_type_script".to_string(),
                ));
            };
            if asset_type_hash != expected_udt_hash.as_slice() {
                return Err(LiquidityLoopOutError::Chain(
                    "observed lock asset_type_hash mismatch for UDT asset".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_liquidity_live_cell(
    cell: &LiveCell,
    quote: &LoopOutQuoteTerms,
    expected_onchain_amount: u128,
    artifact: &LiquidityLockScriptArtifact,
    context: &str,
) -> Result<(), LiquidityLoopOutError> {
    let lock_script = cell.output.lock();
    validate_liquidity_lock_args(
        &lock_script.args().raw_data(),
        quote,
        expected_onchain_amount,
        &artifact.code_hash,
        artifact.hash_type.into(),
        &lock_script.code_hash(),
        lock_script.hash_type().into(),
    )?;

    if quote.asset.kind == LiquidityAssetKind::Udt {
        let cell_type_script = cell.output.type_().to_opt();
        let expected_type: Option<packed::Script> =
            quote.asset.udt_type_script.clone().map(Into::into);
        if cell_type_script != expected_type {
            return Err(LiquidityLoopOutError::Chain(format!(
                "{context} UDT type script mismatch"
            )));
        }

        let cell_data = cell.data.raw_data();
        if cell_data.len() != 16 {
            return Err(LiquidityLoopOutError::Chain(format!(
                "{context} UDT data length {} does not match expected 16",
                cell_data.len()
            )));
        }
        let udt_amount = u128::from_le_bytes(cell_data[..16].try_into().unwrap());
        if udt_amount != expected_onchain_amount {
            return Err(LiquidityLoopOutError::Chain(format!(
                "{context} UDT amount mismatch: expected {expected_onchain_amount}, got {udt_amount}"
            )));
        }
    } else if cell.output.type_().to_opt().is_some() {
        return Err(LiquidityLoopOutError::Chain(format!(
            "{context} CKB cell has unexpected type script"
        )));
    } else {
        let expected_ckb = u64::try_from(expected_onchain_amount).map_err(|_| {
            LiquidityLoopOutError::Chain(format!("{context} CKB amount does not fit u64"))
        })?;
        if u64::from(cell.output.capacity()) < expected_ckb {
            return Err(LiquidityLoopOutError::Chain(format!(
                "{context} CKB capacity {} below amount {expected_ckb}",
                u64::from(cell.output.capacity())
            )));
        }
    }

    if u64::from(cell.output.capacity()) < quote.capacity_requirement_ckb {
        return Err(LiquidityLoopOutError::Chain(format!(
            "{context} capacity {} below requirement {}",
            u64::from(cell.output.capacity()),
            quote.capacity_requirement_ckb
        )));
    }

    Ok(())
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

    async fn validate_observed_liquidity_cell(
        &self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
        expected_onchain_amount: u128,
        context: &str,
    ) -> Result<(), LiquidityLoopOutError> {
        let artifact = self.liquidity_lock_artifact.as_ref().ok_or_else(|| {
            LiquidityLoopOutError::Chain(
                "liquidity-lock script artifact is not configured".to_string(),
            )
        })?;
        let cell = ractor::call!(self.ckb_chain_actor, |reply| {
            CkbChainMessage::GetLiveCell(outpoint.clone(), reply)
        })
        .map_err(|error| {
            LiquidityLoopOutError::Chain(format!(
                "failed to query live cell for {context}: {error}"
            ))
        })?
        .map_err(|error| {
            LiquidityLoopOutError::Chain(format!(
                "ckb rpc error querying live cell for {context}: {error}"
            ))
        })?
        .ok_or_else(|| {
            LiquidityLoopOutError::Chain(format!("{context} cell not found or already spent"))
        })?;

        validate_liquidity_live_cell(&cell, quote, expected_onchain_amount, artifact, context)
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
            match result.tx_status {
                TxStatus::Committed(block_number, ..) => {
                    let _ = ckb_chain_actor
                        .send_message(CkbChainMessage::CommitFundingTx(tx_hash, block_number));
                    let _ = liquidity_actor
                        .send_message(LiquidityActorMessage::PayoutConfirmed(swap_id));
                }
                TxStatus::Rejected(reason) => {
                    let _ = liquidity_actor.send_message(LiquidityActorMessage::ChainTxRejected(
                        swap_id,
                        LiquidityChainTxRole::Payout,
                        reason,
                    ));
                }
                _ => {}
            }
        });
        RpcReplyPort::from(sender)
    }

    fn provider_claim_tracer_callback_for(
        swap_id: Hash256,
        watched_outpoint: packed::OutPoint,
        payment_hash: Hash256,
        liquidity_actor: ActorRef<LiquidityActorMessage>,
    ) -> RpcReplyPort<Result<CkbOutPointSpendTracingResult, String>> {
        let (sender, receiver) =
            tokio::sync::oneshot::channel::<Result<CkbOutPointSpendTracingResult, String>>();
        tokio::spawn(async move {
            let Ok(result) = receiver.await else {
                tracing::warn!(?swap_id, "provider claim tracer callback was dropped");
                return;
            };
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(?swap_id, %error, "provider claim tracing failed");
                    return;
                }
            };
            if result.outpoint != watched_outpoint {
                tracing::warn!(
                    ?swap_id,
                    "provider claim tracer returned a different outpoint"
                );
                return;
            }
            let Some(input) = result.spending_transaction.inputs().get(result.input_index) else {
                tracing::warn!(
                    ?swap_id,
                    input_index = result.input_index,
                    "provider claim tracer returned an invalid input index"
                );
                return;
            };
            if input.previous_output() != watched_outpoint {
                tracing::warn!(
                    ?swap_id,
                    input_index = result.input_index,
                    "provider claim input does not spend the watched outpoint"
                );
                return;
            }
            if result.script_group_input_index > result.input_index {
                tracing::warn!(
                    ?swap_id,
                    input_index = result.input_index,
                    script_group_input_index = result.script_group_input_index,
                    "provider claim script-group input follows watched input"
                );
                return;
            }
            if result
                .spending_transaction
                .inputs()
                .get(result.script_group_input_index)
                .is_none()
            {
                tracing::warn!(
                    ?swap_id,
                    script_group_input_index = result.script_group_input_index,
                    "provider claim script-group input index is invalid"
                );
                return;
            }
            let Some(witness) = result
                .spending_transaction
                .witnesses()
                .get(result.script_group_input_index)
            else {
                tracing::warn!(
                    ?swap_id,
                    script_group_input_index = result.script_group_input_index,
                    "provider claim transaction is missing its indexed witness"
                );
                return;
            };
            let preimage = match parse_liquidity_lock_claim_witness(&witness) {
                Ok(preimage) => preimage,
                Err(error) => {
                    tracing::warn!(?swap_id, %error, "provider claim witness is invalid");
                    return;
                }
            };
            let observed_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            if observed_hash != payment_hash {
                tracing::warn!(
                    ?swap_id,
                    "provider claim preimage does not match the payment hash"
                );
                return;
            }
            if let Err(error) =
                liquidity_actor.send_message(LiquidityActorMessage::ProviderClaimObserved(swap_id))
            {
                tracing::warn!(?swap_id, %error, "failed to deliver provider claim observation");
            }
        });
        RpcReplyPort::from(sender)
    }

    fn loop_in_lock_tracer_callback_for(
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
            match result.tx_status {
                TxStatus::Committed(block_number, ..) => {
                    let _ = ckb_chain_actor
                        .send_message(CkbChainMessage::CommitFundingTx(tx_hash, block_number));
                    let _ = liquidity_actor
                        .send_message(LiquidityActorMessage::LoopInLockConfirmed(swap_id));
                }
                TxStatus::Rejected(reason) => {
                    let _ = liquidity_actor.send_message(LiquidityActorMessage::ChainTxRejected(
                        swap_id,
                        LiquidityChainTxRole::Payout,
                        reason,
                    ));
                }
                _ => {}
            }
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

    fn build_loop_in_lock_funding_request(
        &self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<FundingRequest, LiquidityLoopOutError> {
        let artifact = self
            .liquidity_lock_artifact
            .as_ref()
            .ok_or_else(Self::missing_payout_builder)?;
        let (output, _) = build_loop_in_client_lock_output(artifact, quote)?;
        let gross_amount = loop_in_gross_onchain_amount(quote)?;
        let local_reserved_ckb_amount = if quote.asset.udt_type_script.is_some() {
            quote.capacity_requirement_ckb
        } else {
            let amount = u64::try_from(gross_amount).map_err(|_| {
                LiquidityLoopOutError::Chain(
                    "cannot build loop in lock transaction: CKB amount overflows u64".to_string(),
                )
            })?;
            quote
                .capacity_requirement_ckb
                .checked_sub(amount)
                .ok_or_else(|| {
                    LiquidityLoopOutError::Chain(
                    "cannot build loop in lock transaction: capacity requirement below CKB amount"
                        .to_string(),
                )
                })?
        };

        Ok(FundingRequest {
            script: output.lock(),
            udt_type_script: quote.asset.udt_type_script.clone().map(Into::into),
            local_amount: gross_amount,
            funding_fee_rate: DEFAULT_LIQUIDITY_PAYOUT_FEE_RATE,
            remote_amount: 0,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount: 0,
        })
    }

    async fn fund_and_sign_lock_tx(
        &self,
        request: FundingRequest,
        error_context: &str,
    ) -> Result<(TransactionView, packed::OutPoint, Option<Hash256>), LiquidityLoopOutError> {
        let liquidity_lock_script = request.script.clone();
        let funded_tx = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::Fund,
            CKB_SEND_TX_TIMEOUT_MS,
            FundingTx::new(),
            request
        )
        .map_err(|error| LiquidityLoopOutError::Chain(format!("fund actor call failed: {error}")))?
        .map_err(|error| {
            LiquidityLoopOutError::Chain(format!("fund {error_context} failed: {error}"))
        })?;
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
            LiquidityLoopOutError::Chain(format!("sign {error_context} failed: {error}"))
        })?;
        let tx = signed_tx.as_ref().cloned().ok_or_else(|| {
            LiquidityLoopOutError::Chain(format!("signed {error_context} funding tx is empty"))
        })?;
        let outpoint = Self::payout_outpoint_for_signed_tx(&tx, &liquidity_lock_script)?;

        if let Some(funded_tx_hash) = funded_tx_hash {
            if funded_tx_hash != tx.hash().into() {
                self.ckb_chain_actor
                    .send_message(CkbChainMessage::RemoveFundingTx(funded_tx_hash))
                    .map_err(|error| {
                        LiquidityLoopOutError::Chain(format!(
                            "remove unsigned {error_context} funding tx reservation failed: {error}"
                        ))
                    })?;
                self.ckb_chain_actor
                    .send_message(CkbChainMessage::AddFundingTx(signed_tx))
                    .map_err(|error| {
                        LiquidityLoopOutError::Chain(format!(
                            "add signed {error_context} funding tx reservation failed: {error}"
                        ))
                    })?;
            }
        }

        Ok((tx, outpoint, funded_tx_hash))
    }

    async fn send_loop_in_lock_tx(
        &mut self,
        quote: &LoopOutQuoteTerms,
        funding_tx: &str,
        tx: TransactionView,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), LiquidityLoopOutError> {
        let tx_hash: Hash256 = tx.hash().into();

        let send_result = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::SendTx,
            CKB_SEND_TX_TIMEOUT_MS,
            tx.clone()
        )
        .map_err(|error| {
            let failure_reason = format!(
                "send tx actor call failed for loop in lock liquidity tx {tx_hash} from funding_tx {funding_tx}: {error}"
            );
            let _ = self.store.update_liquidity_chain_tx_status(
                &quote.quote_id,
                LiquidityChainTxRole::Payout,
                fiber_types::LiquidityChainTxStatus::Rejected,
                Some(failure_reason.clone()),
                now_timestamp_as_millis_u64(),
            );
            LiquidityLoopOutError::Chain(failure_reason)
        })?;
        if let Err(error) = send_result {
            let failure_reason = format!(
                "send tx failed for loop in lock liquidity tx {tx_hash} from funding_tx {funding_tx}: {error}"
            );
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

        self.store
            .update_liquidity_chain_tx_status(
                &quote.quote_id,
                LiquidityChainTxRole::Payout,
                fiber_types::LiquidityChainTxStatus::Broadcast,
                None,
                now_timestamp_as_millis_u64(),
            )
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::loop_in_lock_tracer_callback_for(
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
}

impl<S> CkbLiquidityChainWatcher<S>
where
    S: LiquidityStore + Send + Sync,
{
    fn persist_signed_tx(
        &self,
        swap_id: &Hash256,
        role: LiquidityChainTxRole,
        tx: &TransactionView,
    ) -> Result<(), LiquidityLoopOutError> {
        self.store
            .insert_liquidity_chain_tx_signed_tx(swap_id, role, tx.data())
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))
    }

    fn reload_signed_tx(
        &self,
        swap_id: &Hash256,
        role: LiquidityChainTxRole,
        expected_tx_hash: &Hash256,
        context: &str,
    ) -> Result<TransactionView, LiquidityLoopOutError> {
        if let Some(tx) = self.pending_payout_txs.get(swap_id).cloned() {
            let tx_hash: Hash256 = tx.hash().into();
            if tx_hash != *expected_tx_hash {
                return Err(LiquidityLoopOutError::Chain(format!(
                    "{context}: pending signed transaction hash does not match persisted record"
                )));
            }
            return Ok(tx);
        }
        let packed_tx = self
            .store
            .get_liquidity_chain_tx_signed_tx(swap_id, role)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Chain(format!(
                    "{context}: missing persisted signed transaction"
                ))
            })?;
        let tx: TransactionView = packed_tx.into_view();
        let tx_hash: Hash256 = tx.hash().into();
        if tx_hash != *expected_tx_hash {
            return Err(LiquidityLoopOutError::Chain(format!(
                "{context}: persisted signed transaction hash does not match persisted record"
            )));
        }
        Ok(tx)
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
        self.persist_signed_tx(&quote.quote_id, LiquidityChainTxRole::Payout, &tx)?;
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
        let tx = self.reload_signed_tx(
            &quote.quote_id,
            LiquidityChainTxRole::Payout,
            &record.tx_hash,
            "cannot broadcast payout",
        )?;
        let tx_hash: Hash256 = tx.hash().into();

        let send_result = ractor::call_t!(
            self.ckb_chain_actor,
            CkbChainMessage::SendTx,
            CKB_SEND_TX_TIMEOUT_MS,
            tx.clone()
        )
        .map_err(|error| {
            let failure_reason = format!("send tx actor call failed: {error}");
            let _ = self.store.update_liquidity_chain_tx_status(
                &quote.quote_id,
                LiquidityChainTxRole::Payout,
                fiber_types::LiquidityChainTxStatus::Rejected,
                Some(failure_reason.clone()),
                now_timestamp_as_millis_u64(),
            );
            LiquidityLoopOutError::Chain(failure_reason)
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

        self.store
            .update_liquidity_chain_tx_status(
                &quote.quote_id,
                LiquidityChainTxRole::Payout,
                fiber_types::LiquidityChainTxStatus::Broadcast,
                None,
                now_timestamp_as_millis_u64(),
            )
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;

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
        let watchable = matches!(
            record.status,
            fiber_types::LiquidityChainTxStatus::Broadcast
                | fiber_types::LiquidityChainTxStatus::Confirmed
        ) || (record.status == fiber_types::LiquidityChainTxStatus::Planned
            && record.outpoint.is_some());
        if !watchable {
            return Err(LiquidityLoopOutError::Chain(format!(
                "cannot watch payout transaction with non-watchable status {:?}",
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
        quote: &LoopOutQuoteTerms,
        funding_tx: &str,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        self.ensure_loop_in_lock_available(funding_tx)?;
        if let Some(record) = self
            .store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
        {
            match record.status {
                fiber_types::LiquidityChainTxStatus::Broadcast
                | fiber_types::LiquidityChainTxStatus::Confirmed => {
                    return self.watch_loop_in_lock(quote.quote_id, myself).await;
                }
                fiber_types::LiquidityChainTxStatus::Planned => {
                    let tx = self.reload_signed_tx(
                        &quote.quote_id,
                        LiquidityChainTxRole::Payout,
                        &record.tx_hash,
                        "cannot retry loop in lock",
                    )?;
                    return self
                        .send_loop_in_lock_tx(quote, funding_tx, tx, myself)
                        .await;
                }
                fiber_types::LiquidityChainTxStatus::Rejected => {
                    let tx = self.reload_signed_tx(
                        &quote.quote_id,
                        LiquidityChainTxRole::Payout,
                        &record.tx_hash,
                        "cannot retry rejected loop in lock",
                    )?;
                    self.store
                        .update_liquidity_chain_tx_status(
                            &quote.quote_id,
                            LiquidityChainTxRole::Payout,
                            fiber_types::LiquidityChainTxStatus::Planned,
                            None,
                            now_timestamp_as_millis_u64(),
                        )
                        .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
                    return self
                        .send_loop_in_lock_tx(quote, funding_tx, tx, myself)
                        .await;
                }
            }
        }

        let tx = if let Some(tx) = self.pending_payout_txs.get(&quote.quote_id).cloned() {
            tx
        } else {
            let request = self.build_loop_in_lock_funding_request(quote)?;
            let (tx, outpoint, _) = self
                .fund_and_sign_lock_tx(request, "loop in lock transaction")
                .await?;
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
            self.store
                .update_liquidity_swap(
                    &quote.quote_id,
                    crate::liquidity::store::LiquiditySwapUpdate {
                        onchain_outpoint: Some(outpoint),
                        updated_at: now,
                        ..Default::default()
                    },
                )
                .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
            self.persist_signed_tx(&quote.quote_id, LiquidityChainTxRole::Payout, &tx)?;
            self.pending_payout_txs.insert(quote.quote_id, tx.clone());
            tx
        };
        self.send_loop_in_lock_tx(quote, funding_tx, tx, myself)
            .await
    }

    fn ensure_loop_in_lock_available(&mut self, funding_tx: &str) -> Result<(), Self::Error> {
        self.liquidity_lock_artifact
            .as_ref()
            .ok_or_else(Self::missing_payout_builder)?;
        if funding_tx.trim().is_empty() {
            return Err(LiquidityLoopOutError::Chain(
                "loop in funding_tx must not be empty".to_string(),
            ));
        }
        if funding_tx != LOOP_IN_LOCAL_FUNDING_TX_DESCRIPTOR {
            return Err(LiquidityLoopOutError::Chain(format!(
                "unsupported loop in funding_tx: only {LOOP_IN_LOCAL_FUNDING_TX_DESCRIPTOR} is supported"
            )));
        }
        Ok(())
    }

    async fn validate_observed_loop_in_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
    ) -> Result<(), Self::Error> {
        let gross_amount = loop_in_gross_onchain_amount(quote)?;
        self.validate_observed_liquidity_cell(
            quote,
            outpoint,
            gross_amount,
            "observed loop in lock",
        )
        .await
    }

    async fn validate_observed_loop_out_payout(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
    ) -> Result<(), Self::Error> {
        self.validate_observed_liquidity_cell(
            quote,
            outpoint,
            quote.amount,
            "observed loop out payout",
        )
        .await
    }

    async fn watch_loop_in_lock(
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
                    "cannot watch loop in lock without persisted transaction identity".to_string(),
                )
            })?;
        if !matches!(
            record.status,
            fiber_types::LiquidityChainTxStatus::Planned
                | fiber_types::LiquidityChainTxStatus::Broadcast
                | fiber_types::LiquidityChainTxStatus::Confirmed
        ) {
            return Err(LiquidityLoopOutError::Chain(format!(
                "cannot watch loop in lock transaction with non-watchable status {:?}",
                record.status
            )));
        }
        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash: record.tx_hash,
                confirmations: 1,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: Self::loop_in_lock_tracer_callback_for(
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
        let payment_preimage = match (swap.swap_kind, swap.role) {
            (LiquiditySwapKind::LoopIn, LiquiditySwapRole::Provider) => {
                LoopInProviderClaimTxPlan::from_record(&swap)?.payment_preimage
            }
            _ => LoopOutClaimTxPlan::from_record(&swap)?.payment_preimage,
        };
        if payment_preimage != request.payment_preimage {
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
            claim_cell_deps.extend(udt_cell_deps);
        }
        let tx = match (swap.swap_kind, swap.role) {
            (LiquiditySwapKind::LoopIn, LiquiditySwapRole::Provider) => {
                let plan = LoopInProviderClaimTxPlan::from_record(&swap)?;
                build_loop_in_provider_claim_transaction(
                    &quote,
                    &plan.client_lock_outpoint,
                    plan.payment_preimage,
                    &claim_cell_deps,
                )?
            }
            _ => {
                let plan = LoopOutClaimTxPlan::from_record(&swap)?;
                build_loop_out_claim_transaction(
                    &quote,
                    &plan.payout_outpoint,
                    plan.payment_preimage,
                    &claim_cell_deps,
                )?
            }
        };
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
        // The claim transaction is deterministically rebuilt from the persisted swap
        // record (secp256k1 RFC6979 signatures) and its rebuilt hash is already
        // verified against the persisted `LiquidityChainTxRecord::tx_hash` above, so
        // the signed bytes are never persisted or reloaded.
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

    async fn watch_provider_claim(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let swap = self
            .store
            .get_liquidity_swap(&swap_id)
            .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?
            .ok_or_else(|| {
                LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}"))
            })?;
        if swap.role != LiquiditySwapRole::Provider || swap.swap_kind != LiquiditySwapKind::LoopOut
        {
            return Err(LiquidityLoopOutError::Chain(
                "cannot watch provider claim for non-provider loop out swap".to_string(),
            ));
        }
        if !matches!(
            swap.state,
            LiquiditySwapState::PaymentSettled | LiquiditySwapState::ClaimPending
        ) {
            return Err(LiquidityLoopOutError::Chain(format!(
                "cannot watch provider claim in state {:?}",
                swap.state
            )));
        }
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
        let outpoint = swap.onchain_outpoint.clone().ok_or_else(|| {
            LiquidityLoopOutError::Chain(
                "cannot watch provider claim without payout outpoint".to_string(),
            )
        })?;
        let artifact = self
            .liquidity_lock_artifact
            .as_ref()
            .ok_or_else(Self::missing_payout_builder)?;
        let lock_script =
            build_liquidity_lock_script(artifact, &Self::payout_output_params(&quote));
        self.ckb_chain_actor
            .send_message(CkbChainMessage::CreateOutPointSpendTracer(
                CkbOutPointSpendTracer {
                    outpoint: outpoint.clone(),
                    lock_script,
                    confirmations: 1,
                    callback: Self::provider_claim_tracer_callback_for(
                        swap_id,
                        outpoint,
                        swap.payment_hash,
                        myself,
                    ),
                },
            ))
            .map_err(|error| {
                LiquidityLoopOutError::Chain(format!(
                    "create provider claim outpoint tracer failed: {error}"
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
            refund_cell_deps.extend(udt_cell_deps);
        }
        let tx = match (record.swap_kind, record.role) {
            (LiquiditySwapKind::LoopIn, LiquiditySwapRole::Client) => {
                build_loop_in_client_refund_transaction(
                    &quote,
                    &plan.payout_outpoint,
                    plan.refund_after_lock_time,
                    &refund_cell_deps,
                )?
            }
            _ => build_loop_out_refund_transaction(
                &quote,
                &plan.payout_outpoint,
                plan.refund_after_lock_time,
                &refund_cell_deps,
            )?,
        };
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
        // The refund transaction is deterministically rebuilt from the persisted swap
        // record (secp256k1 RFC6979 signatures) and its rebuilt hash is already
        // verified against the persisted `LiquidityChainTxRecord::tx_hash` above, so
        // the signed bytes are never persisted or reloaded.
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

    use crate::ckb::{CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult, FundingTx, LiveCell};
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

        async fn watch_loop_in_lock(
            &mut self,
            _swap_id: Hash256,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn broadcast_loop_in_lock(
            &mut self,
            _quote: &LoopOutQuoteTerms,
            _funding_tx: &str,
            _myself: ActorRef<LiquidityActorMessage>,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        fn ensure_loop_in_lock_available(&mut self, _funding_tx: &str) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn validate_observed_loop_in_lock(
            &mut self,
            _quote: &LoopOutQuoteTerms,
            _outpoint: &packed::OutPoint,
        ) -> Result<(), Self::Error> {
            Err(LiquidityLoopOutError::Chain("unused".to_string()))
        }

        async fn validate_observed_loop_out_payout(
            &mut self,
            _quote: &LoopOutQuoteTerms,
            _outpoint: &packed::OutPoint,
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

        async fn watch_provider_claim(
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
        signed_txs: Arc<Mutex<HashMap<(Hash256, LiquidityChainTxRole), packed::Transaction>>>,
        status_events: Arc<Mutex<Vec<LiquidityChainTxStatus>>>,
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
            swap_id: &Hash256,
            update: LiquiditySwapUpdate,
        ) -> Result<(), LiquidityStoreError> {
            let mut swaps = self.swaps.lock().unwrap();
            let swap = swaps
                .get_mut(swap_id)
                .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
            if update.onchain_outpoint.is_some() {
                swap.onchain_outpoint = update.onchain_outpoint;
            }
            swap.updated_at = update.updated_at;
            Ok(())
        }

        fn insert_liquidity_chain_tx(
            &self,
            record: LiquidityChainTxRecord,
        ) -> Result<(), LiquidityStoreError> {
            let mut chain_txs = self.chain_txs.lock().unwrap();
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
            Ok(self
                .chain_txs
                .lock()
                .unwrap()
                .get(&(*swap_id, role))
                .cloned())
        }

        fn insert_liquidity_chain_tx_signed_tx(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
            tx: packed::Transaction,
        ) -> Result<(), LiquidityStoreError> {
            self.signed_txs.lock().unwrap().insert((*swap_id, role), tx);
            Ok(())
        }

        fn get_liquidity_chain_tx_signed_tx(
            &self,
            swap_id: &Hash256,
            role: LiquidityChainTxRole,
        ) -> Result<Option<packed::Transaction>, LiquidityStoreError> {
            Ok(self
                .signed_txs
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
            self.status_events.lock().unwrap().push(status);
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

        fn list_liquidity_chain_txs_by_swap(
            &self,
            swap_id: &Hash256,
        ) -> Result<Vec<LiquidityChainTxRecord>, LiquidityStoreError> {
            Ok(self
                .chain_txs
                .lock()
                .unwrap()
                .values()
                .filter(|record| record.swap_id == *swap_id)
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

        fn set_provider_mode(&self, _enabled: bool) -> Result<(), LiquidityStoreError> {
            Err(LiquidityStoreError::Backend("unused".to_string()))
        }

        fn get_provider_mode(&self) -> Result<bool, LiquidityStoreError> {
            Ok(false)
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
        CreateOutPointSpendTracer {
            outpoint: packed::OutPoint,
            lock_script: packed::Script,
            confirmations: u64,
        },
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
                CkbChainMessage::CreateOutPointSpendTracer(tracer) => {
                    events
                        .lock()
                        .unwrap()
                        .push(MockCkbEvent::CreateOutPointSpendTracer {
                            outpoint: tracer.outpoint,
                            lock_script: tracer.lock_script,
                            confirmations: tracer.confirmations,
                        });
                }
                _ => {}
            }
            Ok(())
        }
    }

    struct TxCapturingCkbActor;

    #[async_trait::async_trait]
    impl Actor for TxCapturingCkbActor {
        type Msg = CkbChainMessage;
        type State = Arc<Mutex<Vec<TransactionView>>>;
        type Arguments = Arc<Mutex<Vec<TransactionView>>>;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            txs: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(txs)
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            txs: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            if let CkbChainMessage::SendTx(tx, reply) = message {
                txs.lock().unwrap().push(tx);
                let _ = reply.send(Ok(()));
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
            expires_at: 20_000,
            payout_deadline: 30_000,
            refund_after_lock_time: 40_000,
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            client_invoice: None,
        }
    }

    fn test_loop_in_quote_terms() -> LoopOutQuoteTerms {
        LoopOutQuoteTerms {
            swap_kind: LiquiditySwapKind::LoopIn,
            client_invoice: Some("lnbc-client-invoice".to_string()),
            ..test_loop_out_quote_terms()
        }
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
        test_funding_transaction_with_lock_script(quote, output_index, liquidity_script)
    }

    fn test_funding_transaction_with_lock_script(
        quote: &LoopOutQuoteTerms,
        output_index: u32,
        liquidity_script: packed::Script,
    ) -> TransactionView {
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

    fn loop_in_lock_script_for_quote(quote: &LoopOutQuoteTerms) -> packed::Script {
        build_loop_in_client_lock_output(&liquidity_lock_artifact(), quote)
            .expect("loop in client lock output builds")
            .0
            .lock()
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
            std::slice::from_ref(&cell_dep),
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
            std::slice::from_ref(&cell_dep),
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
            std::slice::from_ref(&cell_dep),
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
            std::slice::from_ref(&cell_dep),
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
    fn chain_watcher_loop_in_provider_claim_plan_uses_client_lock_outpoint() {
        let preimage: Hash256 = [4u8; 32].into();
        let outpoint = test_outpoint(42);
        let mut record = test_swap_record_with_preimage(preimage);
        record.role = LiquiditySwapRole::Provider;
        record.swap_kind = LiquiditySwapKind::LoopIn;
        record.state = LiquiditySwapState::ClaimPending;
        record.onchain_outpoint = Some(outpoint.clone());

        let plan = LoopInProviderClaimTxPlan::from_record(&record).unwrap();

        assert_eq!(plan.swap_id, record.swap_id);
        assert_eq!(plan.client_lock_outpoint, outpoint);
        assert_eq!(plan.payment_preimage, preimage);
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
        assert_eq!(
            store.status_events.lock().unwrap().as_slice(),
            [LiquidityChainTxStatus::Rejected]
        );

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
            store.status_events.lock().unwrap().as_slice(),
            [
                LiquidityChainTxStatus::Rejected,
                LiquidityChainTxStatus::Rejected
            ]
        );
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
    async fn ckb_watcher_rebroadcasts_persisted_payout_after_restart() {
        let quote = test_loop_out_quote_terms();
        let funded_tx = test_funding_transaction_with_script(&quote, 0);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let expected_outpoint = packed::OutPoint::new(signed_tx.hash(), 1);
        let reserve_events = Arc::new(Mutex::new(Vec::new()));
        let (reserve_ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events: reserve_events.clone(),
                funded_tx,
                signed_tx: signed_tx.clone(),
                send_error: false,
            },
        )
        .await
        .unwrap();
        let store = NoopLiquidityStore::default();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            reserve_ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );
        let outpoint = watcher.reserve_payout_lock_outpoint(&quote).await.unwrap();
        assert_eq!(outpoint, expected_outpoint);
        wait_for_mock_events(&reserve_events, 4).await;

        // Simulate restart: fresh watcher with an empty in-memory pending map.
        let sent_txs = Arc::new(Mutex::new(Vec::new()));
        let (broadcast_ckb_actor, _handle) =
            ractor::Actor::spawn(None, TxCapturingCkbActor, sent_txs.clone())
                .await
                .unwrap();
        let mut restarted_watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            broadcast_ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );
        assert!(restarted_watcher.pending_payout_txs.is_empty());

        restarted_watcher
            .broadcast_payout_lock(
                &quote,
                &expected_outpoint,
                spawn_mock_liquidity_actor().await.0,
            )
            .await
            .unwrap();

        let sent = sent_txs.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let sent_hash: Hash256 = sent[0].hash().into();
        assert_eq!(sent_hash, signed_tx.hash().into());
    }

    #[tokio::test]
    async fn ckb_watcher_payout_rebroadcast_missing_signed_tx_fails_closed() {
        let quote = test_loop_out_quote_terms();
        let outpoint = test_outpoint(41);
        let (ckb_actor, _handle) =
            ractor::Actor::spawn(None, MockCkbActor, Arc::new(Mutex::new(Vec::new())))
                .await
                .unwrap();
        let store = NoopLiquidityStore::default();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [42u8; 32].into(),
                outpoint: Some(outpoint.clone()),
                status: LiquidityChainTxStatus::Planned,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );

        let error = watcher
            .broadcast_payout_lock(&quote, &outpoint, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("missing persisted signed transaction"));
    }

    #[tokio::test]
    async fn ckb_watcher_payout_rebroadcast_hash_mismatch_fails_closed() {
        let quote = test_loop_out_quote_terms();
        let outpoint = test_outpoint(43);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let (ckb_actor, _handle) =
            ractor::Actor::spawn(None, MockCkbActor, Arc::new(Mutex::new(Vec::new())))
                .await
                .unwrap();
        let store = NoopLiquidityStore::default();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [44u8; 32].into(),
                outpoint: Some(outpoint.clone()),
                status: LiquidityChainTxStatus::Planned,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        store
            .insert_liquidity_chain_tx_signed_tx(
                &quote.quote_id,
                LiquidityChainTxRole::Payout,
                signed_tx.data(),
            )
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );

        let error = watcher
            .broadcast_payout_lock(&quote, &outpoint, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("persisted signed transaction hash does not match persisted record"));
    }

    #[tokio::test]
    async fn ckb_watcher_broadcast_loop_in_lock_persists_tx_identity_before_send_tx() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_in_quote_terms();
        let loop_in_lock_script = loop_in_lock_script_for_quote(&quote);
        let funded_tx =
            test_funding_transaction_with_lock_script(&quote, 0, loop_in_lock_script.clone());
        let signed_tx =
            test_funding_transaction_with_lock_script(&quote, 1, loop_in_lock_script.clone());
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
        let mut swap = test_swap_record_with_outpoint(test_outpoint(35));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.state = LiquiditySwapState::OnchainLockPending;
        store.insert_liquidity_swap(swap).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );

        watcher
            .broadcast_loop_in_lock(&quote, "local-wallet", spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap();

        wait_for_mock_events(&events, 7).await;
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                MockCkbEvent::Fund {
                    script: loop_in_lock_script,
                    local_amount: quote.amount + quote.provider_fee,
                    local_reserved_ckb_amount: quote.capacity_requirement_ckb
                        - u64::try_from(quote.amount + quote.provider_fee).unwrap(),
                },
                MockCkbEvent::Sign,
                MockCkbEvent::RemoveFundingTx(funded_tx.hash().into()),
                MockCkbEvent::AddFundingTx(signed_tx.hash().into()),
                MockCkbEvent::SendTx,
                MockCkbEvent::CreateTxTracer(
                    CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                ),
                MockCkbEvent::CommitFundingTx(signed_tx.hash().into()),
            ]
        );
        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .expect("loop in lock tx record is persisted");
        assert_eq!(record.role, LiquidityChainTxRole::Payout);
        assert_eq!(record.tx_hash, signed_tx.hash().into());
        assert_eq!(record.status, LiquidityChainTxStatus::Broadcast);
        assert_eq!(record.outpoint, Some(expected_outpoint.clone()));
        let swap = store.get_liquidity_swap(&quote.quote_id).unwrap().unwrap();
        assert_eq!(swap.onchain_outpoint, Some(expected_outpoint));
    }

    #[tokio::test]
    async fn ckb_watcher_watch_loop_in_lock_uses_existing_record_without_send() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let swap_id: Hash256 = [36u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [37u8; 32].into(),
                outpoint: Some(test_outpoint(36)),
                status: LiquidityChainTxStatus::Broadcast,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store);

        watcher
            .watch_loop_in_lock(swap_id, spawn_mock_liquidity_actor().await.0)
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
    async fn ckb_watcher_rejects_unsupported_loop_in_funding_tx_descriptor() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let quote = test_loop_in_quote_terms();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );

        let error = watcher
            .broadcast_loop_in_lock(&quote, "0xfeedbeef", spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("unsupported loop in funding_tx"));
        assert!(events.lock().unwrap().is_empty());
        assert!(store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn ckb_watcher_retries_rejected_loop_in_lock_with_matching_pending_tx_as_planned() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_in_quote_terms();
        let loop_in_lock_script = loop_in_lock_script_for_quote(&quote);
        let signed_tx = test_funding_transaction_with_lock_script(&quote, 1, loop_in_lock_script);
        let outpoint = packed::OutPoint::new(signed_tx.hash(), 1);
        let (ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events: events.clone(),
                funded_tx: signed_tx.clone(),
                signed_tx: signed_tx.clone(),
                send_error: false,
            },
        )
        .await
        .unwrap();
        let store = NoopLiquidityStore::default();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: signed_tx.hash().into(),
                outpoint: Some(outpoint),
                status: LiquidityChainTxStatus::Rejected,
                failure_reason: Some("previous send failed".to_string()),
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );
        watcher
            .pending_payout_txs
            .insert(quote.quote_id, signed_tx.clone());

        watcher
            .broadcast_loop_in_lock(&quote, "local-wallet", spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap();

        wait_for_mock_events(&events, 3).await;
        assert_eq!(
            store.status_events.lock().unwrap().as_slice(),
            [
                LiquidityChainTxStatus::Planned,
                LiquidityChainTxStatus::Broadcast
            ]
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, MockCkbEvent::SendTx))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn ckb_watcher_rejected_loop_in_lock_without_pending_tx_returns_clear_retry_error() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let quote = test_loop_in_quote_terms();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id: quote.quote_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [38u8; 32].into(),
                outpoint: Some(test_outpoint(38)),
                status: LiquidityChainTxStatus::Rejected,
                failure_reason: Some("previous send failed".to_string()),
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store.clone(),
            liquidity_lock_artifact(),
        );

        let error = watcher
            .broadcast_loop_in_lock(&quote, "local-wallet", spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("missing persisted signed transaction"));
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(
            store
                .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
                .unwrap()
                .unwrap()
                .status,
            LiquidityChainTxStatus::Rejected
        );
    }

    #[tokio::test]
    async fn ckb_watcher_watch_loop_in_lock_allows_planned_record_for_crash_window() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let swap_id: Hash256 = [39u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: [40u8; 32].into(),
                outpoint: Some(test_outpoint(39)),
                status: LiquidityChainTxStatus::Planned,
                failure_reason: None,
                created_at: 1,
                updated_at: 2,
            })
            .unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new(ckb_actor, store);

        watcher
            .watch_loop_in_lock(swap_id, spawn_mock_liquidity_actor().await.0)
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
        assert!(
            store
                .get_liquidity_chain_tx_signed_tx(&quote.quote_id, LiquidityChainTxRole::Claim)
                .unwrap()
                .is_none(),
            "claim is deterministically rebuilt and hash-verified, so its signed tx is not persisted"
        );
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
        assert!(
            store
                .get_liquidity_chain_tx_signed_tx(&quote.quote_id, LiquidityChainTxRole::Refund)
                .unwrap()
                .is_none(),
            "refund is deterministically rebuilt and hash-verified, so its signed tx is not persisted"
        );
    }

    #[tokio::test]
    async fn ckb_watcher_broadcast_loop_in_client_refund_persists_tx_identity_before_send_tx() {
        let sent_txs = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) =
            ractor::Actor::spawn(None, TxCapturingCkbActor, sent_txs.clone())
                .await
                .unwrap();
        let quote = test_loop_in_quote_terms();
        let mut swap = test_swap_record_with_outpoint(test_outpoint(28));
        swap.swap_id = quote.quote_id;
        swap.quote_id = quote.quote_id;
        swap.swap_kind = LiquiditySwapKind::LoopIn;
        swap.role = LiquiditySwapRole::Client;
        swap.state = LiquiditySwapState::RefundPending;
        swap.refund_after_lock_time = quote.refund_after_lock_time;
        let cell_dep = packed::CellDep::new_builder()
            .out_point(test_outpoint(29))
            .dep_type(ckb_types::core::DepType::Code)
            .build();
        let store = NoopLiquidityStore::default();
        store.insert_loop_out_quote(quote.clone(), 1).unwrap();
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
            ckb_actor,
            store.clone(),
            loop_in_lock_script_for_quote(&quote),
            vec![cell_dep.clone()],
        );

        watcher.broadcast_refund(&swap).await.unwrap();

        let record = store
            .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Refund)
            .unwrap()
            .expect("refund tx record is persisted");
        let txs = sent_txs.lock().unwrap();
        assert_eq!(txs.len(), 1);
        let tx = &txs[0];
        assert_eq!(record.tx_hash, tx.hash().into());
        assert_eq!(record.role, LiquidityChainTxRole::Refund);
        assert_eq!(record.status, LiquidityChainTxStatus::Broadcast);
        assert!(record.outpoint.is_none());
        let input = tx.inputs().get(0).unwrap();
        assert_eq!(input.previous_output(), swap.onchain_outpoint.unwrap());
        assert_eq!(u64::from(input.since()), quote.refund_after_lock_time);
        assert_eq!(tx.outputs().get(0).unwrap().lock(), quote.refund_lock);
        assert_eq!(
            tx.cell_deps().into_iter().collect::<Vec<_>>(),
            vec![cell_dep]
        );
        assert_eq!(
            tx.witnesses().get(0).unwrap(),
            build_liquidity_lock_refund_witness()
        );
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

        assert!(error.to_string().contains("non-watchable status"));
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
    async fn ckb_watcher_watches_planned_payout_record_for_crash_window() {
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
        let swap_id: Hash256 = [41u8; 32].into();
        store
            .insert_liquidity_chain_tx(LiquidityChainTxRecord {
                swap_id,
                role: LiquidityChainTxRole::Payout,
                tx_hash: signed_tx.hash().into(),
                outpoint: Some(test_outpoint(41)),
                status: LiquidityChainTxStatus::Planned,
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
    async fn ckb_watcher_maps_rejected_payout_tracer_result_to_actor_message() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let quote = test_loop_out_quote_terms();
        let funded_tx = test_funding_transaction_with_script(&quote, 0);
        let signed_tx = test_funding_transaction_with_script(&quote, 1);
        let tx_hash: Hash256 = signed_tx.hash().into();
        let (ckb_actor, _handle) = ractor::Actor::spawn(
            None,
            PayoutMockCkbActor,
            PayoutMockCkbActorArgs {
                events,
                funded_tx,
                signed_tx,
                send_error: false,
            },
        )
        .await
        .unwrap();
        let (liquidity_actor, mut receiver) = spawn_mock_liquidity_actor().await;

        CkbLiquidityChainWatcher::<NoopLiquidityStore>::payout_tracer_callback_for(
            quote.quote_id,
            tx_hash,
            ckb_actor,
            liquidity_actor,
        )
        .send(CkbTxTracingResult {
            tx_hash,
            tx_status: TxStatus::Rejected("rejected by pool".to_string()),
        })
        .expect("callback accepts rejected status");

        let message = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .expect("liquidity actor receives rejected continuation")
            .expect("mock actor is alive");
        assert!(matches!(
            message,
            LiquidityActorMessage::ChainTxRejected(swap_id, role, reason)
                if swap_id == quote.quote_id
                    && role == LiquidityChainTxRole::Payout
                    && reason == "rejected by pool"
        ));
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

    fn provider_claim_spending_tx(
        watched_outpoint: &packed::OutPoint,
        input_index: usize,
        witness: packed::Bytes,
    ) -> TransactionView {
        let other_outpoint = test_outpoint(99);
        let inputs = if input_index == 0 {
            vec![watched_outpoint.clone(), other_outpoint]
        } else {
            vec![other_outpoint, watched_outpoint.clone()]
        };
        ckb_types::core::TransactionBuilder::default()
            .set_inputs(
                inputs
                    .into_iter()
                    .map(|outpoint| packed::CellInput::new(outpoint, 0))
                    .collect(),
            )
            .set_witnesses(vec![packed::Bytes::default(), witness])
            .build()
    }

    #[tokio::test]
    async fn provider_claim_callback_uses_reported_input_index_and_sends_once() {
        let swap_id = [81u8; 32].into();
        let preimage: Hash256 = [82u8; 32].into();
        let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
        let outpoint = test_outpoint(81);
        let tx = provider_claim_spending_tx(
            &outpoint,
            1,
            build_liquidity_lock_claim_witness(preimage.into()),
        );
        let (actor, mut messages) = spawn_mock_liquidity_actor().await;

        CkbLiquidityChainWatcher::<NoopLiquidityStore>::provider_claim_tracer_callback_for(
            swap_id,
            outpoint.clone(),
            payment_hash,
            actor,
        )
        .send(Ok(crate::ckb::CkbOutPointSpendTracingResult {
            outpoint,
            spending_transaction: tx,
            input_index: 1,
            script_group_input_index: 1,
            block_number: 1,
        }))
        .unwrap();

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), messages.recv())
                .await
                .unwrap(),
            Some(LiquidityActorMessage::ProviderClaimObserved(id)) if id == swap_id
        ));
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn provider_refund_group_does_not_scan_unrelated_claim_like_witness() {
        let swap_id = [89u8; 32].into();
        let preimage = [90u8; 32];
        let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
        let outpoint = test_outpoint(89);
        let tx = ckb_types::core::TransactionBuilder::default()
            .set_inputs(vec![
                packed::CellInput::new(test_outpoint(97), 0),
                packed::CellInput::new(outpoint.clone(), 0),
            ])
            .set_witnesses(vec![
                build_liquidity_lock_claim_witness(preimage),
                build_liquidity_lock_refund_witness(),
            ])
            .build();
        let (actor, mut messages) = spawn_mock_liquidity_actor().await;

        CkbLiquidityChainWatcher::<NoopLiquidityStore>::provider_claim_tracer_callback_for(
            swap_id,
            outpoint.clone(),
            payment_hash,
            actor,
        )
        .send(Ok(crate::ckb::CkbOutPointSpendTracingResult {
            outpoint,
            spending_transaction: tx,
            input_index: 1,
            script_group_input_index: 1,
            block_number: 1,
        }))
        .unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), messages.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn provider_claim_callback_uses_first_same_script_group_witness() {
        let swap_id = [87u8; 32].into();
        let preimage = [88u8; 32];
        let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
        let outpoint = test_outpoint(87);
        let tx = ckb_types::core::TransactionBuilder::default()
            .set_inputs(vec![
                packed::CellInput::new(test_outpoint(98), 0),
                packed::CellInput::new(outpoint.clone(), 0),
            ])
            .set_witnesses(vec![
                build_liquidity_lock_claim_witness(preimage),
                packed::Bytes::default(),
            ])
            .build();
        let (actor, mut messages) = spawn_mock_liquidity_actor().await;

        CkbLiquidityChainWatcher::<NoopLiquidityStore>::provider_claim_tracer_callback_for(
            swap_id,
            outpoint.clone(),
            payment_hash,
            actor,
        )
        .send(Ok(crate::ckb::CkbOutPointSpendTracingResult {
            outpoint,
            spending_transaction: tx,
            input_index: 1,
            script_group_input_index: 0,
            block_number: 1,
        }))
        .unwrap();

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), messages.recv())
                .await
                .expect("valid grouped claim continuation timed out"),
            Some(LiquidityActorMessage::ProviderClaimObserved(id)) if id == swap_id
        ));
    }

    #[tokio::test]
    async fn provider_claim_callback_rejects_refund_wrong_preimage_and_malformed_witnesses() {
        let swap_id = [83u8; 32].into();
        let expected_preimage: Hash256 = [84u8; 32].into();
        let payment_hash = HashAlgorithm::CkbHash.hash(expected_preimage).into();
        let outpoint = test_outpoint(83);
        let witnesses = [
            build_liquidity_lock_refund_witness(),
            build_liquidity_lock_claim_witness([85u8; 32]),
            Bytes::from(vec![1, 2, 3]).pack(),
        ];

        for witness in witnesses {
            let tx = provider_claim_spending_tx(&outpoint, 1, witness);
            let (actor, mut messages) = spawn_mock_liquidity_actor().await;
            CkbLiquidityChainWatcher::<NoopLiquidityStore>::provider_claim_tracer_callback_for(
                swap_id,
                outpoint.clone(),
                payment_hash,
                actor,
            )
            .send(Ok(crate::ckb::CkbOutPointSpendTracingResult {
                outpoint: outpoint.clone(),
                spending_transaction: tx,
                input_index: 1,
                script_group_input_index: 1,
                block_number: 1,
            }))
            .unwrap();

            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), messages.recv())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn provider_claim_watch_uses_persisted_outpoint_and_quote_derived_script() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone())
            .await
            .unwrap();
        let store = NoopLiquidityStore::default();
        let mut quote = test_loop_out_quote_terms();
        let outpoint = test_outpoint(86);
        let mut swap = test_swap_record_with_outpoint(outpoint.clone());
        swap.role = LiquiditySwapRole::Provider;
        swap.state = LiquiditySwapState::PaymentSettled;
        swap.payment_hash = quote.payment_hash;
        quote.quote_id = swap.quote_id;
        store
            .insert_loop_out_quote(quote.clone(), 1)
            .expect("persist quote");
        store.insert_liquidity_swap(swap.clone()).unwrap();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            store,
            liquidity_lock_artifact(),
        );

        watcher
            .watch_provider_claim(swap.swap_id, spawn_mock_liquidity_actor().await.0)
            .await
            .unwrap();

        wait_for_mock_events(&events, 1).await;
        assert_eq!(
            events.lock().unwrap().as_slice(),
            [MockCkbEvent::CreateOutPointSpendTracer {
                outpoint,
                lock_script: liquidity_lock_script_for_quote(&quote),
                confirmations: 1,
            }]
        );
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

    #[test]
    fn refund_plan_rejects_loop_in_provider_refund() {
        let record = LiquiditySwapRecord {
            swap_kind: LiquiditySwapKind::LoopIn,
            role: LiquiditySwapRole::Provider,
            ..test_provider_refund_pending_record(test_outpoint(8))
        };

        let error = LoopOutRefundTxPlan::from_record(&record).unwrap_err();

        assert!(error.to_string().contains("loop in provider"));
    }

    fn test_loop_in_quote(now_ms: u64) -> LoopOutQuoteTerms {
        let sk = SecretKey::from_slice(&[0xcd; 32]).unwrap();
        LoopOutQuoteTerms {
            quote_id: [1u8; 32].into(),
            swap_kind: LiquiditySwapKind::LoopIn,
            provider: Pubkey::from(sk.public_key(SECP256K1)),
            asset: LiquidityAsset {
                asset_id: "ckb".to_string(),
                kind: LiquidityAssetKind::Ckb,
                udt_type_script: None,
                min_amount: 1,
                max_amount: 100_000_000,
                available_capacity: 1_000_000,
                base_fee: 5,
                proportional_fee_ppm: 0,
                enabled: true,
            },
            amount: 100,
            provider_fee: 5,
            routing_fee_limit: 10,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 61,
            payment_hash: [44u8; 32].into(),
            payment_preimage: None,
            expires_at: now_ms,
            payout_deadline: now_ms,
            refund_after_lock_time: 10_000,
            claimant_lock: packed::Script::new_builder()
                .code_hash([10u8; 32].pack())
                .hash_type(packed::Byte::new(0))
                .args([1u8; 20].pack())
                .build(),
            refund_lock: packed::Script::new_builder()
                .code_hash([11u8; 32].pack())
                .hash_type(packed::Byte::new(0))
                .args([2u8; 20].pack())
                .build(),
            client_invoice: None,
        }
    }

    #[test]
    fn validate_liquidity_lock_args_accepts_matching_ckb_lock_script_args() {
        use crate::liquidity::build_liquidity_lock_args;

        let quote = test_loop_in_quote(1_000_000);
        let code_hash = [9u8; 32].pack();
        let args = build_liquidity_lock_args(
            quote.payment_hash.as_ref().try_into().unwrap(),
            &quote.claimant_lock,
            &quote.refund_lock,
            quote.refund_after_lock_time,
            loop_in_gross_onchain_amount(&quote).unwrap(),
            None,
        );
        assert!(validate_liquidity_lock_args(
            &args,
            &quote,
            loop_in_gross_onchain_amount(&quote).unwrap(),
            &code_hash,
            0,
            &code_hash,
            0,
        )
        .is_ok());
    }

    #[test]
    fn validate_liquidity_lock_args_rejects_wrong_code_hash() {
        let quote = test_loop_in_quote(1_000_000);
        let args = vec![0u8; 152];
        let result = validate_liquidity_lock_args(
            &args,
            &quote,
            loop_in_gross_onchain_amount(&quote).unwrap(),
            &[9u8; 32].pack(),
            0,
            &[8u8; 32].pack(),
            0,
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("liquidity-lock contract"));
    }

    #[test]
    fn validate_liquidity_lock_args_rejects_wrong_payment_hash() {
        let quote = test_loop_in_quote(1_000_000);
        let mut args = vec![0u8; 152];
        args[0..32].copy_from_slice(&[99u8; 32]);
        let result = validate_liquidity_lock_args(
            &args,
            &quote,
            loop_in_gross_onchain_amount(&quote).unwrap(),
            &[9u8; 32].pack(),
            0,
            &[9u8; 32].pack(),
            0,
        );
        assert!(result.unwrap_err().to_string().contains("payment_hash"));
    }

    #[test]
    fn validate_liquidity_lock_args_rejects_wrong_amount() {
        use crate::liquidity::build_liquidity_lock_args;

        let quote = test_loop_in_quote(1_000_000);
        let code_hash = [9u8; 32].pack();
        let args = build_liquidity_lock_args(
            quote.payment_hash.as_ref().try_into().unwrap(),
            &quote.claimant_lock,
            &quote.refund_lock,
            quote.refund_after_lock_time,
            999,
            None,
        );
        let result = validate_liquidity_lock_args(
            &args,
            &quote,
            loop_in_gross_onchain_amount(&quote).unwrap(),
            &code_hash,
            0,
            &code_hash,
            0,
        );
        assert!(result.unwrap_err().to_string().contains("amount"));
    }

    #[test]
    fn validate_liquidity_lock_args_uses_direction_specific_expected_amount() {
        use crate::liquidity::build_liquidity_lock_args;

        let quote = test_loop_out_quote_terms();
        let code_hash = [9u8; 32].pack();
        let args = build_liquidity_lock_args(
            quote.payment_hash.as_ref().try_into().unwrap(),
            &quote.claimant_lock,
            &quote.refund_lock,
            quote.refund_after_lock_time,
            quote.amount,
            None,
        );

        validate_liquidity_lock_args(&args, &quote, quote.amount, &code_hash, 0, &code_hash, 0)
            .unwrap();
        assert!(validate_liquidity_lock_args(
            &args,
            &quote,
            quote.amount + quote.provider_fee,
            &code_hash,
            0,
            &code_hash,
            0,
        )
        .unwrap_err()
        .to_string()
        .contains("amount"));
    }

    #[test]
    fn validate_liquidity_lock_args_rejects_each_exact_field_mismatch() {
        use crate::liquidity::build_liquidity_lock_args;

        let quote = test_loop_out_quote_terms();
        let code_hash = [9u8; 32].pack();
        let args = build_liquidity_lock_args(
            quote.payment_hash.as_ref().try_into().unwrap(),
            &quote.claimant_lock,
            &quote.refund_lock,
            quote.refund_after_lock_time,
            quote.amount,
            None,
        );
        let validate = |args: &[u8], cell_hash_type| {
            validate_liquidity_lock_args(
                args,
                &quote,
                quote.amount,
                &code_hash,
                0,
                &code_hash,
                cell_hash_type,
            )
            .unwrap_err()
            .to_string()
        };

        assert!(validate(&args, 1).contains("liquidity-lock contract"));

        let mut wrong = args.clone();
        wrong.pop();
        assert!(validate(&wrong, 0).contains("length"));

        let fields = [
            (0, "payment_hash"),
            (32, "claimant_lock_hash"),
            (64, "refund_lock_hash"),
            (96, "refund_after_lock_time"),
            (104, "amount"),
            (120, "asset_type_hash"),
        ];
        for (offset, expected_error) in fields {
            let mut wrong = args.clone();
            wrong[offset] ^= 1;
            assert!(
                validate(&wrong, 0).contains(expected_error),
                "offset {offset} did not report {expected_error}"
            );
        }
    }

    struct LiveCellMockCkbActor;

    #[async_trait::async_trait]
    impl Actor for LiveCellMockCkbActor {
        type Msg = CkbChainMessage;
        type State = Option<LiveCell>;
        type Arguments = Option<LiveCell>;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            cell: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(cell)
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            if let CkbChainMessage::GetLiveCell(_outpoint, reply) = message {
                let _ = reply.send(Ok(state.take()));
            }
            Ok(())
        }
    }

    fn observed_loop_in_lock_output(quote: &LoopOutQuoteTerms) -> packed::CellOutput {
        let lock = loop_in_lock_script_for_quote(quote);
        let udt_type_script: Option<packed::Script> =
            quote.asset.udt_type_script.clone().map(Into::into);
        packed::CellOutput::new_builder()
            .capacity(quote.capacity_requirement_ckb)
            .lock(lock)
            .type_(udt_type_script.pack())
            .build()
    }

    fn observed_loop_in_lock_data(quote: &LoopOutQuoteTerms) -> packed::Bytes {
        if quote.asset.kind == LiquidityAssetKind::Udt {
            Bytes::from(
                loop_in_gross_onchain_amount(quote)
                    .unwrap()
                    .to_le_bytes()
                    .to_vec(),
            )
            .pack()
        } else {
            Bytes::new().pack()
        }
    }

    async fn validate_observed_cell(
        quote: &LoopOutQuoteTerms,
        cell: Option<LiveCell>,
        loop_out: bool,
    ) -> Result<(), LiquidityLoopOutError> {
        let (ckb_actor, handle) = ractor::Actor::spawn(None, LiveCellMockCkbActor, cell)
            .await
            .unwrap();
        let stop_ref = ckb_actor.clone();
        let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_artifact(
            ckb_actor,
            NoopLiquidityStore::default(),
            liquidity_lock_artifact(),
        );
        let result = if loop_out {
            watcher
                .validate_observed_loop_out_payout(quote, &test_outpoint(0))
                .await
        } else {
            watcher
                .validate_observed_loop_in_lock(quote, &test_outpoint(0))
                .await
        };
        stop_ref.stop(Some("test done".to_string()));
        let _ = handle.await;
        result
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_accepts_valid_ckb_cell() {
        let quote = test_loop_in_quote_terms();
        let cell = LiveCell {
            output: observed_loop_in_lock_output(&quote),
            data: observed_loop_in_lock_data(&quote),
        };
        validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_insufficient_ckb_capacity() {
        let quote = test_loop_in_quote_terms();
        let output = observed_loop_in_lock_output(&quote)
            .as_builder()
            .capacity(quote.capacity_requirement_ckb - 1)
            .build();
        let cell = LiveCell {
            output,
            data: observed_loop_in_lock_data(&quote),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("capacity"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_accepts_valid_udt_cell() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let cell = LiveCell {
            output: observed_loop_in_lock_output(&quote),
            data: observed_loop_in_lock_data(&quote),
        };
        validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_missing_udt_data() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let cell = LiveCell {
            output: observed_loop_in_lock_output(&quote),
            data: Bytes::new().pack(),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UDT data length"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_15_byte_udt_data() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let cell = LiveCell {
            output: observed_loop_in_lock_output(&quote),
            data: Bytes::from(vec![0u8; 15]).pack(),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UDT data length"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_17_byte_udt_data() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let cell = LiveCell {
            output: observed_loop_in_lock_output(&quote),
            data: Bytes::from(vec![0u8; 17]).pack(),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UDT data length"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_wrong_udt_amount() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let wrong = loop_in_gross_onchain_amount(&quote).unwrap() + 1;
        let cell = LiveCell {
            output: observed_loop_in_lock_output(&quote),
            data: Bytes::from(wrong.to_le_bytes().to_vec()).pack(),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UDT amount mismatch"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_wrong_udt_type_script() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let output = observed_loop_in_lock_output(&quote)
            .as_builder()
            .type_(Some(script("wrong-udt")).pack())
            .build();
        let cell = LiveCell {
            output,
            data: observed_loop_in_lock_data(&quote),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UDT type script mismatch"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_unexpected_ckb_type_script() {
        let quote = test_loop_in_quote_terms();
        let output = observed_loop_in_lock_output(&quote)
            .as_builder()
            .type_(Some(script("unexpected-udt")).pack())
            .build();
        let cell = LiveCell {
            output,
            data: observed_loop_in_lock_data(&quote),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("type script"));
    }

    #[tokio::test]
    async fn validate_observed_loop_in_lock_rejects_insufficient_udt_capacity() {
        let (quote, _udt_type_script) = test_loop_in_udt_quote_terms();
        let output = observed_loop_in_lock_output(&quote)
            .as_builder()
            .capacity(quote.capacity_requirement_ckb - 1)
            .build();
        let cell = LiveCell {
            output,
            data: observed_loop_in_lock_data(&quote),
        };
        let error = validate_observed_cell(&quote, Some(cell), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("capacity"));
    }

    fn observed_loop_out_payout(quote: &LoopOutQuoteTerms) -> LiveCell {
        LiveCell {
            output: packed::CellOutput::new_builder()
                .capacity(quote.capacity_requirement_ckb)
                .lock(liquidity_lock_script_for_quote(quote))
                .build(),
            data: Bytes::new().pack(),
        }
    }

    #[tokio::test]
    async fn validate_observed_loop_out_payout_accepts_exact_live_ckb_cell() {
        let quote = test_loop_out_quote_terms();

        validate_observed_cell(&quote, Some(observed_loop_out_payout(&quote)), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn validate_observed_loop_out_payout_rejects_missing_or_spent_cell() {
        let quote = test_loop_out_quote_terms();

        let error = validate_observed_cell(&quote, None, true)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not found or already spent"));
    }

    #[tokio::test]
    async fn validate_observed_loop_out_payout_rejects_loop_in_gross_amount() {
        let quote = test_loop_out_quote_terms();
        let wrong_lock = crate::liquidity::tx::build_liquidity_lock_script(
            &liquidity_lock_artifact(),
            &LiquidityLockOutputParams {
                payment_hash: quote.payment_hash.into(),
                claimant_lock: quote.claimant_lock.clone(),
                refund_lock: quote.refund_lock.clone(),
                refund_after_lock_time: quote.refund_after_lock_time,
                amount: quote.amount + quote.provider_fee,
                asset_type_script: None,
                capacity: quote.capacity_requirement_ckb,
            },
        );
        let mut cell = observed_loop_out_payout(&quote);
        cell.output = cell.output.as_builder().lock(wrong_lock).build();

        let error = validate_observed_cell(&quote, Some(cell), true)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("amount"));
    }
}
