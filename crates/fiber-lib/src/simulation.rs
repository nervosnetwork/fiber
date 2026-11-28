//! Native test hooks used by the deterministic simulation harness.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fiber_types::{Hash256, Pubkey, TlcInfo};
use ractor::{ActorCell, ActorId};
use rand::{rngs::StdRng, SeedableRng};
use tokio::sync::{mpsc, oneshot};

use crate::fiber::types::FiberMessage;
use crate::store::Store;

pub use crate::fiber::network::{start_node, SimulatedNode};
pub use crate::store::store_impl::ChannelRecords;

thread_local! {
    static ENVIRONMENT: RefCell<Option<Environment>> = const { RefCell::new(None) };
}

struct Environment {
    seed: u64,
    random: StdRng,
    pauses: BTreeMap<(PausePoint, Pubkey, Hash256), Pause>,
    mailbox_delays: BTreeMap<ActorId, MailboxRule>,
    settlements: Option<Vec<TlcSettlement>>,
}

struct Pause {
    preceding_hits: usize,
    reached: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

/// Installs test entropy, clock and fault hooks for a complete simulation run.
/// Run all simulated hosts on this thread.
pub fn run<T>(seed: u64, clock: fn() -> Option<u64>, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ENVIRONMENT.take();
        }
    }

    ENVIRONMENT.with_borrow_mut(|environment| {
        assert!(environment.is_none(), "cannot nest Fiber simulations");
        *environment = Some(Environment {
            seed,
            random: StdRng::seed_from_u64(seed),
            pauses: BTreeMap::new(),
            mailbox_delays: BTreeMap::new(),
            settlements: None,
        });
    });
    let _reset = Reset;
    fiber_types::with_simulation_clock(clock, run)
}

pub(crate) fn is_running() -> bool {
    ENVIRONMENT.with_borrow(Option::is_some)
}

pub(crate) fn with_random<T>(f: impl FnOnce(&mut dyn rand::RngCore) -> T) -> T {
    ENVIRONMENT.with_borrow_mut(|environment| {
        f(&mut environment.as_mut().expect("active simulation").random)
    })
}

/// Fill `dest` from the active deterministic simulation RNG.
pub fn fill_random(dest: &mut [u8]) {
    with_random(|rng| rng.fill_bytes(dest));
}

/// Derive simulation entropy from a logical owner rather than task scheduling order.
pub(crate) fn keyed_random(domain: &[u8], context: &[u8]) -> [u8; 32] {
    ENVIRONMENT.with_borrow(|environment| {
        let seed = environment.as_ref().expect("active simulation").seed;
        let mut material = Vec::with_capacity(8 + domain.len() + context.len());
        material.extend_from_slice(&seed.to_le_bytes());
        material.extend_from_slice(domain);
        material.extend_from_slice(context);
        ckb_hash::blake2b_256(&material)
    })
}

/// Read the persisted channel records atomically and reject a partial pair.
pub fn channel_records(store: &Store, channel: &Hash256) -> Option<ChannelRecords> {
    store.simulation_channel_records(channel)
}

/// Compare a published cache entry against persisted bytes while the harness holds time still.
/// Returns `None` if there is no published cache entry.
pub fn cached_channel_matches_disk(store: &Store, channel: &Hash256) -> Option<bool> {
    store.simulation_cached_channel_matches_disk(channel)
}

/// Read the raw outpoint index, including entries for a missing channel.
pub fn indexed_funding_outpoints(
    store: &Store,
    channel: &Hash256,
) -> Vec<ckb_types::packed::OutPoint> {
    store.simulation_indexed_funding_outpoints(channel)
}

/// Verify that the raw pending payment-hash secondary indexes match persisted channel states.
pub fn verify_payment_hash_index(store: &Store) -> Result<(), String> {
    store.simulation_verify_payment_hash_index()
}

/// One endpoint's durable balance change from fulfilling a TLC.
#[derive(Clone, Debug)]
pub struct TlcSettlement {
    /// Node applying the balance change.
    pub node: Pubkey,
    /// Channel on which the TLC was committed.
    pub channel: Hash256,
    /// TLC ID scoped to the channel and offering endpoint.
    pub id: u64,
    /// Whether this endpoint offered the TLC.
    pub offered: bool,
    /// Payment that owns this TLC.
    pub payment: Hash256,
    /// Value transferred on this hop.
    pub amount: u128,
    /// Payer's attempt ID on the first outgoing hop.
    pub attempt: Option<u64>,
    /// Previous channel and received TLC ID on a forwarding node.
    pub previous: Option<(Hash256, u64)>,
}

/// Start collecting durable TLC fulfillments for the current simulation run.
pub fn observe_tlc_settlements() {
    ENVIRONMENT.with_borrow_mut(|environment| {
        let environment = environment.as_mut().expect("active simulation");
        assert!(environment.settlements.is_none());
        environment.settlements = Some(Vec::new());
    });
}

/// Drain fulfillments observed since the previous call, in persistence order.
pub fn take_tlc_settlements() -> Vec<TlcSettlement> {
    with_settlements(std::mem::take).unwrap_or_default()
}

fn with_settlements<T>(f: impl FnOnce(&mut Vec<TlcSettlement>) -> T) -> Option<T> {
    ENVIRONMENT.with_borrow_mut(|environment| environment.as_mut()?.settlements.as_mut().map(f))
}

// State clones used for persistence share this buffer. A successful write takes
// its entries once, and a crash discards entries that never reached a write.
pub(crate) type PendingTlcSettlements = Arc<Mutex<Vec<TlcSettlement>>>;

pub(crate) fn stage_tlc_settlement(
    pending: &mut Option<PendingTlcSettlements>,
    node: Pubkey,
    channel: Hash256,
    tlc: &TlcInfo,
) {
    if ENVIRONMENT.with_borrow(|env| env.as_ref().is_some_and(|env| env.settlements.is_some())) {
        pending
            .get_or_insert_with(Default::default)
            .lock()
            .expect("pending simulation settlements")
            .push(TlcSettlement {
                node,
                channel,
                id: tlc.id(),
                offered: tlc.is_offered(),
                payment: tlc.payment_hash,
                amount: tlc.amount,
                attempt: tlc.attempt_id,
                previous: tlc.forwarding_tlc,
            });
    }
}

pub(crate) fn publish_tlc_settlements(pending: Option<PendingTlcSettlements>) {
    if let Some(pending) = pending {
        with_settlements(|settlements| {
            settlements.append(&mut pending.lock().expect("pending simulation settlements"));
        });
    }
}

struct MailboxRule {
    delay: Duration,
    hits: Rc<Cell<u64>>,
}

/// A per-message delay applied to one live simulated actor.
/// Dropping the handle clears the rule for subsequent messages.
pub struct MailboxDelay {
    actor: ActorId,
    hits: Rc<Cell<u64>>,
}

impl MailboxDelay {
    /// Number of messages that started waiting on this rule.
    pub fn hits(&self) -> u64 {
        self.hits.get()
    }
}

impl Drop for MailboxDelay {
    fn drop(&mut self) {
        ENVIRONMENT.with_borrow_mut(|environment| {
            if let Some(environment) = environment {
                environment.mailbox_delays.remove(&self.actor);
            }
        });
    }
}

/// Slow ordinary message handling for an exact Fiber actor in this simulation.
/// A replacement actor after a crash has a fresh mailbox and must be armed separately.
pub fn slow_mailbox(actor: &ActorCell, delay: Duration) -> MailboxDelay {
    assert!(!delay.is_zero(), "mailbox delay must be positive");
    let actor = actor.get_id();
    let hits = Rc::new(Cell::new(0));
    ENVIRONMENT.with_borrow_mut(|environment| {
        let delays = &mut environment
            .as_mut()
            .expect("active simulation")
            .mailbox_delays;
        assert!(!delays.contains_key(&actor), "mailbox delay already armed");
        delays.insert(
            actor,
            MailboxRule {
                delay,
                hits: hits.clone(),
            },
        );
    });
    MailboxDelay { actor, hits }
}

pub(crate) async fn before_actor_message(actor: ActorId) {
    let delay = ENVIRONMENT.with_borrow(|environment| {
        let rule = environment.as_ref()?.mailbox_delays.get(&actor)?;
        rule.hits.set(rule.hits.get() + 1);
        Some(rule.delay)
    });
    if let Some(delay) = delay {
        // The handler owns this message while subsequent sends queue up in Ractor.
        tokio::time::sleep(delay).await;
    }
}

/// A bounded simulation transport connected directly to the Fiber send path.
#[derive(Clone, Debug)]
pub struct SimulatedPeerSender {
    sender: mpsc::Sender<FiberMessage>,
    blocked: Arc<AtomicU64>,
}

impl SimulatedPeerSender {
    /// Attach a bounded writer queue and count times a sender waits for capacity.
    pub fn new(sender: mpsc::Sender<FiberMessage>, blocked: Arc<AtomicU64>) -> Self {
        Self { sender, blocked }
    }

    pub(crate) async fn send(
        &self,
        message: FiberMessage,
    ) -> Result<(), mpsc::error::SendError<FiberMessage>> {
        match self.sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(message)) => {
                self.blocked.fetch_add(1, Ordering::Relaxed);
                self.sender.send(message).await
            }
            Err(mpsc::error::TrySendError::Closed(message)) => Err(mpsc::error::SendError(message)),
        }
    }
}

/// Minimal fault-injection hook invoked before `MockCkbChainClient` requests.
#[async_trait::async_trait]
pub trait SimulatedChainClientHook: Send + Sync + std::fmt::Debug {
    /// Called before executing a simulated CKB client RPC request.
    async fn before_client_request(&self, request: &'static str) -> anyhow::Result<()>;
}

/// An armed pause in the simulated channel or payment data plane.
pub struct CommitmentPause {
    signal: oneshot::Receiver<()>,
    reached: bool,
    _resume_on_drop: oneshot::Sender<()>,
}

impl CommitmentPause {
    /// Returns whether the channel actor has reached the pause.
    pub fn reached(&mut self) -> bool {
        self.reached |= self.signal.try_recv().is_ok();
        self.reached
    }
}

/// Pause one channel on one node at the durable commitment boundary.
/// Dropping the handle resumes the channel if its runtime is still running.
pub fn pause_before_commitment_ack(
    node: Pubkey,
    channel: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(PausePoint::CommitmentAck, node, channel, preceding_hits)
}

/// Pause a relay after it persists downstream fulfillment and before sending it upstream.
pub fn pause_before_forwarding_fulfill(
    node: Pubkey,
    downstream: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(PausePoint::ForwardFulfill, node, downstream, preceding_hits)
}

/// Pause a funding channel after a resulting state is durably written.
pub fn pause_after_funding_state(
    node: Pubkey,
    channel: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(PausePoint::FundingPersisted, node, channel, preceding_hits)
}

/// Pause after a durable channel ID rewrite before notifying the network actor.
pub fn pause_after_channel_id_rewrite(
    node: Pubkey,
    old_id: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(PausePoint::ChannelIdRewritten, node, old_id, preceding_hits)
}

/// Pause after `receive_ack_v2` persists `removed_confirmed_at` and before relaying removal effects.
pub fn pause_after_remove_confirmed(
    node: Pubkey,
    channel: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(
        PausePoint::RemoveConfirmedBeforeRelay,
        node,
        channel,
        preceding_hits,
    )
}

/// Pause after `receive_commitment_v2` persists a peer `RemoveTlcFail` and before sending ACK.
pub fn pause_after_peer_fail_recorded(
    node: Pubkey,
    channel: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(
        PausePoint::PeerFailRecordedBeforeAck,
        node,
        channel,
        preceding_hits,
    )
}

/// Pause after `SettleTlcSet` marks an immediate invoice `Received` and before sending `RemoveTlc(Fulfill)`.
pub fn pause_after_invoice_marked_received(
    node: Pubkey,
    payment_hash: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    arm(
        PausePoint::InvoiceMarkedReceived,
        node,
        payment_hash,
        preceding_hits,
    )
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PausePoint {
    CommitmentAck,
    ForwardFulfill,
    FundingPersisted,
    ChannelIdRewritten,
    RemoveConfirmedBeforeRelay,
    PeerFailRecordedBeforeAck,
    InvoiceMarkedReceived,
}

fn arm(
    point: PausePoint,
    node: Pubkey,
    channel: Hash256,
    preceding_hits: usize,
) -> CommitmentPause {
    let (reached, wait) = oneshot::channel();
    let (resume, paused) = oneshot::channel();
    ENVIRONMENT.with_borrow_mut(|environment| {
        let pauses = &mut environment.as_mut().expect("active simulation").pauses;
        let key = (point, node, channel);
        assert!(
            pauses
                .get(&key)
                .is_none_or(|pause| pause.reached.is_closed()),
            "commitment pause already armed"
        );
        pauses.insert(
            key,
            Pause {
                preceding_hits,
                reached,
                resume: paused,
            },
        );
    });
    CommitmentPause {
        signal: wait,
        reached: false,
        _resume_on_drop: resume,
    }
}

pub(crate) async fn after_commit_before_ack(node: Pubkey, channel: Hash256) {
    pause_at(PausePoint::CommitmentAck, node, channel).await;
}

pub(crate) async fn before_forwarding_fulfill(node: Pubkey, channel: Hash256) {
    pause_at(PausePoint::ForwardFulfill, node, channel).await;
}

pub(crate) async fn after_funding_state(node: Pubkey, channel: Hash256) {
    pause_at(PausePoint::FundingPersisted, node, channel).await;
}

pub(crate) async fn after_channel_id_rewrite(node: Pubkey, old_id: Hash256) {
    pause_at(PausePoint::ChannelIdRewritten, node, old_id).await;
}

pub(crate) async fn after_remove_confirmed_before_relay(node: Pubkey, channel: Hash256) {
    pause_at(PausePoint::RemoveConfirmedBeforeRelay, node, channel).await;
}

pub(crate) async fn after_peer_fail_recorded(node: Pubkey, channel: Hash256) {
    pause_at(PausePoint::PeerFailRecordedBeforeAck, node, channel).await;
}

pub(crate) async fn after_invoice_marked_received(node: Pubkey, payment_hash: Hash256) {
    pause_at(PausePoint::InvoiceMarkedReceived, node, payment_hash).await;
}

async fn pause_at(point: PausePoint, node: Pubkey, channel: Hash256) {
    let pause = ENVIRONMENT.with_borrow_mut(|environment| {
        let pauses = &mut environment.as_mut()?.pauses;
        let key = (point, node, channel);
        let pause = pauses.get_mut(&key)?;
        if pause.reached.is_closed() {
            pauses.remove(&key);
            return None;
        }
        if pause.preceding_hits > 0 {
            pause.preceding_hits -= 1;
            return None;
        }
        pauses.remove(&key)
    });
    if let Some(pause) = pause {
        let _ = pause.reached.send(());
        let _ = pause.resume.await;
    }
}
