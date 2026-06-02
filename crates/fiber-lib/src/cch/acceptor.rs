//! Operator-facing swap-proposal broadcaster for the CCH multi-asset flow.
//!
//! When a [`SendBTC`](super::SendBTC) / [`ReceiveBTC`](super::ReceiveBTC)
//! request involves a Fiber asset that is allowlisted but **not** in the
//! configured `fixed_rate_assets`, the hub cannot price the counterparty leg
//! itself. Instead it persists the [`SwapProposal`] in its own table
//! (no [`CchOrder`](fiber_types::CchOrder) exists yet) and broadcasts it
//! through this actor to every subscribed operator. The original `send_btc` /
//! `receive_btc` RPC returns immediately with that swap proposal — it does
//! **not** wait for the operator.
//!
//! The workflow resumes asynchronously when an operator answers with a
//! [`SwapProposalResponse`](fiber_types::SwapProposalResponse) via the
//! `submit_swap_proposal_response` RPC method; that response is routed to the
//! [`CchActor`](super::CchActor), which (on accept) mints the counterparty
//! invoice and creates the order as `Pending`.
//!
//! This actor is therefore a publish/subscribe fan-out: it owns the set of
//! operator subscription sinks and broadcasts proposals to them. It holds no
//! per-proposal state of its own — correlation, timeout, and first-wins
//! resolution all live in the CCH actor, driven by fire-and-forget messages.
//! When a new operator subscribes, the CCH actor streams the currently-pending
//! proposals from the store to that subscriber via [`replay_pending_proposals`]
//! (in a spawned task, off the mailbox), so a late operator still sees the
//! outstanding work.

use std::sync::atomic::{AtomicU64, Ordering};

use jsonrpsee::SubscriptionSink;
use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::cch::CchOrderStore;
use fiber_types::{Hash256, SwapProposal};

/// Reason recorded on an order whose proposal exceeds its timeout window.
pub const TIMEOUT_REASON: &str = "acceptor_timeout";

pub enum SwapAcceptorMessage {
    /// Register a new subscriber for proposal notifications. Replay of the
    /// already-pending proposals to this subscriber is handled by the CCH
    /// actor (which owns the store), so the acceptor only has to register the
    /// sink for future broadcasts.
    AddSink(SubscriptionSink),
    /// Broadcast a proposal to every connected subscriber. Fire-and-forget;
    /// the acceptor keeps no record of the proposal afterwards.
    Broadcast(Box<SwapProposal>),
    /// Test-only: report the current number of registered sinks so tests can
    /// assert that a proposal was fanned out.
    #[cfg(test)]
    TestSinkCount(ractor::RpcReplyPort<usize>),
}

#[derive(Default)]
pub struct SwapAcceptorState {
    sinks: Vec<SubscriptionSink>,
}

pub struct SwapAcceptorActor;

#[async_trait::async_trait]
impl Actor for SwapAcceptorActor {
    type Msg = SwapAcceptorMessage;
    type State = SwapAcceptorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(SwapAcceptorState::default())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SwapAcceptorMessage::AddSink(sink) => {
                state.sinks.push(sink);
            }
            SwapAcceptorMessage::Broadcast(proposal) => {
                let payload = match serde_json::value::to_raw_value(&*proposal) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::error!("failed to serialize swap proposal: {}", err);
                        return Ok(());
                    }
                };
                // Best-effort broadcast. Sinks that fail to receive are dropped,
                // matching the behaviour of `pubsub::PubSubServerActor`.
                let sinks = std::mem::take(&mut state.sinks);
                for sink in sinks {
                    if sink.send(payload.clone()).await.is_ok() {
                        state.sinks.push(sink);
                    }
                }
            }
            #[cfg(test)]
            SwapAcceptorMessage::TestSinkCount(reply) => {
                let _ = reply.send(state.sinks.len());
            }
        }
        Ok(())
    }
}

/// Replay the currently-pending proposals to a freshly-subscribed `sink`,
/// loading each full record from `store` one at a time so the whole set is
/// never held in memory at once (only the small list of keys is). Intended to
/// be run in a spawned task (the per-sink `send` is async and must not block
/// the CCH actor mailbox).
pub async fn replay_pending_proposals<S: CchOrderStore>(store: S, sink: SubscriptionSink) {
    // Snapshot just the keys (32 bytes each) up front; the iterator itself is
    // not `Send`, and collecting it lets the proposal bodies stay out of memory
    // until each one is streamed.
    let payment_hashes: Vec<Hash256> = store
        .get_cch_pending_proposal_keys_iter()
        .into_iter()
        .collect();
    for payment_hash in payment_hashes {
        let proposal = match store.get_cch_pending_proposal(&payment_hash) {
            Ok(proposal) => proposal,
            // Raced with resolution/timeout; skip it.
            Err(_) => continue,
        };
        let payload = match serde_json::value::to_raw_value(&proposal) {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(
                    "failed to serialize pending swap proposal {:x}: {}",
                    proposal.proposal_id,
                    err
                );
                continue;
            }
        };
        if sink.send(payload).await.is_err() {
            // Subscriber went away mid-replay; stop early.
            break;
        }
    }
}

/// Generate a fresh proposal id. We salt the payment hash with a per-process
/// monotonic counter so that two proposals built for the same payment hash
/// (e.g. a retry after rejection) are distinguishable on the operator side.
pub fn next_proposal_id(payment_hash: &Hash256) -> Hash256 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(payment_hash.as_ref());
    // Mix the counter into the trailing 8 bytes; preserves the linkage to the
    // payment hash for log correlation while ensuring uniqueness.
    for (i, b) in n.to_le_bytes().iter().enumerate() {
        bytes[24 + i] ^= *b;
    }
    Hash256::from(bytes)
}
