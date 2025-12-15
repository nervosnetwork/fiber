//! LND Payment and Invoice Tracker Actor
//!
//! This module implements `LndTrackerActor`, which manages concurrent tracking of
//! Lightning Network invoices and payments via LND (Lightning Network Daemon).
//!
//! ## Key Features
//!
//! - **Concurrent Tracking**: Tracks up to 5 invoices simultaneously to avoid overwhelming LND
//! - **Queue Management**: Maintains FIFO queue for pending invoice tracking requests
//! - **Timeout**: Re-queues active invoices after 5 minutes to prevent indefinite blocking
//! - **Completion Handling**: Properly cleans up when tracker tasks complete, timeout or fail
//!
//! ## Architecture
//!
//! The actor uses a message-passing model with two main message types:
//! - `TrackInvoice(Hash256)`: Adds invoice to tracking queue
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

use std::{collections::VecDeque, str::FromStr, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef, OutputPort};
use tokio::{select, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    cch::{CchIncomingEvent, CchIncomingPaymentStatus, CchOutgoingPaymentStatus},
    fiber::types::Hash256,
};

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

    /// Notification that an invoice tracker task has completed
    ///
    /// Sent by InvoiceTracker tasks when they terminate (either successfully
    /// when invoice reaches final state, or due to error).
    InvoiceTrackerCompleted {
        payment_hash: Hash256,
        completed_successfully: bool,
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
}

/// Arguments for starting the LndTrackerActor
pub struct LndTrackerArgs {
    pub port: Arc<OutputPort<CchIncomingEvent>>,
    pub lnd_connection: LndConnectionInfo,
    pub token: CancellationToken,
    pub tracker: TaskTracker,
}

/// State for the LndTrackerActor
pub struct LndTrackerState {
    port: Arc<OutputPort<CchIncomingEvent>>,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
    tracker: TaskTracker,
    /// Queue of payment hashes waiting to be tracked
    invoice_queue: VecDeque<Hash256>,
    /// Number of currently active invoice trackers
    active_invoice_trackers: usize,
}

/// Ractor Actor to track LND payments and invoices
///
/// This actor manages tracking of Lightning Network Daemon (LND) payments and invoices.
/// It provides the following features:
///
/// ## Payment Tracking
/// - Automatically tracks all LND payments in the background
/// - Sends `CchIncomingEvent::PaymentChanged` events to the output port
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
/// let port = Arc::new(OutputPort::<CchIncomingEvent>::default());
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
        let (actor, _handle) = Actor::spawn_linked(
            Some("lnd_tracker_actor".to_string()),
            LndTrackerActor,
            args,
            root_actor,
        )
        .await?;
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
        };

        // Start payment tracker in background
        let payment_tracker = PaymentTracker {
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

            #[cfg(test)]
            LndTrackerMessage::GetState(reply_port) => {
                let snapshot = StateSnapshot {
                    invoice_queue_len: state.invoice_queue.len(),
                    active_invoice_trackers: state.active_invoice_trackers,
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

            // Spawned Task Completion Flow:
            // 1. Clone actor reference and payment hash before moving into async task
            // 2. Spawn tracker in background (tokio::spawn)
            // 3. Capture result from tracker.run()
            // 4. ALWAYS send InvoiceTrackerCompleted message back to actor
            //    - This ensures we decrement counter and remove from queue
            //    - Even on error, the tracker has quit, so we must clean up

            let myself_clone = myself.clone();
            self.tracker.spawn(async move {
                select! {
                    _ = sleep(INVOICE_TRACKING_TIMEOUT) => {
                        myself_clone.cast(LndTrackerMessage::InvoiceTrackerCompleted {
                            payment_hash,
                            completed_successfully: false,
                        }).expect("cast LndTrackerMessage");
                    }
                    result = tracker.run() => {
                        myself_clone.cast(LndTrackerMessage::InvoiceTrackerCompleted {
                            payment_hash,
                            completed_successfully: result.is_ok(),
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
struct PaymentTracker {
    port: Arc<OutputPort<CchIncomingEvent>>,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl PaymentTracker {
    async fn run(self) {
        tracing::debug!("PaymentTracker: will connect {}", self.lnd_connection.uri);

        loop {
            select! {
                result = self.run_inner() => {
                    match result {
                        Ok(_) => {
                            break;
                        }
                        Err(err) => {
                            tracing::error!(
                                "Error tracking LND payments, retry 15 seconds later: {:?}",
                                err
                            );
                            select! {
                                _ = sleep(Duration::from_secs(15)) => {
                                    // continue
                                }
                                _ = self.token.cancelled() => {
                                    tracing::debug!("Cancellation received, shutting down payment tracker");
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down payment tracker");
                    return;
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_router_client().await?;
        let mut stream = client
            .track_payments(routerrpc::TrackPaymentsRequest {
                no_inflight_updates: true,
            })
            .await?
            .into_inner();

        loop {
            select! {
                payment_opt = stream.next() => {
                    match payment_opt {
                        Some(Ok(payment)) => self.on_payment(payment).await?,
                        Some(Err(err)) => return Err(err.into()),
                        None => return Err(anyhow!("unexpected closed stream")),
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down payment tracker");
                    return Ok(());
                }
            }
        }
    }

    async fn on_payment(&self, payment: lnrpc::Payment) -> Result<()> {
        tracing::debug!("payment: {:?}", payment);
        let payment_preimage = if !payment.payment_preimage.is_empty() {
            Some(Hash256::from_str(&payment.payment_preimage)?)
        } else {
            None
        };
        use lnrpc::payment::PaymentStatus;
        let status: CchOutgoingPaymentStatus = PaymentStatus::try_from(payment.status)
            .unwrap_or(PaymentStatus::InFlight)
            .into();

        let event = CchIncomingEvent::PaymentChanged {
            payment_hash: Hash256::from_str(&payment.payment_hash)?,
            payment_preimage,
            status,
        };
        self.port.send(event);
        Ok(())
    }
}

/// Internal struct for tracking individual invoices
struct InvoiceTracker {
    port: Arc<OutputPort<CchIncomingEvent>>,
    payment_hash: Hash256,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl InvoiceTracker {
    async fn run(self) -> Result<()> {
        tracing::debug!(
            "InvoiceTracker: will connect {} for payment_hash={}",
            self.lnd_connection.uri,
            self.payment_hash
        );

        loop {
            select! {
                result = self.run_inner() => {
                    match result {
                        Ok(_) => {
                            tracing::debug!("InvoiceTracker completed successfully for payment_hash={}", self.payment_hash);
                            return Ok(());
                        }
                        Err(err) => {
                            tracing::error!(
                                "Error tracking LND invoice {}, retry 15 seconds later: {:?}",
                                self.payment_hash,
                                err
                            );
                            select! {
                                _ = sleep(Duration::from_secs(15)) => {
                                    // continue
                                }
                                _ = self.token.cancelled() => {
                                    tracing::debug!("Cancellation received, shutting down invoice tracker");
                                    return Err(anyhow!("Cancelled"));
                                }
                            }
                        }
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down invoice tracker");
                    return Err(anyhow!("Cancelled"));
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_invoices_client().await?;
        let mut stream = client
            .subscribe_single_invoice(invoicesrpc::SubscribeSingleInvoiceRequest {
                r_hash: self.payment_hash.into(),
            })
            .await?
            .into_inner();

        loop {
            select! {
                invoice_opt = stream.next() => {
                    match invoice_opt {
                        Some(Ok(invoice)) => if self.on_invoice(invoice).await? {
                            return Ok(());
                        },
                        Some(Err(err)) => return Err(err.into()),
                        None => return Err(anyhow!("unexpected closed stream")),
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down invoice tracker");
                    return Ok(());
                }
            }
        }
    }

    // Return true to quit the tracker
    async fn on_invoice(&self, invoice: lnrpc::Invoice) -> Result<bool> {
        tracing::debug!("[InvoiceTracker] invoice: {:?}", invoice);
        use lnrpc::invoice::InvoiceState;
        let status: CchIncomingPaymentStatus = InvoiceState::try_from(invoice.state)
            .unwrap_or(InvoiceState::Open)
            .into();

        let event = CchIncomingEvent::InvoiceChanged {
            payment_hash: Hash256::try_from(invoice.r_hash.as_slice())?,
            status,
        };
        self.port.send(event);

        // Quit tracker when the status is final
        Ok(status == CchIncomingPaymentStatus::Settled
            || status == CchIncomingPaymentStatus::Failed)
    }
}
