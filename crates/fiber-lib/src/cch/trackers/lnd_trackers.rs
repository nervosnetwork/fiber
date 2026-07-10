//! LND Payment and Invoice Tracker Actor
//!
//! This module implements `LndTrackerActor`, which manages concurrent tracking of
//! Lightning Network invoices and payments via LND (Lightning Network Daemon).
//!
//! ## Key Features
//!
//! - **Concurrent Tracking**: Tracks up to 5 invoices simultaneously to avoid overwhelming LND.
//! - **Queue Management**: Maintains FIFO queue for pending invoice tracking requests.
//! - **Timeout**: Re-queues active invoices after 5 minutes to prevent indefinite blocking.
//! - **Completion Handling**: Properly cleans up when tracker tasks complete, timeout or fail.
//!
//! ## Architecture
//!
//! The actor uses a message-passing model with two main message types:
//! - `TrackInvoice(Hash256)`: Adds invoice to tracking queue
//! - `TrackPayment(Hash256)`: Starts an idempotent per-payment tracker
//! - `InvoiceTrackerCompleted{...}`: Sent by spawned tracker tasks when they finish
//!
//! When a tracker task completes (successfully or with error), it ALWAYS sends
//! `InvoiceTrackerCompleted` back to the actor. The actor maintains two data structures:
//! - `invoice_queue`: VecDeque of pending invoice hashes
//! - `active_invoice_trackers`: Number of active invoice trackers
//!
//! When completion message arrives:
//! 1. Decrement `active_invoice_trackers` counter
//! 2. Re-queue if failed
//! 3. Dequeue invoices from the queue and start tracking

use std::{
    collections::{HashMap, VecDeque},
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
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef, OutputPort};
use tokio::{select, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{cch::trackers::CchTrackingEvent, invoice::CkbInvoiceStatus};
use fiber_types::payment::PaymentStatus as FiberPaymentStatus;
use fiber_types::Hash256;

const MAX_CONCURRENT_INVOICE_TRACKERS: usize = 5;
const INVOICE_TRACKING_TIMEOUT: Duration = Duration::from_secs(5 * 60); // 5 minutes

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
    /// Track a new invoice
    TrackInvoice(Hash256),

    /// Track an outgoing payment by hash until LND reports a terminal status.
    TrackPayment(Hash256),

    /// Stop tracking an invoice (remove from queue if not yet being tracked)
    StopTracking(Hash256),

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

/// Snapshot of actor state (for testing)
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub invoice_queue_len: usize,
    pub active_invoice_trackers: usize,
    pub active_payment_trackers: usize,
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
    token: CancellationToken,
    tracker: TaskTracker,
    /// Queue of payment hashes waiting to be tracked
    invoice_queue: VecDeque<Hash256>,
    /// Number of currently active invoice trackers
    active_invoice_trackers: usize,
    /// Active per-payment trackers keyed by payment hash for idempotent dispatch.
    active_payment_trackers: HashMap<Hash256, ActivePaymentTracker>,
    next_payment_tracker_id: u64,
}

struct ActivePaymentTracker {
    tracker_id: u64,
    token: CancellationToken,
}

/// Ractor Actor to track LND payments and invoices
///
/// This actor manages tracking of Lightning Network Daemon (LND) payments and invoices.
/// It provides the following features:
///
/// ## Payment Tracking
/// - Automatically tracks all LND payments in the background
/// - Sends `CchTrackingEvent::PaymentChanged` events to the output port
///
/// ## Invoice Tracking
/// - Supports tracking individual invoices via `LndTrackerMessage::TrackInvoice`
/// - Implements concurrency control: maximum 5 concurrent invoice connections
/// - Track invoices with a 5-minute timeout and automatically retry them later
/// - Queues additional invoices when concurrency limit is reached
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
/// let lnd_connection = LndConnectionInfo {
///     uri: "https://localhost:10009".parse().unwrap(),
///     cert: Some(cert_bytes),
///     macaroon: Some(macaroon_bytes),
/// };
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
        let state = LndTrackerState {
            port: args.port.clone(),
            lnd_connection: args.lnd_connection.clone(),
            token: args.token.clone(),
            tracker: args.tracker.clone(),
            invoice_queue: VecDeque::new(),
            active_invoice_trackers: 0,
            active_payment_trackers: HashMap::new(),
            next_payment_tracker_id: 0,
        };

        // Keep the global tracker as a fast notification path. Per-payment trackers
        // provide the reliable reconnect/restart recovery path for active orders.
        let payment_tracker = AllPaymentsTracker {
            port: args.port,
            lnd_connection: args.lnd_connection,
            token: args.token,
        };

        args.tracker.spawn(async move {
            payment_tracker.run().await;
        });

        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let res = match message {
            LndTrackerMessage::TrackInvoice(payment_hash) => {
                state.invoice_queue.push_back(payment_hash);
                state.process_invoice_queue(myself).await?;
                Ok(())
            }
            LndTrackerMessage::TrackPayment(payment_hash) => {
                state.start_payment_tracker(myself, payment_hash);
                Ok(())
            }
            LndTrackerMessage::StopTracking(payment_hash) => {
                // Remove from queue if present
                state.invoice_queue.retain(|&hash| hash != payment_hash);
                tracing::debug!("Stopped tracking invoice {:x}", payment_hash);
                Ok(())
            }
            LndTrackerMessage::StopTrackingPayment(payment_hash) => {
                state.stop_payment_tracker(&payment_hash);
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
                    state.active_invoice_trackers,
                    MAX_CONCURRENT_INVOICE_TRACKERS
                );
                state.active_invoice_trackers = state.active_invoice_trackers.saturating_sub(1);
                // Re-queue failed tracker
                if !completed_successfully {
                    state.invoice_queue.push_back(payment_hash);
                }

                // Now that a slot is free, we can start tracking more invoices from the queue
                state.process_invoice_queue(myself).await?;

                Ok(())
            }
            LndTrackerMessage::PaymentTrackerCompleted {
                payment_hash,
                tracker_id,
            } => {
                state.complete_payment_tracker(&payment_hash, tracker_id);
                Ok(())
            }

            #[cfg(test)]
            LndTrackerMessage::GetState(reply_port) => {
                let snapshot = StateSnapshot {
                    invoice_queue_len: state.invoice_queue.len(),
                    active_invoice_trackers: state.active_invoice_trackers,
                    active_payment_trackers: state.active_payment_trackers.len(),
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
                .set(state.active_invoice_trackers as f64);
        }

        res
    }
}

impl LndTrackerState {
    fn start_payment_tracker(
        &mut self,
        myself: ActorRef<LndTrackerMessage>,
        payment_hash: Hash256,
    ) {
        if self.active_payment_trackers.contains_key(&payment_hash) {
            tracing::debug!(
                "Outgoing payment tracker already active for payment_hash={}",
                payment_hash
            );
            return;
        }

        let tracker_id = self.next_payment_tracker_id;
        self.next_payment_tracker_id = self.next_payment_tracker_id.wrapping_add(1);
        let token = self.token.child_token();
        self.active_payment_trackers.insert(
            payment_hash,
            ActivePaymentTracker {
                tracker_id,
                token: token.clone(),
            },
        );

        let tracker = PaymentTracker {
            port: self.port.clone(),
            payment_hash,
            lnd_connection: self.lnd_connection.clone(),
            token,
        };
        self.tracker.spawn(async move {
            tracker.run().await;
            let _ = myself.send_message(LndTrackerMessage::PaymentTrackerCompleted {
                payment_hash,
                tracker_id,
            });
        });
        tracing::debug!(
            "Started outgoing payment tracker for payment_hash={} tracker_id={}",
            payment_hash,
            tracker_id
        );
    }

    fn stop_payment_tracker(&mut self, payment_hash: &Hash256) {
        if let Some(active_tracker) = self.active_payment_trackers.remove(payment_hash) {
            active_tracker.token.cancel();
            tracing::debug!(
                "Stopped outgoing payment tracker for payment_hash={} tracker_id={}",
                payment_hash,
                active_tracker.tracker_id
            );
        }
    }

    fn complete_payment_tracker(&mut self, payment_hash: &Hash256, tracker_id: u64) {
        let is_current_tracker = self
            .active_payment_trackers
            .get(payment_hash)
            .is_some_and(|tracker| tracker.tracker_id == tracker_id);
        if is_current_tracker {
            self.active_payment_trackers.remove(payment_hash);
            tracing::debug!(
                "Completed outgoing payment tracker for payment_hash={} tracker_id={}",
                payment_hash,
                tracker_id
            );
        }
    }

    async fn process_invoice_queue(
        &mut self,
        myself: ActorRef<LndTrackerMessage>,
    ) -> Result<(), ActorProcessingErr> {
        // Process invoices from queue
        while self.active_invoice_trackers < MAX_CONCURRENT_INVOICE_TRACKERS {
            let Some(payment_hash) = self.invoice_queue.pop_front() else {
                break;
            };
            self.active_invoice_trackers += 1;

            let tracker = InvoiceTracker {
                port: self.port.clone(),
                lnd_connection: self.lnd_connection.clone(),
                token: self.token.clone(),
                payment_hash,
            };

            // ALWAYS send InvoiceTrackerCompleted message back to actor
            // - This ensures we decrement counter and remove from queue
            // - Even on error, the tracker has quit, so we must clean up
            let myself_clone = myself.clone();
            self.tracker.spawn(async move {
                select! {
                    _ = sleep(INVOICE_TRACKING_TIMEOUT) => {
                        let _ = tracker;
                        myself_clone.cast(LndTrackerMessage::InvoiceTrackerCompleted {
                            payment_hash,
                            completed_successfully: false,
                        }).expect("cast LndTrackerMessage");
                    }
                    completed_successfully = tracker.run() => {
                        myself_clone.cast(LndTrackerMessage::InvoiceTrackerCompleted {
                            payment_hash,
                            completed_successfully,
                        }).expect("cast LndTrackerMessage");
                    }
                }
            });

            tracing::debug!(
                "Started invoice tracker for payment_hash={}, active={}/{}",
                payment_hash,
                self.active_invoice_trackers,
                MAX_CONCURRENT_INVOICE_TRACKERS
            );
        }

        Ok(())
    }
}

/// Internal struct for tracking payments
struct AllPaymentsTracker {
    port: Arc<OutputPort<CchTrackingEvent>>,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl AllPaymentsTracker {
    async fn run(self) {
        let token = self.token.clone();
        let fut = self.run_loop();
        token.run_until_cancelled(fut).await;
    }

    async fn run_loop(self) {
        tracing::debug!("PaymentTracker: will connect {}", self.lnd_connection.uri);

        while let Err(err) = self.run_once().await {
            tracing::error!(
                "Error tracking LND payments, retry 15 seconds later: {:?}",
                err
            );
            sleep(Duration::from_secs(15)).await;
        }
    }

    async fn run_once(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_router_client().await?;
        let mut stream = client
            .track_payments(routerrpc::TrackPaymentsRequest {
                no_inflight_updates: true,
            })
            .await?
            .into_inner();

        loop {
            match stream.next().await {
                Some(Ok(payment)) => self.on_payment(payment).await?,
                Some(Err(err)) => return Err(err.into()),
                None => return Err(anyhow!("unexpected closed stream")),
            }
        }
    }

    async fn on_payment(&self, payment: lnrpc::Payment) -> Result<()> {
        let payment_hash = payment.payment_hash.clone();
        let status = payment.status();
        let has_payment_preimage = !is_payment_preimage_empty(&payment.payment_preimage, status);
        tracing::debug!(
            "payment changed payment_hash={} status={:?} has_payment_preimage={}",
            payment_hash,
            status,
            has_payment_preimage
        );
        self.port.send(map_lnd_payment_changed_event(payment)?);
        Ok(())
    }
}

/// Tracks one outgoing payment by hash. Unlike `TrackPayments`, `TrackPaymentV2`
/// returns the current state of the requested payment after every reconnect,
/// including a terminal state reached while CCH or LND was unavailable.
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
        let status = payment.status();
        let is_terminal = is_lnd_payment_terminal(status);
        let event = map_lnd_payment_changed_event(payment)?;
        self.port.send(event);
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
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
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
            tracing::error!(
                "Error tracking LND invoice {}, retry 15 seconds later: {:?}",
                self.payment_hash,
                err
            );
            sleep(Duration::from_secs(15)).await;
        }
        tracing::debug!(
            "InvoiceTracker completed successfully for payment_hash={}",
            self.payment_hash
        );
    }

    async fn run_once(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_invoices_client().await?;
        let mut stream = client
            .subscribe_single_invoice(invoicesrpc::SubscribeSingleInvoiceRequest {
                r_hash: self.payment_hash.into(),
            })
            .await?
            .into_inner();

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
