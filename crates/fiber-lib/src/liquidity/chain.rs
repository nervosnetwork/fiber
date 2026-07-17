//! Chain adapter boundary for loop-out liquidity operations.

use ckb_types::packed;
use fiber_types::{Hash256, HashAlgorithm, LiquiditySwapState};
use ractor::ActorRef;

use crate::ckb::CkbChainMessage;
use crate::liquidity::actor::LiquidityActorMessage;
use crate::liquidity::store::LiquiditySwapRecord;
use crate::liquidity::tx::{
    build_liquidity_lock_output, LiquidityLockBuildError, LiquidityLockOutputParams,
    LiquidityLockScriptArtifact,
};
use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};

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

/// Chain boundary required by Loop Out liquidity workflows.
pub trait LiquidityChainWatcher {
    /// Adapter-specific error returned by chain operations.
    type Error;

    /// Reserve the payout lock outpoint for the accepted quote before broadcast.
    fn reserve_payout_lock_outpoint(
        &mut self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<packed::OutPoint, Self::Error>;

    /// Broadcast the payout lock transaction for the accepted quote and outpoint.
    fn broadcast_payout_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
        outpoint: &packed::OutPoint,
    ) -> Result<(), Self::Error>;

    /// Schedule payout lock watching and report completion back to `myself`.
    fn watch_payout_lock(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Broadcast the claim transaction for a paid Loop Out swap.
    fn broadcast_claim(&mut self, request: LoopOutClaimRequest) -> Result<(), Self::Error>;

    /// Schedule client claim watching and report completion back to `myself`.
    fn watch_claim(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;

    /// Broadcast the provider refund transaction for an expired payout lock.
    fn broadcast_refund(&mut self, record: &LiquiditySwapRecord) -> Result<(), Self::Error>;

    /// Schedule provider refund watching and report completion back to `myself`.
    fn watch_refund(
        &mut self,
        swap_id: Hash256,
        myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error>;
}

/// CKB-backed liquidity chain watcher boundary.
pub struct CkbLiquidityChainWatcher {
    ckb_chain_actor: ActorRef<CkbChainMessage>,
}

impl CkbLiquidityChainWatcher {
    /// Create a liquidity chain watcher backed by the CKB chain actor.
    pub fn new(ckb_chain_actor: ActorRef<CkbChainMessage>) -> Self {
        Self { ckb_chain_actor }
    }

    fn not_wired(operation: &str) -> LiquidityLoopOutError {
        LiquidityLoopOutError::Chain(format!(
            "liquidity chain operation `{operation}` is not wired to CKB transaction builders yet"
        ))
    }
}

impl LiquidityChainWatcher for CkbLiquidityChainWatcher {
    type Error = LiquidityLoopOutError;

    fn reserve_payout_lock_outpoint(
        &mut self,
        _quote: &LoopOutQuoteTerms,
    ) -> Result<packed::OutPoint, Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("reserve_payout_lock_outpoint"))
    }

    fn broadcast_payout_lock(
        &mut self,
        _quote: &LoopOutQuoteTerms,
        _outpoint: &packed::OutPoint,
    ) -> Result<(), Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("broadcast_payout_lock"))
    }

    fn watch_payout_lock(
        &mut self,
        _swap_id: Hash256,
        _myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("watch_payout_lock"))
    }

    fn broadcast_claim(&mut self, _request: LoopOutClaimRequest) -> Result<(), Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("broadcast_claim"))
    }

    fn watch_claim(
        &mut self,
        _swap_id: Hash256,
        _myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("watch_claim"))
    }

    fn broadcast_refund(&mut self, _record: &LiquiditySwapRecord) -> Result<(), Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("broadcast_refund"))
    }

    fn watch_refund(
        &mut self,
        _swap_id: Hash256,
        _myself: ActorRef<LiquidityActorMessage>,
    ) -> Result<(), Self::Error> {
        let _ = &self.ckb_chain_actor;
        Err(Self::not_wired("watch_refund"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_types::{bytes::Bytes, packed, prelude::*};
    use fiber_types::{Hash256, HashAlgorithm, LiquiditySwapState};

    use crate::liquidity::store::{LiquiditySwapKind, LiquiditySwapRecord, LiquiditySwapRole};

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
}
