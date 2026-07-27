//! LND Payment and Invoice Tracker Actor
//!
//! This module implements `LndTrackerActor`, which manages concurrent tracking of
//! Lightning Network invoices and payments via LND (Lightning Network Daemon).
//!
//! ## Key Features
//!
//! - **Bounded Tracking**: Tracks every admitted invoice and outgoing payment up to separate
//!   global limits.
//! - **Connection Reuse**: Multiplexes invoice subscriptions over one shared LND client.
//! - **Reconnect Protection**: Bounds concurrent subscription attempts and adds retry jitter.
//! - **Completion Handling**: Properly cleans up when tracker tasks complete, stop, or fail.
//!
//! ## Architecture
//!
//! Tracking requests are submitted through `TrackInvoice(Hash256)` and
//! `TrackPayment(Hash256)`. Spawned tracker tasks report completion back to the actor.
//!
//! The queue is only needed when restoring more persisted orders than the current global
//! admission limit. Newly created orders reserve global capacity before their LND invoice is
//! created, so every newly admitted invoice starts tracking immediately.
//! Outgoing payments use the same admission model: `send_btc` reserves capacity before exposing
//! its payable Fiber invoice, while restored orders above the limit wait in a bounded-concurrency
//! queue.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef, OutputPort, RpcReplyPort};
use tokio::{
    sync::Semaphore,
    time::{sleep, timeout},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{cch::trackers::CchTrackingEvent, invoice::CkbInvoiceStatus};
use fiber_types::payment::PaymentStatus as FiberPaymentStatus;
use fiber_types::Hash256;

pub(crate) const MAX_TRACKED_INVOICES: usize = 100;
pub(crate) const MAX_TRACKED_PAYMENTS: usize = 100;
const MAX_CONCURRENT_SUBSCRIBE_ATTEMPTS: usize = 10;
const SUBSCRIBE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
const INVOICE_TRACKER_RETRY_DELAY: Duration = Duration::from_secs(15);
const INVOICE_TRACKER_RETRY_JITTER: Duration = Duration::from_secs(15);

/// LND connection information
///
/// This struct contains the connection details for communicating with an LND node.
#[derive(Clone)]
pub struct LndConnectionInfo {
    pub uri: Uri,
    pub cert: Option<Vec<u8>>,
    pub macaroon: Option<Vec<u8>>,
}

impl LndConnectionInfo {
    pub fn new(uri: Uri, cert: Option<Vec<u8>>, macaroon: Option<Vec<u8>>) -> Self {
        Self {
            uri,
            cert,
            macaroon,
        }
    }

    pub async fn create_router_client(
        &self,
    ) -> Result<RouterClient, lnd_grpc_tonic_client::channel::Error> {
        create_router_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_deref(),
        )
        .await
    }

    pub async fn create_invoices_client(
        &self,
    ) -> Result<InvoicesClient, lnd_grpc_tonic_client::channel::Error> {
        create_invoices_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_deref(),
        )
        .await
    }
}

/// Message types for the LndTrackerActor
#[derive(Debug)]
pub enum LndTrackerMessage {
    /// Reserve global capacity before creating an externally payable LND invoice.
    ReserveInvoiceTracking(Hash256, RpcReplyPort<InvoiceTrackingReservationResult>),

    /// Track a new invoice
    TrackInvoice(Hash256),

    /// Stop tracking an invoice (remove from queue if not yet being tracked)
    StopTracking(Hash256),

    /// Reserve global capacity before exposing a payable Fiber invoice for `send_btc`.
    ReservePaymentTracking(Hash256, RpcReplyPort<PaymentTrackingReservationResult>),

    /// Restore a persisted payment-tracking order created before admission control.
    RestorePaymentTracking(Hash256),

    /// Track an outgoing payment by hash until LND reports a terminal status.
    TrackPayment(Hash256),

    /// Stop tracking an outgoing payment.
    StopTrackingPayment(Hash256),

    /// Notification that an invoice tracker task has completed
    ///
    /// Sent by InvoiceTracker tasks when they terminate (either successfully
    /// when invoice reaches final state, or due to error).
    InvoiceTrackerCompleted {
        payment_hash: Hash256,
        completed_successfully: bool,
    },

    /// Notification that a per-payment tracker task has terminated.
    PaymentTrackerCompleted {
        payment_hash: Hash256,
        tracker_id: u64,
    },

    /// Get current state snapshot (for testing)
    #[cfg(test)]
    GetState(ractor::RpcReplyPort<StateSnapshot>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceTrackingReservationResult {
    Reserved,
    AlreadyTracked,
    CapacityExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentTrackingReservationResult {
    Reserved,
    AlreadyTracked,
    CapacityExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvoiceTrackingState {
    Reserved,
    Queued,
    Active,
    Stopping,
}

/// Snapshot of actor state (for testing)
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub invoice_queue_len: usize,
    pub active_invoice_trackers: usize,
    pub reserved_invoice_trackers: usize,
    pub stopping_invoice_trackers: usize,
    pub tracked_invoices: usize,
    pub payment_queue_len: usize,
    pub active_payment_trackers: usize,
    pub reserved_payment_trackers: usize,
    pub tracked_payment_count: usize,
}

/// Arguments for starting the LndTrackerActor
pub struct LndTrackerArgs {
    pub port: Arc<OutputPort<CchTrackingEvent>>,
    pub lnd_connection: LndConnectionInfo,
    pub token: CancellationToken,
    pub tracker: TaskTracker,
}

/// State for the LndTrackerActor
pub struct LndTrackerState {
    port: Arc<OutputPort<CchTrackingEvent>>,
    lnd_connection: LndConnectionInfo,
    invoices_client: InvoicesClient,
    token: CancellationToken,
    tracker: TaskTracker,
    /// Queue of payment hashes waiting to be tracked
    invoice_queue: VecDeque<Hash256>,
    /// State of every admitted invoice tracker. The map size is the global capacity usage.
    invoice_trackers: HashMap<Hash256, InvoiceTrackingState>,
    /// Cancellation token for each active or stopping invoice tracker.
    active_invoice_tracker_tokens: HashMap<Hash256, CancellationToken>,
    /// Invoices requested again while their previous tracker is stopping.
    restart_stopping_invoices: HashSet<Hash256>,
    /// Limits only subscription establishment. Permits are released once streams are established.
    subscribe_attempts: Arc<Semaphore>,
    /// Payment hashes waiting for a bounded tracking slot.
    payment_queue: VecDeque<Hash256>,
    /// State of every admitted or restored outgoing Lightning payment.
    payment_trackers: HashMap<Hash256, PaymentTrackingState>,
    next_payment_tracker_id: u64,
}

struct ActivePaymentTracker {
    tracker_id: u64,
    token: CancellationToken,
}

enum PaymentTrackingState {
    Reserved,
    Queued,
    Active(ActivePaymentTracker),
}

/// Ractor Actor to track LND payments and invoices
///
/// This actor manages tracking of Lightning Network Daemon (LND) payments and invoices.
/// It provides the following features:
///
/// ## Payment Tracking
/// - Reserves capacity before admitting new `send_btc` orders
/// - Bounds concurrently active per-payment trackers while preserving restart recovery
/// - Deduplicates trackers and reconnects until LND reports a terminal status
/// - Sends `CchTrackingEvent::PaymentChanged` events to the output port
///
/// ## Invoice Tracking
/// - Supports tracking individual invoices via `LndTrackerMessage::TrackInvoice`
/// - Tracks all invoices admitted by the global invoice limit
/// - Multiplexes subscriptions over a shared LND HTTP/2 client
/// - Limits concurrent subscription attempts to protect LND during startup and reconnects
///
/// ## Example Usage
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use ractor::{ActorCell, OutputPort};
/// use tokio_util::{sync::CancellationToken, task::TaskTracker};
///
/// // Create output port for events
/// let port = Arc::new(OutputPort::<CchTrackingEvent>::default());
///
/// // Create connection info
/// let lnd_connection = LndConnectionInfo::new(
///     "https://localhost:10009".parse().unwrap(),
///     Some(cert_bytes),
///     Some(macaroon_bytes),
/// );
///
/// // Start the actor
/// let args = LndTrackerArgs {
///     port: port.clone(),
///     lnd_connection,
///     token: CancellationToken::new(),
///     tracker: TaskTracker::new(),
/// };
///
/// let actor = LndTrackerActor::start(args, root_actor).await?;
///
/// // Track an invoice
/// actor.cast(LndTrackerMessage::TrackInvoice(payment_hash))?;
/// ```
#[derive(Default)]
pub struct LndTrackerActor;

impl LndTrackerActor {
    pub async fn start(
        args: LndTrackerArgs,
        root_actor: ActorCell,
    ) -> Result<ActorRef<LndTrackerMessage>> {
        // Use None for actor name to allow multiple instances (e.g., in tests)
        let (actor, _handle) = Actor::spawn_linked(None, LndTrackerActor, args, root_actor).await?;
        Ok(actor)
    }
}

#[async_trait::async_trait]
impl Actor for LndTrackerActor {
    type Msg = LndTrackerMessage;
    type State = LndTrackerState;
    type Arguments = LndTrackerArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let invoices_client = args.lnd_connection.create_invoices_client().await?;
        let state = LndTrackerState {
            port: args.port,
            lnd_connection: args.lnd_connection,
            invoices_client,
            token: args.token,
            tracker: args.tracker,
            invoice_queue: VecDeque::new(),
            invoice_trackers: HashMap::new(),
            active_invoice_tracker_tokens: HashMap::new(),
            restart_stopping_invoices: HashSet::new(),
            subscribe_attempts: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBSCRIBE_ATTEMPTS)),
            payment_queue: VecDeque::new(),
            payment_trackers: HashMap::new(),
            next_payment_tracker_id: 0,
        };

        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let res = match message {
            LndTrackerMessage::ReserveInvoiceTracking(payment_hash, reply_port) => {
                let result = if state.invoice_trackers.contains_key(&payment_hash) {
                    InvoiceTrackingReservationResult::AlreadyTracked
                } else if state.invoice_trackers.len() >= MAX_TRACKED_INVOICES {
                    InvoiceTrackingReservationResult::CapacityExceeded
                } else {
                    state
                        .invoice_trackers
                        .insert(payment_hash, InvoiceTrackingState::Reserved);
                    InvoiceTrackingReservationResult::Reserved
                };
                let _ = reply_port.send(result);
                Ok(())
            }
            LndTrackerMessage::TrackInvoice(payment_hash) => {
                let should_process_queue = match state.invoice_trackers.get_mut(&payment_hash) {
                    Some(tracker_state @ InvoiceTrackingState::Reserved) => {
                        *tracker_state = InvoiceTrackingState::Queued;
                        state.invoice_queue.push_back(payment_hash);
                        true
                    }
                    Some(InvoiceTrackingState::Stopping) => {
                        state.restart_stopping_invoices.insert(payment_hash);
                        tracing::debug!(
                            "Will restart invoice tracker {:x} after its previous task stops",
                            payment_hash
                        );
                        false
                    }
                    Some(_) => {
                        tracing::debug!("Invoice {:x} is already being tracked", payment_hash);
                        false
                    }
                    None => {
                        state
                            .invoice_trackers
                            .insert(payment_hash, InvoiceTrackingState::Queued);
                        if state.invoice_trackers.len() > MAX_TRACKED_INVOICES {
                            // Existing persisted orders must still be restored after an upgrade even
                            // if they predate the admission limit. New orders reserve capacity before
                            // creating their LND invoice and cannot take this path.
                            tracing::warn!(
                                "Restoring invoice tracker {:x} above the global limit of {}",
                                payment_hash,
                                MAX_TRACKED_INVOICES
                            );
                        }
                        state.invoice_queue.push_back(payment_hash);
                        true
                    }
                };
                if should_process_queue {
                    state.process_invoice_queue(myself).await?;
                }
                Ok(())
            }
            LndTrackerMessage::StopTracking(payment_hash) => {
                match state.invoice_trackers.get(&payment_hash).copied() {
                    Some(InvoiceTrackingState::Active) => {
                        if let Some(token) = state.active_invoice_tracker_tokens.get(&payment_hash)
                        {
                            token.cancel();
                        }
                        state
                            .invoice_trackers
                            .insert(payment_hash, InvoiceTrackingState::Stopping);
                    }
                    Some(InvoiceTrackingState::Stopping) => {
                        state.restart_stopping_invoices.remove(&payment_hash);
                    }
                    Some(InvoiceTrackingState::Reserved | InvoiceTrackingState::Queued) => {
                        state.invoice_trackers.remove(&payment_hash);
                        state.invoice_queue.retain(|&hash| hash != payment_hash);
                    }
                    None => {}
                }
                tracing::debug!("Stopped tracking invoice {:x}", payment_hash);
                Ok(())
            }
            LndTrackerMessage::ReservePaymentTracking(payment_hash, reply_port) => {
                let result = state.reserve_payment_tracking(payment_hash);
                let _ = reply_port.send(result);
                Ok(())
            }
            LndTrackerMessage::RestorePaymentTracking(payment_hash) => {
                state.restore_payment_tracking(payment_hash);
                Ok(())
            }
            LndTrackerMessage::TrackPayment(payment_hash) => {
                state.queue_payment_tracker(myself, payment_hash);
                Ok(())
            }
            LndTrackerMessage::StopTrackingPayment(payment_hash) => {
                state.stop_payment_tracker(payment_hash);
                Ok(())
            }
            LndTrackerMessage::InvoiceTrackerCompleted {
                payment_hash,
                completed_successfully,
            } => {
                tracing::debug!(
                    "Processing completion for payment_hash={}, success={}, active={}/{}",
                    payment_hash,
                    completed_successfully,
                    state.active_invoice_trackers(),
                    MAX_TRACKED_INVOICES
                );
                state.active_invoice_tracker_tokens.remove(&payment_hash);
                match state.invoice_trackers.get(&payment_hash).copied() {
                    Some(InvoiceTrackingState::Active)
                        if !completed_successfully && !state.token.is_cancelled() =>
                    {
                        state
                            .invoice_trackers
                            .insert(payment_hash, InvoiceTrackingState::Queued);
                        state.invoice_queue.push_back(payment_hash);
                    }
                    Some(InvoiceTrackingState::Stopping)
                        if state.restart_stopping_invoices.remove(&payment_hash) =>
                    {
                        state
                            .invoice_trackers
                            .insert(payment_hash, InvoiceTrackingState::Queued);
                        state.invoice_queue.push_back(payment_hash);
                    }
                    Some(InvoiceTrackingState::Active | InvoiceTrackingState::Stopping) => {
                        state.restart_stopping_invoices.remove(&payment_hash);
                        state.invoice_trackers.remove(&payment_hash);
                    }
                    Some(InvoiceTrackingState::Reserved | InvoiceTrackingState::Queued) | None => {
                        tracing::warn!(
                            "Ignoring stale completion for inactive invoice tracker {:x}",
                            payment_hash
                        );
                        return Ok(());
                    }
                }

                // Now that a slot is free, we can start tracking more invoices from the queue
                state.process_invoice_queue(myself).await?;

                Ok(())
            }
            LndTrackerMessage::PaymentTrackerCompleted {
                payment_hash,
                tracker_id,
            } => {
                state.complete_payment_tracker(myself, payment_hash, tracker_id);
                Ok(())
            }

            #[cfg(test)]
            LndTrackerMessage::GetState(reply_port) => {
                let snapshot = StateSnapshot {
                    invoice_queue_len: state.invoice_queue.len(),
                    active_invoice_trackers: state.active_invoice_trackers(),
                    reserved_invoice_trackers: state
                        .count_invoice_trackers(InvoiceTrackingState::Reserved),
                    stopping_invoice_trackers: state
                        .count_invoice_trackers(InvoiceTrackingState::Stopping),
                    tracked_invoices: state.invoice_trackers.len(),
                    payment_queue_len: state.payment_queue.len(),
                    active_payment_trackers: state.active_payment_tracker_count(),
                    reserved_payment_trackers: state.reserved_payment_tracker_count(),
                    tracked_payment_count: state.payment_trackers.len(),
                };
                let _ = reply_port.send(snapshot);
                Ok(())
            }
        };

        // update metrics
        #[cfg(feature = "metrics")]
        {
            metrics::gauge!(crate::metrics::CCH_LND_TRACKER_INVOICE_QUEUE_LEN)
                .set(state.invoice_queue.len() as f64);
            metrics::gauge!(crate::metrics::CCH_LND_TRACKER_ACTIVE_INVOICE_TRACKERS)
                .set(state.active_invoice_trackers() as f64);
        }

        res
    }
}

impl LndTrackerState {
    fn reserve_payment_tracking(
        &mut self,
        payment_hash: Hash256,
    ) -> PaymentTrackingReservationResult {
        if self.payment_trackers.contains_key(&payment_hash) {
            return PaymentTrackingReservationResult::AlreadyTracked;
        }
        if self.payment_trackers.len() >= MAX_TRACKED_PAYMENTS {
            return PaymentTrackingReservationResult::CapacityExceeded;
        }
        self.payment_trackers
            .insert(payment_hash, PaymentTrackingState::Reserved);
        PaymentTrackingReservationResult::Reserved
    }

    fn restore_payment_tracking(&mut self, payment_hash: Hash256) {
        if self.payment_trackers.contains_key(&payment_hash) {
            return;
        }
        self.payment_trackers
            .insert(payment_hash, PaymentTrackingState::Reserved);
        if self.payment_trackers.len() > MAX_TRACKED_PAYMENTS {
            tracing::warn!(
                "Restored outgoing payment tracker {} above the global limit of {}",
                payment_hash,
                MAX_TRACKED_PAYMENTS
            );
        }
    }

    fn queue_payment_tracker(
        &mut self,
        myself: ActorRef<LndTrackerMessage>,
        payment_hash: Hash256,
    ) {
        match self.payment_trackers.get_mut(&payment_hash) {
            Some(PaymentTrackingState::Reserved) => {
                self.payment_trackers
                    .insert(payment_hash, PaymentTrackingState::Queued);
                self.payment_queue.push_back(payment_hash);
            }
            Some(PaymentTrackingState::Queued) | Some(PaymentTrackingState::Active(_)) => {
                tracing::debug!(
                    "Outgoing payment tracker already queued or active for payment_hash={}",
                    payment_hash
                );
                return;
            }
            None => {
                // Persisted orders created before admission control must still recover.
                self.payment_trackers
                    .insert(payment_hash, PaymentTrackingState::Queued);
                if self.payment_trackers.len() > MAX_TRACKED_PAYMENTS {
                    tracing::warn!(
                        "Restoring outgoing payment tracker {} above the global limit of {}",
                        payment_hash,
                        MAX_TRACKED_PAYMENTS
                    );
                }
                self.payment_queue.push_back(payment_hash);
            }
        }

        self.process_payment_queue(myself);
    }

    fn process_payment_queue(&mut self, myself: ActorRef<LndTrackerMessage>) {
        while self.active_payment_tracker_count() < MAX_TRACKED_PAYMENTS {
            let Some(payment_hash) = self.payment_queue.pop_front() else {
                break;
            };
            if !matches!(
                self.payment_trackers.get(&payment_hash),
                Some(PaymentTrackingState::Queued)
            ) {
                continue;
            }

            let tracker_id = self.next_payment_tracker_id;
            self.next_payment_tracker_id = self.next_payment_tracker_id.wrapping_add(1);
            let token = self.token.child_token();
            self.payment_trackers.insert(
                payment_hash,
                PaymentTrackingState::Active(ActivePaymentTracker {
                    tracker_id,
                    token: token.clone(),
                }),
            );

            let tracker = PaymentTracker {
                port: self.port.clone(),
                payment_hash,
                lnd_connection: self.lnd_connection.clone(),
                token,
            };
            let myself = myself.clone();
            self.tracker.spawn(async move {
                tracker.run().await;
                let _ = myself.send_message(LndTrackerMessage::PaymentTrackerCompleted {
                    payment_hash,
                    tracker_id,
                });
            });
            tracing::debug!(
                "Started outgoing payment tracker for payment_hash={} tracker_id={} active={}/{}",
                payment_hash,
                tracker_id,
                self.active_payment_tracker_count(),
                MAX_TRACKED_PAYMENTS
            );
        }
    }

    fn stop_payment_tracker(&mut self, payment_hash: Hash256) {
        let remove_immediately = match self.payment_trackers.get_mut(&payment_hash) {
            Some(PaymentTrackingState::Reserved | PaymentTrackingState::Queued) => true,
            Some(PaymentTrackingState::Active(active_tracker)) => {
                active_tracker.token.cancel();
                tracing::debug!(
                    "Stopping outgoing payment tracker for payment_hash={} tracker_id={}",
                    payment_hash,
                    active_tracker.tracker_id
                );
                false
            }
            None => false,
        };
        if remove_immediately {
            self.payment_trackers.remove(&payment_hash);
            self.payment_queue.retain(|hash| *hash != payment_hash);
            tracing::debug!(
                "Released outgoing payment tracking reservation for payment_hash={}",
                payment_hash
            );
        }
    }

    fn complete_payment_tracker(
        &mut self,
        myself: ActorRef<LndTrackerMessage>,
        payment_hash: Hash256,
        tracker_id: u64,
    ) {
        let is_current_tracker = self
            .payment_trackers
            .get(&payment_hash)
            .is_some_and(|state| {
                matches!(
                    state,
                    PaymentTrackingState::Active(tracker) if tracker.tracker_id == tracker_id
                )
            });
        if is_current_tracker {
            self.payment_trackers.remove(&payment_hash);
            tracing::debug!(
                "Completed outgoing payment tracker for payment_hash={} tracker_id={}",
                payment_hash,
                tracker_id
            );
            self.process_payment_queue(myself);
        }
    }

    fn active_payment_tracker_count(&self) -> usize {
        self.payment_trackers
            .values()
            .filter(|state| matches!(state, PaymentTrackingState::Active(_)))
            .count()
    }

    #[cfg(test)]
    fn reserved_payment_tracker_count(&self) -> usize {
        self.payment_trackers
            .values()
            .filter(|state| matches!(state, PaymentTrackingState::Reserved))
            .count()
    }

    #[cfg(test)]
    fn count_invoice_trackers(&self, expected_state: InvoiceTrackingState) -> usize {
        self.invoice_trackers
            .values()
            .filter(|&&state| state == expected_state)
            .count()
    }

    fn active_invoice_trackers(&self) -> usize {
        self.invoice_trackers
            .values()
            .filter(|&&state| {
                matches!(
                    state,
                    InvoiceTrackingState::Active | InvoiceTrackingState::Stopping
                )
            })
            .count()
    }

    async fn process_invoice_queue(
        &mut self,
        myself: ActorRef<LndTrackerMessage>,
    ) -> Result<(), ActorProcessingErr> {
        // New orders are globally admitted before invoice creation, so they start immediately.
        // The bound is still enforced here because an upgrade can restore more persisted orders
        // than the current admission limit.
        while self.active_invoice_trackers() < MAX_TRACKED_INVOICES {
            let Some(payment_hash) = self.invoice_queue.pop_front() else {
                break;
            };
            match self.invoice_trackers.get_mut(&payment_hash) {
                Some(tracker_state @ InvoiceTrackingState::Queued) => {
                    *tracker_state = InvoiceTrackingState::Active;
                }
                _ => {
                    tracing::warn!(
                        "Skipping invoice {:x} that is queued with an invalid state",
                        payment_hash
                    );
                    continue;
                }
            }

            let token = self.token.child_token();
            self.active_invoice_tracker_tokens
                .insert(payment_hash, token.clone());
            let tracker = InvoiceTracker {
                port: self.port.clone(),
                invoices_client: self.invoices_client.clone(),
                token,
                subscribe_attempts: self.subscribe_attempts.clone(),
                payment_hash,
            };

            // ALWAYS send InvoiceTrackerCompleted message back to actor
            // - This ensures we decrement counter and remove from queue
            // - Even on error, the tracker has quit, so we must clean up
            let myself_clone = myself.clone();
            self.tracker.spawn(async move {
                let completed_successfully = tracker.run().await;
                myself_clone
                    .cast(LndTrackerMessage::InvoiceTrackerCompleted {
                        payment_hash,
                        completed_successfully,
                    })
                    .expect("cast LndTrackerMessage");
            });

            tracing::debug!(
                "Started invoice tracker for payment_hash={}, active={}/{}",
                payment_hash,
                self.active_invoice_trackers(),
                MAX_TRACKED_INVOICES
            );
        }

        Ok(())
    }
}

/// Tracks one outgoing payment by hash. `TrackPaymentV2` returns the current state
/// after every reconnect, including a terminal state reached while CCH was unavailable.
struct PaymentTracker {
    port: Arc<OutputPort<CchTrackingEvent>>,
    payment_hash: Hash256,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl PaymentTracker {
    async fn run(self) {
        let token = self.token.clone();
        let fut = self.run_loop();
        token.run_until_cancelled(fut).await;
    }

    async fn run_loop(&self) {
        while let Err(err) = self.run_once().await {
            tracing::error!(
                "Error tracking outgoing LND payment {}, retry 15 seconds later: {:?}",
                self.payment_hash,
                err
            );
            sleep(Duration::from_secs(15)).await;
        }
        tracing::debug!(
            "Outgoing payment tracker completed for payment_hash={}",
            self.payment_hash
        );
    }

    async fn run_once(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_router_client().await?;
        let mut stream = client
            .track_payment_v2(track_payment_request(self.payment_hash))
            .await?
            .into_inner();

        loop {
            match stream.next().await {
                Some(Ok(payment)) => {
                    if self.on_payment(payment).await? {
                        return Ok(());
                    }
                }
                Some(Err(err)) => return Err(err.into()),
                None => return Err(anyhow!("unexpected closed stream")),
            }
        }
    }

    /// Emits every update and returns true once the payment is terminal.
    async fn on_payment(&self, payment: lnrpc::Payment) -> Result<bool> {
        let payment_hash = payment.payment_hash.clone();
        let status = payment.status();
        let is_terminal = is_lnd_payment_terminal(status);
        let has_payment_preimage = !is_payment_preimage_empty(&payment.payment_preimage, status);
        tracing::debug!(
            "payment changed payment_hash={} status={:?} has_payment_preimage={}",
            payment_hash,
            status,
            has_payment_preimage
        );
        self.port.send(map_lnd_payment_changed_event(payment)?);
        Ok(is_terminal)
    }
}

fn track_payment_request(payment_hash: Hash256) -> routerrpc::TrackPaymentRequest {
    routerrpc::TrackPaymentRequest {
        payment_hash: payment_hash.into(),
        no_inflight_updates: false,
    }
}

fn is_lnd_payment_terminal(status: lnrpc::payment::PaymentStatus) -> bool {
    matches!(
        status,
        lnrpc::payment::PaymentStatus::Succeeded | lnrpc::payment::PaymentStatus::Failed
    )
}

/// Internal struct for tracking individual invoices
struct InvoiceTracker {
    port: Arc<OutputPort<CchTrackingEvent>>,
    payment_hash: Hash256,
    invoices_client: InvoicesClient,
    token: CancellationToken,
    subscribe_attempts: Arc<Semaphore>,
}

impl InvoiceTracker {
    /// Return true if the tracker completed successfully
    async fn run(self) -> bool {
        let token = self.token.clone();
        let fut = self.run_loop();
        token.run_until_cancelled(fut).await.is_some()
    }

    async fn run_loop(&self) {
        while let Err(err) = self.run_once().await {
            let retry_delay = invoice_tracker_retry_delay();
            tracing::error!(
                "Error tracking LND invoice {}, retry {:?} later: {:?}",
                self.payment_hash,
                retry_delay,
                err
            );
            sleep(retry_delay).await;
        }
        tracing::debug!(
            "InvoiceTracker completed successfully for payment_hash={}",
            self.payment_hash
        );
    }

    async fn run_once(&self) -> Result<()> {
        let permit = self
            .subscribe_attempts
            .acquire()
            .await
            .map_err(|_| anyhow!("invoice subscription limiter closed"))?;
        let mut client = self.invoices_client.clone();
        let response = timeout(
            SUBSCRIBE_ATTEMPT_TIMEOUT,
            client.subscribe_single_invoice(invoicesrpc::SubscribeSingleInvoiceRequest {
                r_hash: self.payment_hash.into(),
            }),
        )
        .await
        .map_err(|_| anyhow!("invoice subscription attempt timed out"))??;
        drop(permit);
        let mut stream = response.into_inner();

        loop {
            match stream.next().await {
                Some(Ok(invoice)) => {
                    if self.on_invoice(invoice).await? {
                        return Ok(());
                    }
                }
                Some(Err(err)) => return Err(err.into()),
                None => return Err(anyhow!("unexpected closed stream")),
            }
        }
    }

    // Return true to quit the tracker
    async fn on_invoice(&self, invoice: lnrpc::Invoice) -> Result<bool> {
        tracing::debug!(
            "[InvoiceTracker] invoice update payment_hash={} state={:?}",
            hex::encode(&invoice.r_hash),
            invoice.state()
        );
        let event = map_lnd_invoice_changed_event(invoice)?;
        self.port.send(event.clone());

        // Quit tracker when the status is final
        Ok(matches!(
            event,
            CchTrackingEvent::InvoiceChanged {
                status: CkbInvoiceStatus::Paid
                    | CkbInvoiceStatus::Cancelled
                    | CkbInvoiceStatus::Expired,
                ..
            }
        ))
    }
}

fn invoice_tracker_retry_delay() -> Duration {
    let jitter_millis = INVOICE_TRACKER_RETRY_JITTER.as_millis() as u64;
    let jitter = Duration::from_millis(rand::random::<u64>() % (jitter_millis + 1));
    INVOICE_TRACKER_RETRY_DELAY + jitter
}

/// LND represents missing payment preimage using all zeros hash before success.
fn is_payment_preimage_empty(
    payment_preimage: &str,
    status: lnrpc::payment::PaymentStatus,
) -> bool {
    payment_preimage.is_empty()
        || (status != lnrpc::payment::PaymentStatus::Succeeded
            && payment_preimage.chars().all(|c| c == '0'))
}

pub fn map_lnd_payment_changed_event(payment: lnrpc::Payment) -> Result<CchTrackingEvent> {
    let status = payment.status();
    let payment_preimage = if !is_payment_preimage_empty(&payment.payment_preimage, status) {
        Some(Hash256::from_str(&payment.payment_preimage)?)
    } else {
        None
    };
    let status = map_lnd_payment_status(status);

    Ok(CchTrackingEvent::PaymentChanged {
        payment_hash: Hash256::from_str(&payment.payment_hash)?,
        failure_reason: None,
        payment_preimage,
        status,
    })
}

fn map_lnd_invoice_changed_event(invoice: lnrpc::Invoice) -> Result<CchTrackingEvent> {
    let status = map_lnd_invoice_status(invoice.state());

    Ok(CchTrackingEvent::InvoiceChanged {
        payment_hash: Hash256::try_from(invoice.r_hash.as_slice())?,
        failure_reason: None,
        status,
    })
}

fn map_lnd_payment_status(status: lnrpc::payment::PaymentStatus) -> FiberPaymentStatus {
    use lnrpc::payment::PaymentStatus;
    match status {
        PaymentStatus::Unknown => FiberPaymentStatus::Created,
        PaymentStatus::InFlight => FiberPaymentStatus::Inflight,
        PaymentStatus::Succeeded => FiberPaymentStatus::Success,
        PaymentStatus::Failed => FiberPaymentStatus::Failed,
        PaymentStatus::Initiated => FiberPaymentStatus::Created,
    }
}

fn map_lnd_invoice_status(status: lnrpc::invoice::InvoiceState) -> CkbInvoiceStatus {
    use lnrpc::invoice::InvoiceState;
    match status {
        InvoiceState::Open => CkbInvoiceStatus::Open,
        InvoiceState::Settled => CkbInvoiceStatus::Paid,
        InvoiceState::Canceled => CkbInvoiceStatus::Cancelled,
        InvoiceState::Accepted => CkbInvoiceStatus::Received,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYMENT_HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const ZERO_PREIMAGE: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn lnd_payment(
        status: lnrpc::payment::PaymentStatus,
        payment_preimage: &str,
    ) -> lnrpc::Payment {
        lnrpc::Payment {
            payment_hash: PAYMENT_HASH.to_string(),
            payment_preimage: payment_preimage.to_string(),
            status: status as i32,
            ..Default::default()
        }
    }

    #[test]
    fn test_lnd_payment_mapper_accepts_successful_zero_preimage() {
        let event = map_lnd_payment_changed_event(lnd_payment(
            lnrpc::payment::PaymentStatus::Succeeded,
            ZERO_PREIMAGE,
        ))
        .expect("payment event should map");

        match event {
            CchTrackingEvent::PaymentChanged {
                payment_preimage,
                status,
                ..
            } => {
                assert_eq!(status, FiberPaymentStatus::Success);
                assert_eq!(
                    payment_preimage,
                    Some(Hash256::from_str(ZERO_PREIMAGE).expect("zero preimage should parse"))
                );
            }
            CchTrackingEvent::InvoiceChanged { .. } => panic!("expected payment event"),
        }
    }

    #[test]
    fn test_lnd_payment_mapper_keeps_zero_placeholder_empty_before_success() {
        let event = map_lnd_payment_changed_event(lnd_payment(
            lnrpc::payment::PaymentStatus::InFlight,
            ZERO_PREIMAGE,
        ))
        .expect("payment event should map");

        match event {
            CchTrackingEvent::PaymentChanged {
                payment_preimage,
                status,
                ..
            } => {
                assert_eq!(status, FiberPaymentStatus::Inflight);
                assert_eq!(payment_preimage, None);
            }
            CchTrackingEvent::InvoiceChanged { .. } => panic!("expected payment event"),
        }
    }

    #[test]
    fn test_track_payment_request_targets_one_hash_and_streams_inflight_updates() {
        let payment_hash = Hash256::from_str(PAYMENT_HASH).expect("payment hash should parse");
        let request = track_payment_request(payment_hash);

        assert_eq!(request.payment_hash, payment_hash.as_ref());
        assert!(!request.no_inflight_updates);
    }

    #[tokio::test]
    async fn test_payment_tracker_stops_on_current_terminal_state() {
        let tracker = PaymentTracker {
            port: Arc::new(OutputPort::default()),
            payment_hash: Hash256::from_str(PAYMENT_HASH).expect("payment hash should parse"),
            lnd_connection: LndConnectionInfo {
                uri: "https://localhost:10009".parse().unwrap(),
                cert: None,
                macaroon: None,
            },
            token: CancellationToken::new(),
        };

        assert!(tracker
            .on_payment(lnd_payment(
                lnrpc::payment::PaymentStatus::Succeeded,
                ZERO_PREIMAGE,
            ))
            .await
            .expect("successful payment should map"));
        assert!(tracker
            .on_payment(lnd_payment(
                lnrpc::payment::PaymentStatus::Failed,
                ZERO_PREIMAGE,
            ))
            .await
            .expect("failed payment should map"));
        assert!(!tracker
            .on_payment(lnd_payment(
                lnrpc::payment::PaymentStatus::InFlight,
                ZERO_PREIMAGE,
            ))
            .await
            .expect("in-flight payment should map"));
    }
}
