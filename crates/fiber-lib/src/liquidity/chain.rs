//! Chain adapter boundary for loop-out liquidity operations.

use ckb_types::{core::tx_pool::TxStatus, core::TransactionView, packed};
use fiber_types::{Hash256, HashAlgorithm, LiquidityChainTxRole, LiquiditySwapState};
use ractor::{ActorRef, RpcReplyPort};

use crate::ckb::{CkbChainMessage, CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult};
use crate::liquidity::actor::LiquidityActorMessage;
use crate::liquidity::store::{LiquiditySwapRecord, LiquiditySwapRole};
use crate::liquidity::tx::{
    build_liquidity_lock_output, LiquidityLockBuildError, LiquidityLockOutputParams,
    LiquidityLockScriptArtifact,
};
use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};

#[cfg(not(test))]
const CKB_SEND_TX_TIMEOUT_MS: u64 = 8000;
#[cfg(test)]
const CKB_SEND_TX_TIMEOUT_MS: u64 = 50;

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
    /// Build a refund transaction plan from a persisted provider swap record.
    pub fn from_record(
        record: &LiquiditySwapRecord,
        current_lock_time: u64,
    ) -> Result<Self, LiquidityLoopOutError> {
        if record.role != LiquiditySwapRole::Provider {
            return Err(LiquidityLoopOutError::Chain(
                "cannot build refund for non-provider loop out record".to_string(),
            ));
        }
        if current_lock_time < record.refund_after_lock_time {
            return Err(LiquidityLoopOutError::Chain(
                "cannot refund loop out payout before refund lock time".to_string(),
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
            let TxStatus::Committed(..) = result.tx_status else {
                return;
            };

            let message = match role {
                LiquidityChainTxRole::Payout => LiquidityActorMessage::PayoutConfirmed(swap_id),
                LiquidityChainTxRole::Claim => LiquidityActorMessage::ClaimConfirmed(swap_id),
                LiquidityChainTxRole::Refund => LiquidityActorMessage::RefundConfirmed(swap_id),
            };
            let _ = liquidity_actor.send_message(message);
        });
        RpcReplyPort::from(sender)
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
    use std::sync::{Arc, Mutex};

    use ckb_types::{
        bytes::Bytes,
        core::{tx_pool::TxStatus, TransactionView},
        packed,
        prelude::*,
        H256,
    };
    use fiber_types::{
        Hash256, HashAlgorithm, LiquidityAsset, LiquidityAssetKind, LiquidityChainTxRole,
        LiquiditySwapState, Pubkey,
    };
    use ractor::{Actor, ActorProcessingErr};
    use secp256k1::{SecretKey, SECP256K1};
    use tokio::sync::mpsc;

    use crate::ckb::{CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult};
    use crate::liquidity::store::{LiquiditySwapKind, LiquiditySwapRecord, LiquiditySwapRole};

    #[derive(Debug, Clone, Eq, PartialEq)]
    enum MockCkbEvent {
        SendTx,
        CreateTxTracer(CkbTxTracingMask),
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

    fn committed_result(tx_hash: Hash256) -> CkbTxTracingResult {
        CkbTxTracingResult {
            tx_hash,
            tx_status: TxStatus::Committed(1, H256::default(), 0),
        }
    }

    async fn assert_no_liquidity_message(
        receiver: &mut mpsc::UnboundedReceiver<LiquidityActorMessage>,
    ) {
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), receiver.recv()).await;
        assert!(result.is_err(), "unexpected liquidity actor message");
    }

    async fn assert_callback_maps_committed_status(
        role: LiquidityChainTxRole,
        expected: impl FnOnce(Hash256) -> LiquidityActorMessage,
    ) {
        let swap_id: Hash256 = [7u8; 32].into();
        let tx_hash: Hash256 = [8u8; 32].into();
        let (liquidity_actor, mut receiver) = spawn_mock_liquidity_actor().await;
        CkbLiquidityChainWatcher::tracer_callback_for(swap_id, role, liquidity_actor.clone())
            .send(CkbTxTracingResult::unknown(tx_hash))
            .expect("callback accepts unknown status");
        CkbLiquidityChainWatcher::tracer_callback_for(swap_id, role, liquidity_actor.clone())
            .send(CkbTxTracingResult {
                tx_hash,
                tx_status: TxStatus::Rejected("rejected".to_string()),
            })
            .expect("callback accepts rejected status");

        assert_no_liquidity_message(&mut receiver).await;

        CkbLiquidityChainWatcher::tracer_callback_for(swap_id, role, liquidity_actor)
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
        let watcher = CkbLiquidityChainWatcher::new(ckb_actor);
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
        let watcher = CkbLiquidityChainWatcher::new(ckb_actor);
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
        let watcher = CkbLiquidityChainWatcher::new(ckb_actor);
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

        let plan =
            LoopOutRefundTxPlan::from_record(&record, record.refund_after_lock_time).unwrap();

        assert_eq!(plan.swap_id, record.swap_id);
        assert_eq!(plan.payout_outpoint, record.onchain_outpoint.unwrap());
    }

    #[test]
    fn refund_plan_before_lock_time_fails() {
        let record = test_provider_refund_pending_record(test_outpoint(8));

        let error = LoopOutRefundTxPlan::from_record(&record, record.refund_after_lock_time - 1)
            .unwrap_err();

        assert!(error.to_string().contains("lock time"));
    }

    #[test]
    fn refund_plan_for_client_role_fails() {
        let record = LiquiditySwapRecord {
            role: LiquiditySwapRole::Client,
            ..test_provider_refund_pending_record(test_outpoint(8))
        };

        let error =
            LoopOutRefundTxPlan::from_record(&record, record.refund_after_lock_time).unwrap_err();

        assert!(error.to_string().contains("provider"));
    }
}
