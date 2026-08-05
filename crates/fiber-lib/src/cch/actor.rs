use anyhow::{anyhow, Context, Result};
use lightning_invoice::Bolt11Invoice;
use lightning_invoice::Currency as LnCurrency;
use lnd_grpc_tonic_client::{invoicesrpc, lnrpc, Uri};
use ractor::{
    port::OutputPortSubscriberTrait as _, Actor, ActorProcessingErr, ActorRef, OutputPort,
    RpcReplyPort,
};
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use tentacle::secio::SecioKeyPair;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::cch::actions::send_outgoing_payment::{
    outgoing_fee_budget_from_fee_sats, outgoing_max_fee_rate,
};
use crate::cch::actions::{ActionDispatcher, CchOrderAction};
use crate::cch::cch_fiber_agent::{CchFiberAgentActor, CchFiberAgentHttpBackend, CchFiberAgentRef};
use crate::cch::order::CchOrderStateMachine;
use crate::cch::scheduler::{CchOrderSchedulerActor, SchedulerArgs, SchedulerMessage};
use crate::cch::trackers::{
    CchTrackingEvent, InvoiceTrackingReservationResult, LndConnectionInfo, LndTrackerActor,
    LndTrackerArgs, LndTrackerMessage, PaymentTrackingReservationResult, RedactedCchTrackingEvent,
    MAX_TRACKED_INVOICES, MAX_TRACKED_PAYMENTS,
};
use crate::cch::{CchConfig, CchError, CchOrderStore, CchStoreError, OutgoingFeeLimit};
use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::NetworkActorMessage;
use crate::invoice::{Attribute, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder};
use crate::now_timestamp_as_millis_u64;
use crate::store::store_impl::StoreChange;
use crate::time::Duration;
use fiber_types::{
    AttemptStatus, CchInvoice, CchOrder, CchOrderStatus, CchReceiveBtcOrderCreation,
    CchSendBtcOrderCreation, HashAlgorithm,
};
use fiber_types::{Hash256, Privkey};

pub const ACTION_RETRY_BASE_MILLIS: u64 = 1000; // 1 second initial delay
pub const ACTION_RETRY_MAX_MILLIS: u64 = 600_000; // 10 minute max delay
const STARTUP_RECOVERY_ITEM_DELAY_MILLIS: u64 = 1;

/// Average time per Bitcoin block in milliseconds (10 minutes = 600 seconds = 600,000 ms).
pub const BTC_BLOCK_TIME_MILLIS: u64 = 600_000;

fn calculate_retry_delay(retry_count: u32) -> Duration {
    // Exponential backoff starting from ACTION_RETRY_BASE_MILLIS, capped at ACTION_RETRY_MAX_MILLIS
    let max_shift = (ACTION_RETRY_MAX_MILLIS / ACTION_RETRY_BASE_MILLIS).ilog2();
    let delay = ACTION_RETRY_BASE_MILLIS.saturating_mul(1 << retry_count.min(max_shift));
    Duration::from_millis(delay.min(ACTION_RETRY_MAX_MILLIS))
}

#[derive(Clone, Debug, Deserialize)]
pub struct SendBTC {
    pub btc_pay_req: String,
    pub currency: Currency,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReceiveBTC {
    pub fiber_pay_req: String,
}

#[async_trait::async_trait]
pub(crate) trait LndInvoiceClient: Send + Sync {
    async fn lookup_invoice(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<lnrpc::Invoice>, CchError>;

    async fn add_hold_invoice(
        &self,
        request: invoicesrpc::AddHoldInvoiceRequest,
    ) -> Result<invoicesrpc::AddHoldInvoiceResp, CchError>;
}

struct TonicLndInvoiceClient {
    connection: LndConnectionInfo,
}

const LND_NO_INVOICES_CREATED_ERROR: &str = "there are no existing invoices";

fn is_lnd_invoice_not_found(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::NotFound
        || (status.code() == tonic::Code::Unknown
            && status.message() == LND_NO_INVOICES_CREATED_ERROR)
}

#[async_trait::async_trait]
impl LndInvoiceClient for TonicLndInvoiceClient {
    async fn lookup_invoice(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<lnrpc::Invoice>, CchError> {
        let mut client = self.connection.create_invoices_client().await?;
        let request = invoicesrpc::LookupInvoiceMsg {
            lookup_modifier: invoicesrpc::LookupModifier::Default as i32,
            invoice_ref: Some(invoicesrpc::lookup_invoice_msg::InvoiceRef::PaymentHash(
                payment_hash.as_ref().to_vec(),
            )),
        };
        match client.lookup_invoice_v2(request).await {
            Ok(response) => Ok(Some(response.into_inner())),
            Err(status) if is_lnd_invoice_not_found(&status) => Ok(None),
            Err(status) => Err(CchError::LndRpcError(format!(
                "lookup invoice {:x}: {}",
                payment_hash, status
            ))),
        }
    }

    async fn add_hold_invoice(
        &self,
        request: invoicesrpc::AddHoldInvoiceRequest,
    ) -> Result<invoicesrpc::AddHoldInvoiceResp, CchError> {
        let mut client = self.connection.create_invoices_client().await?;
        client
            .add_hold_invoice(request.clone())
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| CchError::LndRpcError(format!("{}, request: {:?}", status, request)))
    }
}

pub enum CchMessage {
    SendBTC(SendBTC, RpcReplyPort<Result<CchOrder, CchError>>),
    ReceiveBTC(ReceiveBTC, RpcReplyPort<Result<CchOrder, CchError>>),

    GetCchOrder(Hash256, RpcReplyPort<Result<CchOrder, CchError>>),

    /// Continue startup recovery after taking a snapshot of persisted keys.
    #[doc(hidden)]
    ContinueStartupRecovery {
        order_keys: Vec<Hash256>,
        receive_creation_keys: Vec<Hash256>,
        send_creation_keys: Vec<Hash256>,
        recovered_orders: usize,
        recovered_receive_creations: usize,
        recovered_send_creations: usize,
        started_at_millis: u64,
    },

    TrackingEvent(CchTrackingEvent),

    /// Store change event from the Fiber node (either in-process or via WebSocket).
    StoreChangeEvent(StoreChange),

    /// Reconcile active outgoing Fiber payments from durable state after a WebSocket reconnect.
    ReconcileFiberPayments,

    /// Schedule a retry for an action with backoff after a transient failure.
    ActionRetry {
        payment_hash: Hash256,
        action: CchOrderAction,
        retry_count: u32,
        reason: String,
    },

    ExecuteAction {
        payment_hash: Hash256,
        action: CchOrderAction,
        retry_count: u32,
    },

    /// Resume a durable `receive_btc` creation left by an interrupted attempt.
    ResumeReceiveBTCOrderCreation {
        payment_hash: Hash256,
        retry_count: u32,
    },

    /// Resume a durable `send_btc` creation left by an interrupted attempt.
    ResumeSendBTCOrderCreation {
        payment_hash: Hash256,
        retry_count: u32,
    },

    SendBTCWorkerCompleted {
        payment_hash: Hash256,
        result: Result<(CchOrder, bool), CchError>,
        port: RpcReplyPort<Result<CchOrder, CchError>>,
    },

    ResumeSendBTCOrderCreationWorkerCompleted {
        payment_hash: Hash256,
        retry_count: u32,
        result: Result<Option<CchOrder>, CchError>,
    },

    /// Expire an active order after its configured expiry window elapses.
    ExpireOrder(Hash256),

    /// Test-only message to insert an order directly into the database
    #[cfg(test)]
    InsertOrder(CchOrder, RpcReplyPort<Result<(), CchError>>),

    /// Test-only readiness probe for startup recovery.
    #[cfg(test)]
    GetStartupRecoveryStatus(RpcReplyPort<bool>),
}

impl From<CchTrackingEvent> for CchMessage {
    fn from(value: CchTrackingEvent) -> Self {
        CchMessage::TrackingEvent(value)
    }
}

impl From<StoreChange> for CchMessage {
    fn from(value: StoreChange) -> Self {
        CchMessage::StoreChangeEvent(value)
    }
}

pub struct CchActor<S>(std::marker::PhantomData<S>);

impl<S> Default for CchActor<S> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

pub struct CchArgs<S> {
    pub config: CchConfig,
    pub tracker: TaskTracker,
    pub token: CancellationToken,
    pub network_actor: Option<ActorRef<NetworkActorMessage>>,
    pub node_keypair: Option<crate::fiber::KeyPair>,
    pub store: S,
    /// The CKB network currency this node is configured for.
    /// Used to validate that incoming invoices match the expected network.
    pub currency: Currency,
    #[cfg(test)]
    pub(crate) lnd_invoice_client: Option<Arc<dyn LndInvoiceClient>>,
}

#[derive(Clone)]
struct DeferredInvoiceStatus {
    status: CkbInvoiceStatus,
    failure_reason: Option<String>,
}

impl DeferredInvoiceStatus {
    fn merge(&mut self, newer: Self) {
        if invoice_status_rank(newer.status) >= invoice_status_rank(self.status) {
            *self = newer;
        }
    }
}

#[derive(Clone)]
pub struct CchState<S> {
    pub(super) config: CchConfig,
    pub(super) fiber_agent_ref: CchFiberAgentRef,
    pub(super) node_keypair: Option<(PublicKey, SecretKey)>,
    pub(super) lnd_connection: LndConnectionInfo,
    lnd_invoice_client: Arc<dyn LndInvoiceClient>,
    pub(super) lnd_tracker: ActorRef<LndTrackerMessage>,
    pub(super) scheduler: ActorRef<SchedulerMessage>,
    pub(super) store: S,
    task_tracker: TaskTracker,
    pending_receive_btc_creation_retries: HashSet<Hash256>,
    pending_send_btc_creation_retries: HashSet<Hash256>,
    active_send_btc_creation_workers: HashSet<Hash256>,
    active_send_btc_requests: HashMap<Hash256, String>,
    deferred_send_btc_invoice_statuses: HashMap<Hash256, DeferredInvoiceStatus>,
    startup_recovery_in_progress: bool,
    /// The CKB network currency this node is configured for.
    pub(super) currency: Currency,
}

#[async_trait::async_trait]
impl<S: CchOrderStore + Send + Sync + Clone + 'static> Actor for CchActor<S> {
    type Msg = CchMessage;
    type State = CchState<S>;
    type Arguments = CchArgs<S>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // Validate generic config invariants (e.g. the outgoing fee budget percentage).
        args.config.validate().map_err(|e| anyhow!(e))?;

        // Validate that we have either an in-process network actor or a fiber RPC URL
        if args.network_actor.is_none() {
            if args.config.fiber_rpc_url.is_none() {
                return Err(anyhow!(
                    "Cch requires either in-process network actor or configured fiber RPC URL"
                )
                .into());
            }
            ensure_fiber_http_url(args.config.fiber_rpc_url.clone())?;
        }

        let lnd_rpc_url: Uri = args.config.lnd_rpc_url.clone().try_into()?;
        let cert = match args.config.resolve_lnd_cert_path() {
            Some(path) => Some(
                tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("read cert file {}", path.display()))?,
            ),
            None => None,
        };
        let macaroon = match args.config.resolve_lnd_macaroon_path() {
            Some(path) => Some(
                tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("read macaroon file {}", path.display()))?,
            ),
            None => None,
        };
        let lnd_connection = LndConnectionInfo::new(lnd_rpc_url, cert, macaroon);
        #[cfg(test)]
        let lnd_invoice_client = args.lnd_invoice_client.unwrap_or_else(|| {
            Arc::new(TonicLndInvoiceClient {
                connection: lnd_connection.clone(),
            })
        });
        #[cfg(not(test))]
        let lnd_invoice_client: Arc<dyn LndInvoiceClient> = Arc::new(TonicLndInvoiceClient {
            connection: lnd_connection.clone(),
        });

        let node_keypair = args.node_keypair.map(|kp| {
            let private_key: Privkey = <[u8; 32]>::try_from(kp.as_ref())
                .expect("valid length for key")
                .into();
            let secio_kp = SecioKeyPair::from(kp);
            (
                PublicKey::from_slice(secio_kp.public_key().inner_ref()).expect("valid public key"),
                private_key.into(),
            )
        });

        // Create LND tracker port and subscribe
        let lnd_port = Arc::new(OutputPort::default());
        let lnd_tracker = LndTrackerActor::start(
            LndTrackerArgs {
                port: lnd_port.clone(),
                lnd_connection: lnd_connection.clone(),
                tracker: args.tracker.clone(),
                token: args.token.clone(),
            },
            myself.get_cell(),
        )
        .await?;
        myself.subscribe_to_port(&lnd_port);

        // Start scheduler actor
        let scheduler = CchOrderSchedulerActor::start(
            SchedulerArgs {
                store: args.store.clone(),
                lnd_tracker: lnd_tracker.clone(),
                cch_actor: myself.clone(),
            },
            myself.get_cell(),
        )
        .await?;

        // Set up store change subscription
        if args.network_actor.is_none() {
            // Separate service mode: subscribe to Fiber node's store changes via WebSocket
            let ws_url = ensure_fiber_ws_url(args.config.fiber_rpc_url.clone())?;
            let myself_clone = myself.clone();
            let token = args.token.clone();
            args.tracker.spawn(async move {
                subscribe_store_changes_ws(ws_url, myself_clone, token).await;
            });
        }

        let fiber_agent_ref = if let Some(network_actor) = args.network_actor {
            CchFiberAgentRef::InProcess(network_actor)
        } else {
            let url = args
                .config
                .fiber_rpc_url
                .as_deref()
                .expect("validated in pre_start");
            let backend = CchFiberAgentHttpBackend::try_new(url)?;
            let (rpc_actor, _handle) =
                ractor::Actor::spawn_linked(None, CchFiberAgentActor, backend, myself.get_cell())
                    .await?;
            CchFiberAgentRef::Rpc(rpc_actor)
        };

        let state = CchState {
            config: args.config,
            fiber_agent_ref,
            store: args.store,
            node_keypair,
            lnd_connection,
            lnd_invoice_client,
            lnd_tracker,
            scheduler,
            task_tracker: args.tracker,
            currency: args.currency,
            pending_receive_btc_creation_retries: HashSet::new(),
            pending_send_btc_creation_retries: HashSet::new(),
            active_send_btc_creation_workers: HashSet::new(),
            active_send_btc_requests: HashMap::new(),
            deferred_send_btc_invoice_statuses: HashMap::new(),
            startup_recovery_in_progress: true,
        };

        Ok(state)
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let store = state.store.clone();
        let started_at_millis = now_timestamp_as_millis_u64();
        let load_snapshot = move || {
            let order_keys = store.get_cch_order_keys_iter().into_iter().collect();
            let receive_creation_keys = store
                .get_receive_btc_order_creation_keys_iter()
                .into_iter()
                .collect();
            let send_creation_keys = store
                .get_send_btc_order_creation_keys_iter()
                .into_iter()
                .collect();
            if let Err(err) = myself.send_message(CchMessage::ContinueStartupRecovery {
                order_keys,
                receive_creation_keys,
                send_creation_keys,
                recovered_orders: 0,
                recovered_receive_creations: 0,
                recovered_send_creations: 0,
                started_at_millis,
            }) {
                tracing::error!("Failed to start CCH recovery from persisted state: {}", err);
            }
        };

        #[cfg(not(target_family = "wasm"))]
        state.task_tracker.spawn_blocking(load_snapshot);
        #[cfg(target_family = "wasm")]
        state.task_tracker.spawn(async move { load_snapshot() });

        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchMessage::SendBTC(send_btc, port) => {
                if state.startup_recovery_in_progress {
                    let _ = port.send(Err(CchError::StartupRecoveryInProgress));
                    return Ok(());
                }
                // A timed-out RPC leaves its message in the mailbox. Do not begin a new
                // state-changing operation when its caller is no longer waiting for the result.
                if port.is_closed() {
                    return Ok(());
                }
                if let Err(err) = state.ensure_send_btc_currency(send_btc.currency) {
                    let _ = port.send(Err(err));
                    return Ok(());
                }
                let payment_hash = match Bolt11Invoice::from_str(&send_btc.btc_pay_req) {
                    Ok(invoice) => Hash256::from(*invoice.payment_hash()),
                    Err(err) => {
                        let _ = port.send(Err(err.into()));
                        return Ok(());
                    }
                };
                if state
                    .active_send_btc_creation_workers
                    .contains(&payment_hash)
                    || state
                        .pending_send_btc_creation_retries
                        .contains(&payment_hash)
                {
                    let pending_error = match state
                        .get_send_btc_order_creation_or_none(&payment_hash)
                    {
                        Ok(Some(creation)) => Some(
                            state
                                .ensure_send_btc_request_matches_creation(
                                    &send_btc.btc_pay_req,
                                    &creation,
                                )
                                .err()
                                .unwrap_or(CchError::SendBTCOrderCreationInProgress(payment_hash)),
                        ),
                        Ok(None) => match state.active_send_btc_requests.get(&payment_hash) {
                            Some(btc_pay_req) if btc_pay_req != &send_btc.btc_pay_req => {
                                Some(CchError::ConflictingSendBTCRequest(payment_hash))
                            }
                            Some(_) => Some(CchError::SendBTCOrderCreationInProgress(payment_hash)),
                            None => None,
                        },
                        Err(err) => Some(err),
                    };
                    if let Some(err) = pending_error {
                        let _ = port.send(Err(err));
                        return Ok(());
                    }
                    state
                        .pending_send_btc_creation_retries
                        .remove(&payment_hash);
                    state.active_send_btc_creation_workers.remove(&payment_hash);
                    state.active_send_btc_requests.remove(&payment_hash);
                }

                let worker_state = state.clone();
                state.active_send_btc_creation_workers.insert(payment_hash);
                state
                    .active_send_btc_requests
                    .insert(payment_hash, send_btc.btc_pay_req.clone());
                let myself = myself.clone();
                state.task_tracker.spawn(async move {
                    let result = worker_state.send_btc(send_btc).await;
                    let _ = myself.send_message(CchMessage::SendBTCWorkerCompleted {
                        payment_hash,
                        result,
                        port,
                    });
                });
                Ok(())
            }
            CchMessage::SendBTCWorkerCompleted {
                payment_hash,
                mut result,
                port,
            } => {
                state
                    .pending_send_btc_creation_retries
                    .remove(&payment_hash);
                if let Ok((_, is_new)) = &result {
                    match state.reconcile_deferred_send_btc_invoice_status(payment_hash) {
                        Ok((order, event_applied)) => {
                            if *is_new || event_applied {
                                state.schedule_job_on_entering(&order);
                                let actions = if *is_new {
                                    ActionDispatcher::on_starting(&order)
                                } else {
                                    ActionDispatcher::on_entering(&order)
                                };
                                append_actions(myself.clone(), order.payment_hash, actions)?;
                            }
                            result = Ok((order, *is_new));
                        }
                        Err(err) => result = Err(err),
                    }
                }
                if let Err(err) = &result {
                    if state
                        .get_send_btc_order_creation_or_none(&payment_hash)?
                        .is_some()
                    {
                        if is_retryable_send_btc_creation_error(err) {
                            schedule_send_btc_creation_retry(
                                &myself,
                                &mut state.pending_send_btc_creation_retries,
                                payment_hash,
                                0,
                                &err.to_string(),
                            );
                        } else {
                            if !matches!(err, CchError::SendBTCOrderCreationInProgress(_)) {
                                tracing::error!(
                                    "Permanently failed to complete send_btc creation {:x}: {}. The durable intent is retained for operator inspection",
                                    payment_hash,
                                    err
                                );
                            }
                            state
                                .deferred_send_btc_invoice_statuses
                                .remove(&payment_hash);
                        }
                    } else {
                        state
                            .deferred_send_btc_invoice_statuses
                            .remove(&payment_hash);
                    }
                }
                state.active_send_btc_creation_workers.remove(&payment_hash);
                state.active_send_btc_requests.remove(&payment_hash);
                if !port.is_closed() {
                    let _ = port.send(result.map(|(order, _)| order));
                }
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                if state.startup_recovery_in_progress {
                    let _ = port.send(Err(CchError::StartupRecoveryInProgress));
                    return Ok(());
                }
                let payment_hash = CkbInvoice::from_str(&receive_btc.fiber_pay_req)
                    .ok()
                    .map(|invoice| *invoice.payment_hash());
                let pending_error = match payment_hash {
                    Some(payment_hash)
                        if state
                            .pending_receive_btc_creation_retries
                            .contains(&payment_hash) =>
                    {
                        match state.get_receive_btc_order_creation_or_none(&payment_hash) {
                            Ok(Some(creation)) => Some(
                                state
                                    .ensure_receive_btc_request_matches_creation(
                                        &receive_btc.fiber_pay_req,
                                        &creation,
                                    )
                                    .err()
                                    .unwrap_or(CchError::ReceiveBTCOrderCreationInProgress(
                                        payment_hash,
                                    )),
                            ),
                            Ok(None) => None,
                            Err(err) => Some(err),
                        }
                    }
                    _ => None,
                };
                let result = match pending_error {
                    Some(err) => Err(err),
                    None => state.receive_btc(receive_btc).await,
                };
                if let Ok((order, true)) = &result {
                    state
                        .pending_receive_btc_creation_retries
                        .remove(&order.payment_hash);
                    // Schedule jobs for new order
                    state.schedule_job_for_non_final_order(order);
                    let actions = ActionDispatcher::on_starting(order);
                    append_actions(myself, order.payment_hash, actions)?;
                } else if let (Some(payment_hash), Err(err)) = (payment_hash, &result) {
                    if state
                        .get_receive_btc_order_creation_or_none(&payment_hash)?
                        .is_some()
                    {
                        if is_retryable_receive_btc_creation_error(err) {
                            schedule_receive_btc_creation_retry(
                                &myself,
                                &mut state.pending_receive_btc_creation_retries,
                                payment_hash,
                                0,
                                &err.to_string(),
                            );
                        } else if !matches!(err, CchError::ReceiveBTCOrderCreationInProgress(_)) {
                            tracing::error!(
                                "Permanently failed to complete receive_btc creation {:x}: {}. The durable intent is retained for operator inspection",
                                payment_hash,
                                err
                            );
                        }
                    }
                }
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result.map(|(order, _)| order));
                }
                Ok(())
            }
            CchMessage::GetCchOrder(payment_hash, port) => {
                let result = state.get_order_or_none(&payment_hash).and_then(|order| {
                    order.ok_or_else(|| CchStoreError::NotFound(payment_hash).into())
                });
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::ContinueStartupRecovery {
                mut order_keys,
                mut receive_creation_keys,
                mut send_creation_keys,
                mut recovered_orders,
                mut recovered_receive_creations,
                mut recovered_send_creations,
                started_at_millis,
            } => {
                if let Some(payment_hash) = order_keys.pop() {
                    // Keep only compact keys in the startup snapshot. Reading the order when its
                    // recovery turn arrives both bounds snapshot memory and observes any state
                    // transition that happened while earlier keys were being processed.
                    if let Some(order) = state.get_order_or_none(&payment_hash)? {
                        recovered_orders += 1;
                        state.recover_persisted_order(&myself, order)?;
                    }
                } else if let Some(payment_hash) = receive_creation_keys.pop() {
                    recovered_receive_creations += 1;
                    if state
                        .pending_receive_btc_creation_retries
                        .insert(payment_hash)
                    {
                        myself.send_message(CchMessage::ResumeReceiveBTCOrderCreation {
                            payment_hash,
                            retry_count: 0,
                        })?;
                    }
                } else if let Some(payment_hash) = send_creation_keys.pop() {
                    recovered_send_creations += 1;
                    if state.pending_send_btc_creation_retries.insert(payment_hash) {
                        myself.send_message(CchMessage::ResumeSendBTCOrderCreation {
                            payment_hash,
                            retry_count: 0,
                        })?;
                    }
                } else {
                    state.startup_recovery_in_progress = false;
                    tracing::info!(
                        recovered_orders,
                        recovered_receive_creations,
                        recovered_send_creations,
                        elapsed_millis =
                            now_timestamp_as_millis_u64().saturating_sub(started_at_millis),
                        "CCH startup recovery completed"
                    );
                    return Ok(());
                }

                // Keep a single recovery continuation in flight and leave a mailbox window
                // between store reads. An immediate self-message can otherwise stay ahead of
                // RPC work while recovering a large key snapshot.
                myself.send_after(
                    Duration::from_millis(STARTUP_RECOVERY_ITEM_DELAY_MILLIS),
                    move || CchMessage::ContinueStartupRecovery {
                        order_keys,
                        receive_creation_keys,
                        send_creation_keys,
                        recovered_orders,
                        recovered_receive_creations,
                        recovered_send_creations,
                        started_at_millis,
                    },
                );
                Ok(())
            }
            CchMessage::TrackingEvent(event) => {
                tracing::debug!("tracking event {:?}", RedactedCchTrackingEvent(&event));
                let payment_hash = *event.payment_hash();
                match state.handle_tracking_event(event).await {
                    Ok(actions) => {
                        append_actions(myself, payment_hash, actions)?;
                    }
                    Err(err) => {
                        // Ignore errors because events come from external systems
                        tracing::error!(
                            "handle_tracking_event for payment hash {:x} failed: {}",
                            payment_hash,
                            err
                        );
                    }
                }
                Ok(())
            }
            CchMessage::StoreChangeEvent(change) => {
                let summary = redacted_store_change_summary(&change);
                tracing::debug!(
                    "store change event kind={} payment_hash={:x} has_payment_preimage={}",
                    summary.kind,
                    summary.payment_hash,
                    summary.has_payment_preimage
                );
                let events = state.map_store_change_to_events(&change);
                for event in events {
                    let payment_hash = *event.payment_hash();
                    match state.handle_tracking_event(event).await {
                        Ok(actions) => {
                            append_actions(myself.clone(), payment_hash, actions)?;
                        }
                        Err(err) => {
                            tracing::error!(
                                "handle_tracking_event for payment hash {:x} failed: {}",
                                payment_hash,
                                err
                            );
                        }
                    }
                }
                Ok(())
            }
            CchMessage::ReconcileFiberPayments => {
                for order in state
                    .store
                    .get_cch_order_keys_iter()
                    .into_iter()
                    .filter_map(|payment_hash| state.store.get_cch_order(&payment_hash).ok())
                    .filter(|order| {
                        matches!(
                            order.status,
                            CchOrderStatus::IncomingAccepted | CchOrderStatus::OutgoingInFlight
                        )
                    })
                {
                    myself.send_message(CchMessage::ExecuteAction {
                        payment_hash: order.payment_hash,
                        action: CchOrderAction::TrackOutgoingPayment,
                        retry_count: 0,
                    })?;
                }
                Ok(())
            }
            CchMessage::ActionRetry {
                payment_hash,
                action,
                retry_count,
                reason,
            } => {
                if state
                    .get_order_for_action_or_none(&payment_hash, action)?
                    .is_none()
                {
                    return Ok(());
                }
                schedule_action_retry(&myself, payment_hash, action, retry_count, &reason);
                Ok(())
            }
            CchMessage::ExecuteAction {
                payment_hash,
                action,
                retry_count,
            } => {
                let order = match state.get_order_for_action_or_none(&payment_hash, action)? {
                    None => return Ok(()),
                    Some(order) => order,
                };
                if let Err(err) =
                    ActionDispatcher::execute(state, &myself, &order, action, retry_count).await
                {
                    schedule_action_retry(
                        &myself,
                        payment_hash,
                        action,
                        retry_count,
                        &err.to_string(),
                    );
                }
                Ok(())
            }
            CchMessage::ResumeReceiveBTCOrderCreation {
                payment_hash,
                retry_count,
            } => {
                state
                    .pending_receive_btc_creation_retries
                    .remove(&payment_hash);
                match state.resume_receive_btc_order_creation(payment_hash).await {
                    Ok(Some(order)) => {
                        state.schedule_job_on_entering(&order);
                        let actions = ActionDispatcher::on_starting(&order);
                        append_actions(myself, order.payment_hash, actions)?;
                    }
                    Ok(None) => {}
                    Err(err) if is_retryable_receive_btc_creation_error(&err) => {
                        schedule_receive_btc_creation_retry(
                            &myself,
                            &mut state.pending_receive_btc_creation_retries,
                            payment_hash,
                            retry_count,
                            &err.to_string(),
                        );
                    }
                    Err(err) => {
                        tracing::error!(
                            "Permanently failed to resume receive_btc creation {:x}: {}. The durable intent is retained for operator inspection",
                            payment_hash,
                            err
                        );
                    }
                }
                Ok(())
            }
            CchMessage::ResumeSendBTCOrderCreation {
                payment_hash,
                retry_count,
            } => {
                state
                    .pending_send_btc_creation_retries
                    .remove(&payment_hash);
                if !state.active_send_btc_creation_workers.insert(payment_hash) {
                    return Ok(());
                }
                let worker_state = state.clone();
                let myself = myself.clone();
                state.task_tracker.spawn(async move {
                    let result = worker_state
                        .resume_send_btc_order_creation(payment_hash)
                        .await;
                    let _ = myself.send_message(
                        CchMessage::ResumeSendBTCOrderCreationWorkerCompleted {
                            payment_hash,
                            retry_count,
                            result,
                        },
                    );
                });
                Ok(())
            }
            CchMessage::ResumeSendBTCOrderCreationWorkerCompleted {
                payment_hash,
                retry_count,
                result,
            } => {
                match result {
                    Ok(Some(_)) => {
                        let (order, _) =
                            state.reconcile_deferred_send_btc_invoice_status(payment_hash)?;
                        state.schedule_job_on_entering(&order);
                        let actions = ActionDispatcher::on_starting(&order);
                        append_actions(myself.clone(), order.payment_hash, actions)?;
                    }
                    Ok(None) => {
                        state
                            .deferred_send_btc_invoice_statuses
                            .remove(&payment_hash);
                    }
                    Err(err) if is_retryable_send_btc_creation_error(&err) => {
                        schedule_send_btc_creation_retry(
                            &myself,
                            &mut state.pending_send_btc_creation_retries,
                            payment_hash,
                            retry_count,
                            &err.to_string(),
                        );
                    }
                    Err(err) => {
                        tracing::error!(
                            "Permanently failed to resume send_btc creation {:x}: {}. The durable intent is retained for operator inspection",
                            payment_hash,
                            err
                        );
                        state
                            .deferred_send_btc_invoice_statuses
                            .remove(&payment_hash);
                    }
                }
                state.active_send_btc_creation_workers.remove(&payment_hash);
                Ok(())
            }
            CchMessage::ExpireOrder(payment_hash) => {
                let actions = state.expire_order(payment_hash, "Order expired")?;
                append_actions(myself, payment_hash, actions)?;
                Ok(())
            }
            #[cfg(test)]
            CchMessage::InsertOrder(order, port) => {
                let result = state.store.insert_cch_order(order).map_err(Into::into);
                if !port.is_closed() {
                    let _ = port.send(result);
                }
                Ok(())
            }
            #[cfg(test)]
            CchMessage::GetStartupRecoveryStatus(port) => {
                let _ = port.send(state.startup_recovery_in_progress);
                Ok(())
            }
        }
    }
}

/// Maps a CKB network currency to the expected Lightning Network invoice currency.
///
/// - Fibb (CKB mainnet) → Bitcoin mainnet
/// - Fibt (CKB testnet) → Bitcoin testnet
/// - Fibd (CKB devnet) → Bitcoin regtest
fn expected_ln_currency(currency: Currency) -> LnCurrency {
    match currency {
        Currency::Fibb => LnCurrency::Bitcoin,
        Currency::Fibt => LnCurrency::BitcoinTestnet,
        Currency::Fibd => LnCurrency::Regtest,
    }
}

fn receive_btc_total_msat(amount_sats: u128, fee_sats: u128) -> Result<i64, CchError> {
    i64::try_from(
        amount_sats
            .checked_add(fee_sats)
            .and_then(|sats| sats.checked_mul(1_000))
            .ok_or(CchError::ReceiveBTCOrderAmountTooLarge)?,
    )
    .map_err(|_| CchError::ReceiveBTCOrderAmountTooLarge)
}

fn lnd_invoice_mismatch(payment_hash: Hash256, reason: impl Into<String>) -> CchError {
    CchError::LndInvoiceMismatch {
        payment_hash,
        reason: reason.into(),
    }
}

fn receive_btc_order_creation_is_expired(
    creation: &CchReceiveBtcOrderCreation,
    current_time: u64,
) -> bool {
    match creation
        .created_at
        .checked_add(creation.order_expiry_delta_seconds)
    {
        Some(expiry_time) => expiry_time <= current_time,
        None => true,
    }
}

fn receive_btc_order_remaining_expiry_seconds(
    creation: &CchReceiveBtcOrderCreation,
    current_time: u64,
) -> Result<u64, CchError> {
    let expiry_time = creation
        .created_at
        .checked_add(creation.order_expiry_delta_seconds)
        .ok_or(CchError::ReceiveBTCOrderCreationExpired(
            creation.payment_hash,
        ))?;
    if expiry_time <= current_time {
        return Err(CchError::ReceiveBTCOrderCreationExpired(
            creation.payment_hash,
        ));
    }
    Ok(expiry_time - current_time)
}

fn deferred_invoice_status_advances_order(
    order_status: CchOrderStatus,
    invoice_status: CkbInvoiceStatus,
) -> bool {
    match invoice_status {
        CkbInvoiceStatus::Open => false,
        CkbInvoiceStatus::Received => order_status == CchOrderStatus::Pending,
        CkbInvoiceStatus::Cancelled | CkbInvoiceStatus::Expired => matches!(
            order_status,
            CchOrderStatus::Pending | CchOrderStatus::IncomingAccepted
        ),
        CkbInvoiceStatus::Paid => order_status == CchOrderStatus::OutgoingSuccess,
    }
}

fn invoice_status_rank(status: CkbInvoiceStatus) -> u8 {
    match status {
        CkbInvoiceStatus::Open => 0,
        CkbInvoiceStatus::Received => 1,
        CkbInvoiceStatus::Cancelled | CkbInvoiceStatus::Expired => 2,
        CkbInvoiceStatus::Paid => 3,
    }
}

impl<S: CchOrderStore> CchState<S> {
    fn recover_persisted_order(
        &mut self,
        myself: &ActorRef<CchMessage>,
        mut order: CchOrder,
    ) -> Result<(), ActorProcessingErr> {
        if order.is_final() {
            let actions = ActionDispatcher::on_starting(&order);
            if let Err(err) = append_actions(myself.clone(), order.payment_hash, actions) {
                tracing::error!(
                    "Failed to schedule final-order resume actions for order {:x}: {}",
                    order.payment_hash,
                    err
                );
            }
            self.schedule_job_for_final_order(&order);
            return Ok(());
        }

        let current_time = now_timestamp_as_millis_u64() / 1000;
        if order.update_if_expired(current_time) {
            let payment_hash = order.payment_hash;
            self.store.update_cch_order(order.clone());
            let actions = ActionDispatcher::on_entering(&order);
            append_actions(myself.clone(), payment_hash, actions)?;
            self.schedule_job_for_final_order(&order);
            tracing::info!("Marked expired order {:x} as Failed", payment_hash);
            return Ok(());
        }

        if matches!(&order.incoming_invoice, CchInvoice::Fiber(_)) {
            // Restore reservations before dispatching payment tracking. Orders created before
            // the admission limit remain recoverable even when they exceed the new limit.
            self.lnd_tracker
                .send_message(LndTrackerMessage::RestorePaymentTracking(
                    order.payment_hash,
                ))?;
        }

        self.schedule_job_for_non_final_order(&order);
        let actions = ActionDispatcher::on_starting(&order);
        if let Err(err) = append_actions(myself.clone(), order.payment_hash, actions) {
            tracing::error!(
                "Failed to schedule resume actions for order {:x}: {}",
                order.payment_hash,
                err
            );
        } else {
            tracing::debug!("Resumed tracking for active order {:x}", order.payment_hash);
        }

        Ok(())
    }

    /// Get a CCH order by payment hash, returning None if not found.
    /// This handles the common pattern of checking for NotFound vs other errors.
    fn get_order_or_none(&self, payment_hash: &Hash256) -> Result<Option<CchOrder>, CchError> {
        match self.store.get_cch_order(payment_hash) {
            Err(CchStoreError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
            Ok(mut order) => {
                order.normalize_amount_sats();
                Ok(Some(order))
            }
        }
    }

    /// Get a CCH order by payment hash, returning None if not found or the order status is final.
    fn get_active_order_or_none(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<CchOrder>, CchError> {
        Ok(self
            .get_order_or_none(payment_hash)?
            .filter(|order| !order.is_final()))
    }

    fn get_order_for_action_or_none(
        &self,
        payment_hash: &Hash256,
        action: CchOrderAction,
    ) -> Result<Option<CchOrder>, CchError> {
        Ok(self
            .get_order_or_none(payment_hash)?
            .filter(|order| action_can_still_run(order, action)))
    }

    fn expire_order(
        &self,
        payment_hash: Hash256,
        failure_reason: &str,
    ) -> Result<Vec<CchOrderAction>, CchError> {
        let mut order = match self.get_active_order_or_none(&payment_hash)? {
            None => return Ok(vec![]),
            Some(order) => order,
        };

        if order.status != CchOrderStatus::Pending {
            tracing::debug!(
                "Ignoring expiry for order {:x} in status {:?}",
                payment_hash,
                order.status
            );
            return Ok(vec![]);
        }

        order.status = CchOrderStatus::Failed;
        order.failure_reason = Some(failure_reason.to_string());
        self.store.update_cch_order(order.clone());
        let _ = self
            .lnd_tracker
            .send_message(LndTrackerMessage::StopTrackingPayment(payment_hash));
        self.schedule_job_for_final_order(&order);
        tracing::info!("Expired order {:x}", payment_hash);

        Ok(ActionDispatcher::on_entering(&order))
    }

    fn schedule_job_for_non_final_order(&self, order: &CchOrder) {
        if let Err(err) = self
            .scheduler
            .send_message(SchedulerMessage::ScheduleExpiry {
                payment_hash: order.payment_hash,
                created_at: order.created_at,
                expiry_delta_seconds: order.expiry_delta_seconds,
            })
        {
            tracing::error!(
                "Failed to schedule expiry job for order {:x}: {}",
                order.payment_hash,
                err
            );
        }
    }

    fn schedule_job_for_final_order(&self, order: &CchOrder) {
        let payment_hash = order.payment_hash;
        if let Err(err) = self
            .scheduler
            .send_message(SchedulerMessage::SchedulePrune {
                payment_hash,
                created_at: order.created_at,
                expiry_delta_seconds: order.expiry_delta_seconds,
            })
        {
            tracing::error!(
                "Failed to schedule prune job for final order {:x}: {}",
                payment_hash,
                err
            );
        }
    }

    fn schedule_job_on_entering(&self, order: &CchOrder) {
        if order.is_final() {
            // A final order no longer needs its dedicated LND payment stream. This
            // also covers terminal transitions triggered by the incoming invoice
            // or another tracker before the per-payment stream completes.
            let _ = self
                .lnd_tracker
                .send_message(LndTrackerMessage::StopTrackingPayment(order.payment_hash));
            self.schedule_job_for_final_order(order);
        } else {
            self.schedule_job_for_non_final_order(order);
        }
    }

    /// Resolve the full wrapped BTC type script.
    ///
    /// Uses the explicit `wrapped_btc_type_script` config option when set.
    /// Falls back to building the script from the contracts context using
    /// `wrapped_btc_type_script_args` (safe in non-standalone mode where the
    /// contracts context is always initialized; standalone mode enforces the
    /// config option at startup).
    fn resolve_wrapped_btc_type_script(&self) -> Result<ckb_jsonrpc_types::Script, CchError> {
        if let Some(ref json_str) = self.config.wrapped_btc_type_script {
            return serde_json::from_str(json_str).map_err(|e| {
                CchError::ConfigError(format!(
                    "failed to parse wrapped_btc_type_script JSON: {}",
                    e
                ))
            });
        }

        let args = hex::decode(
            self.config
                .wrapped_btc_type_script_args
                .trim_start_matches("0x"),
        )
        .map_err(|_| {
            CchError::HexDecodingError(self.config.wrapped_btc_type_script_args.clone())
        })?;

        Ok(get_script_by_contract(Contract::SimpleUDT, &args).into())
    }

    async fn send_btc(&self, send_btc: SendBTC) -> Result<(CchOrder, bool), CchError> {
        self.ensure_send_btc_currency(send_btc.currency)?;
        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        tracing::debug!(
            "BTC invoice parsed payment_hash={:x} currency={:?} has_amount={}",
            Hash256::from(*invoice.payment_hash()),
            invoice.currency(),
            invoice.amount_milli_satoshis().is_some()
        );
        let payment_hash = Hash256::from(*invoice.payment_hash());

        // A retry after an RPC timeout must return the durable result instead of reserving the
        // same payment tracker a second time.
        if let Some(order) = self.get_order_or_none(&payment_hash)? {
            self.ensure_send_btc_request_matches_order(&send_btc.btc_pay_req, &order)?;
            return Ok((order, false));
        }
        if let Some(creation) = self.get_send_btc_order_creation_or_none(&payment_hash)? {
            self.ensure_send_btc_request_matches_creation(&send_btc.btc_pay_req, &creation)?;
            return self
                .complete_send_btc_order_creation_with_tracking(creation)
                .await
                .map(|order| (order, true));
        }

        let order_created_at = now_timestamp_as_millis_u64() / 1000;

        // Validate that the BTC invoice network matches the expected BTC network (#978)
        let expected_ln_currency = expected_ln_currency(self.currency);
        let actual_ln_currency = invoice.currency();
        if actual_ln_currency != expected_ln_currency {
            return Err(CchError::BTCInvoiceNetworkMismatch {
                expected: format!("{:?}", expected_ln_currency),
                actual: format!("{:?}", actual_ln_currency),
            });
        }

        let invoice_created_at = invoice.duration_since_epoch().as_secs();
        if invoice_created_at > order_created_at {
            return Err(CchError::BTCInvoiceCreationTimeInFuture {
                invoice_created_at,
                order_created_at,
            });
        }

        // Validate that outgoing BTC invoice's final CLTV is less than half of incoming CKB invoice's final TLC expiry.
        // This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.
        // BTC uses blocks (~10 min each), CKB uses seconds.
        let btc_final_cltv_seconds = invoice
            .min_final_cltv_expiry_delta()
            .checked_mul(600)
            .ok_or(CchError::BTCInvoiceFinalTlcExpiryDeltaTooLarge)?;
        let ckb_final_tlc_seconds = self.config.ckb_final_tlc_expiry_delta_seconds;
        if btc_final_cltv_seconds >= ckb_final_tlc_seconds / 2 {
            return Err(CchError::BTCInvoiceFinalTlcExpiryDeltaTooLarge);
        }

        let outgoing_invoice_expiry_delta_seconds = invoice
            .expires_at()
            .and_then(|expired_at| expired_at.as_secs().checked_sub(order_created_at))
            .ok_or(CchError::BTCInvoiceExpired)?;
        let incoming_invoice_expiry_delta_seconds =
            outgoing_invoice_expiry_delta_seconds.min(self.config.order_expiry_delta_seconds);
        if incoming_invoice_expiry_delta_seconds
            < self.config.min_outgoing_invoice_expiry_delta_seconds
        {
            return Err(CchError::OutgoingInvoiceExpiryTooShort);
        }

        let amount_msat = invoice
            .amount_milli_satoshis()
            .ok_or(CchError::BTCInvoiceMissingAmount)? as u128;

        let fee_sats = amount_msat
            .checked_mul(self.config.fee_rate_per_million_sats as u128)
            .and_then(|v| v.checked_div(1_000_000_000u128))
            .and_then(|v| v.checked_add(self.config.base_fee_sats as u128))
            .ok_or(CchError::SendBTCOrderAmountTooLarge)?;

        let wrapped_btc_type_script = self.resolve_wrapped_btc_type_script()?;
        let invoice_amount_sats = amount_msat
            .div_ceil(1_000u128)
            .checked_add(fee_sats)
            .ok_or(CchError::SendBTCOrderAmountTooLarge)?;

        let incoming_invoice_deadline = order_created_at
            .checked_add(incoming_invoice_expiry_delta_seconds)
            .ok_or(CchError::SendBTCOrderCreationExpired(payment_hash))?;
        let invoice_builder = InvoiceBuilder::new(send_btc.currency)
            .amount(Some(invoice_amount_sats))
            .payment_hash(payment_hash)
            .hash_algorithm(HashAlgorithm::Sha256)
            .final_expiry_delta(
                self.config
                    .ckb_final_tlc_expiry_delta_seconds
                    .checked_mul(1000)
                    .ok_or_else(|| {
                        CchError::ConfigError(format!(
                            "ckb_final_tlc_expiry_delta_seconds ({}) is too large and causes overflow when converting to milliseconds",
                            self.config.ckb_final_tlc_expiry_delta_seconds
                        ))
                    })?,
            )
            .udt_type_script(wrapped_btc_type_script.clone().into());
        let incoming_invoice = self.build_send_btc_fiber_invoice_until(
            invoice_builder,
            payment_hash,
            incoming_invoice_deadline,
        )?;

        // Reserve tracker capacity before persisting the intent. The intent is the durable
        // boundary: after this point a lost RPC response can be reconciled by payment hash.
        self.reserve_lnd_payment_tracking(payment_hash).await?;
        let creation = CchSendBtcOrderCreation {
            created_at: order_created_at,
            order_expiry_delta_seconds: self.config.order_expiry_delta_seconds,
            btc_pay_req: send_btc.btc_pay_req,
            payment_hash,
            incoming_invoice,
            fee_sats,
            wrapped_btc_type_script,
        };
        let result = async {
            self.store
                .insert_send_btc_order_creation(creation.clone())?;
            self.complete_send_btc_order_creation(creation).await
        }
        .await;
        if result.is_err() {
            self.release_lnd_payment_tracking(payment_hash);
        }
        result.map(|order| (order, true))
    }

    async fn reserve_lnd_payment_tracking(&self, payment_hash: Hash256) -> Result<(), CchError> {
        let reservation = ractor::call!(self.lnd_tracker, |reply| {
            LndTrackerMessage::ReservePaymentTracking(payment_hash, reply)
        })
        .map_err(|err| CchError::LndPaymentTrackerError(err.to_string()))?;
        match reservation {
            PaymentTrackingReservationResult::Reserved => Ok(()),
            PaymentTrackingReservationResult::AlreadyTracked => {
                Err(CchError::LndPaymentAlreadyTracked(payment_hash))
            }
            PaymentTrackingReservationResult::CapacityExceeded => Err(
                CchError::LndPaymentTrackerCapacityExceeded(MAX_TRACKED_PAYMENTS),
            ),
        }
    }

    fn release_lnd_payment_tracking(&self, payment_hash: Hash256) {
        if let Err(err) = self
            .lnd_tracker
            .send_message(LndTrackerMessage::StopTrackingPayment(payment_hash))
        {
            tracing::warn!(
                "Failed to release payment tracker reservation for {:x}: {}",
                payment_hash,
                err
            );
        }
    }

    fn get_send_btc_order_creation_or_none(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<CchSendBtcOrderCreation>, CchError> {
        match self.store.get_send_btc_order_creation(payment_hash) {
            Err(CchStoreError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
            Ok(creation) => Ok(Some(creation)),
        }
    }

    fn ensure_send_btc_currency(&self, currency: Currency) -> Result<(), CchError> {
        if currency == self.currency {
            Ok(())
        } else {
            Err(CchError::CKBInvoiceNetworkMismatch {
                expected: self.currency,
                actual: currency,
            })
        }
    }

    fn ensure_send_btc_request_matches_order(
        &self,
        btc_pay_req: &str,
        order: &CchOrder,
    ) -> Result<(), CchError> {
        if order.outgoing_pay_req == btc_pay_req
            && matches!(order.incoming_invoice, CchInvoice::Fiber(_))
        {
            Ok(())
        } else {
            Err(CchError::ConflictingSendBTCRequest(order.payment_hash))
        }
    }

    fn ensure_send_btc_request_matches_creation(
        &self,
        btc_pay_req: &str,
        creation: &CchSendBtcOrderCreation,
    ) -> Result<(), CchError> {
        if creation.btc_pay_req == btc_pay_req {
            Ok(())
        } else {
            Err(CchError::ConflictingSendBTCRequest(creation.payment_hash))
        }
    }

    async fn resume_send_btc_order_creation(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<CchOrder>, CchError> {
        if self.get_order_or_none(&payment_hash)?.is_some() {
            // The order is authoritative if a crash happened after its batch commit but before
            // the actor observed completion.
            self.store.delete_send_btc_order_creation(&payment_hash);
            return Ok(None);
        }
        let Some(creation) = self.get_send_btc_order_creation_or_none(&payment_hash)? else {
            return Ok(None);
        };
        self.complete_send_btc_order_creation_with_tracking(creation)
            .await
            .map(Some)
    }

    async fn complete_send_btc_order_creation_with_tracking(
        &self,
        creation: CchSendBtcOrderCreation,
    ) -> Result<CchOrder, CchError> {
        let payment_hash = creation.payment_hash;
        self.reserve_lnd_payment_tracking(payment_hash).await?;
        let result = self.complete_send_btc_order_creation(creation).await;
        if result.is_err() {
            self.release_lnd_payment_tracking(payment_hash);
        }
        result
    }

    async fn complete_send_btc_order_creation(
        &self,
        creation: CchSendBtcOrderCreation,
    ) -> Result<CchOrder, CchError> {
        let payment_hash = creation.payment_hash;
        let invoice_info = match self.fiber_agent_ref.call_get_invoice(payment_hash).await? {
            Some(info) => info,
            None => {
                let invoice = self.refresh_send_btc_fiber_invoice(&creation)?;
                let deadline_seconds = self.send_btc_creation_deadline(&creation)?;
                match self
                    .fiber_agent_ref
                    .call_add_invoice(
                        invoice,
                        deadline_seconds,
                        self.config.min_outgoing_invoice_expiry_delta_seconds,
                    )
                    .await
                {
                    Ok(invoice) => crate::cch::cch_fiber_agent::FiberInvoiceInfo {
                        invoice,
                        status: CkbInvoiceStatus::Open,
                    },
                    Err(add_error) => {
                        match self.fiber_agent_ref.call_get_invoice(payment_hash).await? {
                            Some(info) => info,
                            None => return Err(add_error),
                        }
                    }
                }
            }
        };
        let incoming_invoice = invoice_info.invoice;
        self.ensure_send_btc_fiber_invoice_matches(&creation, &incoming_invoice)?;

        let (status, failure_reason) = match invoice_info.status {
            CkbInvoiceStatus::Open => (CchOrderStatus::Pending, None),
            CkbInvoiceStatus::Received => (CchOrderStatus::IncomingAccepted, None),
            CkbInvoiceStatus::Cancelled => (
                CchOrderStatus::Failed,
                Some("Fiber invoice was cancelled before order recovery".to_string()),
            ),
            CkbInvoiceStatus::Expired => (
                CchOrderStatus::Failed,
                Some("Fiber invoice expired before order recovery".to_string()),
            ),
            CkbInvoiceStatus::Paid => {
                return Err(CchError::FiberInvoiceMismatch(payment_hash));
            }
        };

        let amount_sats = incoming_invoice
            .amount
            .ok_or(CchError::SendBTCOrderAmountTooLarge)?;
        let order = CchOrder {
            amount_sats,
            created_at: creation.created_at,
            expiry_delta_seconds: creation.order_expiry_delta_seconds,
            failure_reason,
            incoming_invoice: CchInvoice::Fiber(incoming_invoice),
            outgoing_pay_req: creation.btc_pay_req.clone(),
            payment_preimage: None,
            status,
            fee_sats: creation.fee_sats,
            payment_hash,
            wrapped_btc_type_script: creation.wrapped_btc_type_script,
        };

        match self.store.complete_send_btc_order_creation(order.clone()) {
            Ok(()) => Ok(order),
            Err(CchStoreError::Duplicated(_)) => {
                let existing = self.store.get_cch_order(&payment_hash)?;
                self.ensure_send_btc_request_matches_order(&creation.btc_pay_req, &existing)?;
                self.store.delete_send_btc_order_creation(&payment_hash);
                Ok(existing)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn ensure_send_btc_fiber_invoice_matches(
        &self,
        creation: &CchSendBtcOrderCreation,
        invoice: &CkbInvoice,
    ) -> Result<(), CchError> {
        let deadline_millis = self
            .send_btc_creation_deadline(creation)?
            .checked_mul(1_000)
            .map(u128::from)
            .ok_or(CchError::SendBTCOrderCreationExpired(creation.payment_hash))?;
        let invoice_expiry_millis = invoice
            .expiry_time()
            .and_then(|expiry| invoice.data.timestamp.checked_add(expiry.as_millis()));
        if *invoice.payment_hash() == creation.payment_hash
            && invoice.currency == creation.incoming_invoice.currency
            && invoice.amount == creation.incoming_invoice.amount
            && invoice.hash_algorithm().copied().unwrap_or_default()
                == creation
                    .incoming_invoice
                    .hash_algorithm()
                    .copied()
                    .unwrap_or_default()
            && invoice.final_tlc_minimum_expiry_delta_or_default()
                == creation
                    .incoming_invoice
                    .final_tlc_minimum_expiry_delta_or_default()
            && invoice.udt_type_script() == creation.incoming_invoice.udt_type_script()
            && invoice_expiry_millis.is_some_and(|expiry| expiry <= deadline_millis)
        {
            Ok(())
        } else {
            Err(CchError::FiberInvoiceMismatch(creation.payment_hash))
        }
    }

    fn send_btc_creation_deadline(
        &self,
        creation: &CchSendBtcOrderCreation,
    ) -> Result<u64, CchError> {
        let btc_deadline = Bolt11Invoice::from_str(&creation.btc_pay_req)?
            .expires_at()
            .map(|expiry| expiry.as_secs())
            .ok_or(CchError::SendBTCOrderCreationExpired(creation.payment_hash))?;
        let order_deadline = creation
            .created_at
            .checked_add(creation.order_expiry_delta_seconds)
            .ok_or(CchError::SendBTCOrderCreationExpired(creation.payment_hash))?;
        Ok(btc_deadline.min(order_deadline))
    }

    fn refresh_send_btc_fiber_invoice(
        &self,
        creation: &CchSendBtcOrderCreation,
    ) -> Result<CkbInvoice, CchError> {
        let template = &creation.incoming_invoice;
        let mut builder = InvoiceBuilder::new(template.currency)
            .amount(template.amount)
            .payment_hash(creation.payment_hash)
            .hash_algorithm(template.hash_algorithm().copied().unwrap_or_default())
            .final_expiry_delta(template.final_tlc_minimum_expiry_delta_or_default());
        if let Some(script) = template.udt_type_script() {
            builder = builder.udt_type_script(script.clone());
        }
        self.build_send_btc_fiber_invoice_until(
            builder,
            creation.payment_hash,
            self.send_btc_creation_deadline(creation)?,
        )
    }

    fn build_send_btc_fiber_invoice_until(
        &self,
        mut builder: InvoiceBuilder,
        payment_hash: Hash256,
        deadline_seconds: u64,
    ) -> Result<CkbInvoice, CchError> {
        if let Some((public_key, _)) = &self.node_keypair {
            builder = builder.payee_pub_key(*public_key);
        }

        // Invoice timestamps are millisecond-precise while expiry attributes contain whole
        // seconds. Fix the timestamp first, then round the remaining lifetime down so the
        // resulting absolute expiry can never extend past the BTC/order deadline.
        let mut invoice = builder.build()?;
        let deadline_millis = u128::from(deadline_seconds)
            .checked_mul(1_000)
            .ok_or(CchError::SendBTCOrderCreationExpired(payment_hash))?;
        let remaining_seconds = deadline_millis
            .checked_sub(invoice.data.timestamp)
            .and_then(|remaining| u64::try_from(remaining / 1_000).ok())
            .ok_or(CchError::SendBTCOrderCreationExpired(payment_hash))?;
        let required_expiry_seconds = self
            .config
            .min_outgoing_invoice_expiry_delta_seconds
            .checked_add(self.fiber_agent_ref.add_invoice_expiry_margin_seconds())
            .ok_or(CchError::SendBTCOrderCreationExpired(payment_hash))?;
        if remaining_seconds < required_expiry_seconds {
            return Err(CchError::SendBTCOrderCreationExpired(payment_hash));
        }
        invoice
            .data
            .attrs
            .push(Attribute::ExpiryTime(Duration::from_secs(
                remaining_seconds,
            )));

        if let Some((_, secret_key)) = &self.node_keypair {
            invoice.update_signature(|hash| SECP256K1.sign_ecdsa_recoverable(hash, secret_key))?;
        }
        Ok(invoice)
    }

    fn remaining_outgoing_invoice_expiry_seconds(
        &self,
        invoice: &CkbInvoice,
        current_time_seconds: u64,
    ) -> Result<u64, CchError> {
        let remaining_seconds = match invoice.expiry_time() {
            Some(expiry) => invoice
                .data
                .timestamp
                .checked_add(expiry.as_millis())
                .and_then(|expiry_at| {
                    u64::try_from(expiry_at / 1000)
                        .unwrap_or(u64::MAX)
                        .checked_sub(current_time_seconds)
                })
                .ok_or(CchError::OutgoingInvoiceExpiryTooShort)?,
            // CKB invoices have no default expiry, so use twice the configured minimum when the
            // outgoing invoice does not set one explicitly.
            None => self
                .config
                .min_outgoing_invoice_expiry_delta_seconds
                .checked_mul(2)
                .ok_or(CchError::OutgoingInvoiceExpiryTooShort)?,
        };

        if remaining_seconds < self.config.min_outgoing_invoice_expiry_delta_seconds {
            return Err(CchError::OutgoingInvoiceExpiryTooShort);
        }
        Ok(remaining_seconds)
    }

    async fn receive_btc(&self, receive_btc: ReceiveBTC) -> Result<(CchOrder, bool), CchError> {
        // `from_str` requires the invoice to carry a valid signature, so parsing
        // here also guarantees the Fiber invoice is signed.
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;

        let payment_hash = *invoice.payment_hash();
        if let Some(order) = self.get_order_or_none(&payment_hash)? {
            self.ensure_receive_btc_request_matches_order(&receive_btc.fiber_pay_req, &order)?;
            return Ok((order, false));
        }
        if let Some(creation) = self.get_receive_btc_order_creation_or_none(&payment_hash)? {
            self.ensure_receive_btc_request_matches_creation(
                &receive_btc.fiber_pay_req,
                &creation,
            )?;
            return self
                .complete_receive_btc_order_creation_with_tracking(creation, false)
                .await
                .map(|order| (order, true));
        }

        // Validate that the CKB invoice currency matches the configured network (#982)
        if invoice.currency != self.currency {
            return Err(CchError::CKBInvoiceNetworkMismatch {
                expected: self.currency,
                actual: invoice.currency,
            });
        }

        let amount_sats = invoice.amount().ok_or(CchError::CKBInvoiceMissingAmount)?;

        // Validate amount and fee early so we reject overflow/too-large before other checks.
        let fee_sats = amount_sats
            .checked_mul(self.config.fee_rate_per_million_sats as u128)
            .and_then(|v| v.checked_div(1_000_000u128))
            .and_then(|v| v.checked_add(self.config.base_fee_sats as u128))
            .ok_or(CchError::ReceiveBTCOrderAmountTooLarge)?;
        receive_btc_total_msat(amount_sats, fee_sats)?;

        // Validate that outgoing CKB invoice's final TLC is less than half of incoming BTC invoice's final CLTV expiry.
        // This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.
        // CKB uses milliseconds, BTC uses blocks (~10 min each).
        let ckb_final_tlc_millis = invoice.final_tlc_minimum_expiry_delta_or_default();
        let btc_final_cltv_millis = self
            .config
            .btc_final_tlc_expiry_delta_blocks
            .checked_mul(BTC_BLOCK_TIME_MILLIS)
            .ok_or_else(|| {
                CchError::ConfigError(format!(
                    "btc_final_tlc_expiry_delta_blocks ({}) is too large and causes overflow when converting to milliseconds",
                    self.config.btc_final_tlc_expiry_delta_blocks
                ))
            })?;
        if ckb_final_tlc_millis >= btc_final_cltv_millis / 2 {
            return Err(CchError::CKBInvoiceFinalTlcExpiryDeltaTooLarge);
        }

        let order_created_at_ms = now_timestamp_as_millis_u64();
        let order_created_at = order_created_at_ms / 1000;
        if invoice.data.timestamp > u128::from(order_created_at_ms) {
            return Err(CchError::CKBInvoiceCreationTimeInFuture {
                invoice_created_at_ms: invoice.data.timestamp,
                order_created_at_ms: u128::from(order_created_at_ms),
            });
        }

        self.remaining_outgoing_invoice_expiry_seconds(&invoice, order_created_at)?;

        // Verify wrapped_btc_type_script matches invoice UDT type script
        let wrapped_btc_type_script = self.resolve_wrapped_btc_type_script()?;

        // Verify invoice UDT type script matches configured wrapped_btc_type_script
        if let Some(invoice_udt_script) = invoice.udt_type_script() {
            let invoice_script: ckb_jsonrpc_types::Script = invoice_udt_script.clone().into();
            if invoice_script.code_hash != wrapped_btc_type_script.code_hash
                || invoice_script.hash_type != wrapped_btc_type_script.hash_type
                || invoice_script.args != wrapped_btc_type_script.args
            {
                return Err(CchError::WrappedBTCTypescriptMismatch);
            }
        } else {
            return Err(CchError::WrappedBTCTypescriptMismatch);
        }

        // Validate hash algorithm - must be SHA256 for LND compatibility
        let hash_algorithm = invoice.hash_algorithm().copied().unwrap_or_default();
        if hash_algorithm != HashAlgorithm::Sha256 {
            return Err(CchError::CKBInvoiceIncompatibleHashAlgorithm);
        }

        // Do not create externally payable BTC-side state for an invoice Fiber cannot currently
        // route. The actual payment is still attempted only after the hold invoice is accepted;
        // this dry run is a side-effect-free snapshot that rejects already-invalid orders early.
        let fee_budget_sats =
            outgoing_fee_budget_from_fee_sats(fee_sats, self.config.max_outgoing_fee_percentage);
        self.fiber_agent_ref
            .call_payment_preflight(
                receive_btc.fiber_pay_req.clone(),
                (btc_final_cltv_millis / 2).min(MAX_PAYMENT_TLC_EXPIRY_LIMIT),
                OutgoingFeeLimit {
                    max_fee_amount: fee_budget_sats,
                    max_fee_rate: outgoing_max_fee_rate(amount_sats, fee_budget_sats),
                },
            )
            .await?;

        // Preflight can take long enough for the absolute Fiber invoice expiry to move
        // materially closer. Recheck before persisting the intent: no LND side effect has
        // happened yet, so an invoice that became too short can still be rejected cleanly.
        let refreshed_at = now_timestamp_as_millis_u64() / 1000;
        self.remaining_outgoing_invoice_expiry_seconds(&invoice, refreshed_at)?;

        // Reserve tracker capacity before persisting the intent. A capacity failure has no
        // external side effect and must not leave an intent that blocks a later client retry.
        self.reserve_lnd_invoice_tracking(payment_hash).await?;

        let creation = CchReceiveBtcOrderCreation {
            created_at: order_created_at,
            order_expiry_delta_seconds: self.config.order_expiry_delta_seconds,
            fiber_pay_req: receive_btc.fiber_pay_req,
            payment_hash,
            amount_sats,
            fee_sats,
            wrapped_btc_type_script,
            btc_final_tlc_expiry_delta_blocks: self.config.btc_final_tlc_expiry_delta_blocks,
            max_outgoing_fee_percentage: self.config.max_outgoing_fee_percentage,
        };
        let result = async {
            self.store
                .insert_receive_btc_order_creation(creation.clone())?;
            self.complete_receive_btc_order_creation(creation, true)
                .await
        }
        .await;
        if result.is_err() {
            self.release_lnd_invoice_tracking(payment_hash);
        }
        result.map(|order| (order, true))
    }

    fn get_receive_btc_order_creation_or_none(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<CchReceiveBtcOrderCreation>, CchError> {
        match self.store.get_receive_btc_order_creation(payment_hash) {
            Err(CchStoreError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
            Ok(creation) => Ok(Some(creation)),
        }
    }

    fn ensure_receive_btc_request_matches_order(
        &self,
        fiber_pay_req: &str,
        order: &CchOrder,
    ) -> Result<(), CchError> {
        if order.outgoing_pay_req == fiber_pay_req
            && matches!(order.incoming_invoice, CchInvoice::Lightning(_))
        {
            Ok(())
        } else {
            Err(CchError::ConflictingReceiveBTCRequest(order.payment_hash))
        }
    }

    fn ensure_receive_btc_request_matches_creation(
        &self,
        fiber_pay_req: &str,
        creation: &CchReceiveBtcOrderCreation,
    ) -> Result<(), CchError> {
        if creation.fiber_pay_req == fiber_pay_req {
            Ok(())
        } else {
            Err(CchError::ConflictingReceiveBTCRequest(
                creation.payment_hash,
            ))
        }
    }

    async fn resume_receive_btc_order_creation(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<CchOrder>, CchError> {
        if self.get_order_or_none(&payment_hash)?.is_some() {
            // A crash after committing the order but before observing the completed batch is safe:
            // the order is authoritative and any stale intent can be discarded.
            self.store.delete_receive_btc_order_creation(&payment_hash);
            return Ok(None);
        }
        let Some(creation) = self.get_receive_btc_order_creation_or_none(&payment_hash)? else {
            return Ok(None);
        };
        self.complete_receive_btc_order_creation_with_tracking(creation, false)
            .await
            .map(Some)
    }

    async fn reserve_lnd_invoice_tracking(&self, payment_hash: Hash256) -> Result<(), CchError> {
        let reservation = ractor::call!(self.lnd_tracker, |reply| {
            LndTrackerMessage::ReserveInvoiceTracking(payment_hash, reply)
        })
        .map_err(|err| CchError::LndInvoiceTrackerError(err.to_string()))?;
        match reservation {
            InvoiceTrackingReservationResult::Reserved => Ok(()),
            InvoiceTrackingReservationResult::AlreadyTracked => {
                Err(CchError::LndInvoiceAlreadyTracked(payment_hash))
            }
            InvoiceTrackingReservationResult::CapacityExceeded => Err(
                CchError::LndInvoiceTrackerCapacityExceeded(MAX_TRACKED_INVOICES),
            ),
        }
    }

    fn release_lnd_invoice_tracking(&self, payment_hash: Hash256) {
        if let Err(err) = self
            .lnd_tracker
            .send_message(LndTrackerMessage::StopTracking(payment_hash))
        {
            tracing::warn!(
                "Failed to release invoice tracker reservation for {:x}: {}",
                payment_hash,
                err
            );
        }
    }

    async fn complete_receive_btc_order_creation_with_tracking(
        &self,
        creation: CchReceiveBtcOrderCreation,
        preflight_complete: bool,
    ) -> Result<CchOrder, CchError> {
        let payment_hash = creation.payment_hash;
        self.reserve_lnd_invoice_tracking(payment_hash).await?;
        let result = self
            .complete_receive_btc_order_creation(creation, preflight_complete)
            .await;
        if result.is_err() {
            self.release_lnd_invoice_tracking(payment_hash);
        }
        result
    }

    async fn preflight_receive_btc_creation(
        &self,
        creation: &CchReceiveBtcOrderCreation,
    ) -> Result<(), CchError> {
        let btc_final_cltv_millis = creation
            .btc_final_tlc_expiry_delta_blocks
            .checked_mul(BTC_BLOCK_TIME_MILLIS)
            .ok_or_else(|| {
                CchError::ConfigError(format!(
                    "btc_final_tlc_expiry_delta_blocks ({}) is too large and causes overflow when converting to milliseconds",
                    creation.btc_final_tlc_expiry_delta_blocks
                ))
            })?;
        let fee_budget_sats = outgoing_fee_budget_from_fee_sats(
            creation.fee_sats,
            creation.max_outgoing_fee_percentage,
        );
        self.fiber_agent_ref
            .call_payment_preflight(
                creation.fiber_pay_req.clone(),
                (btc_final_cltv_millis / 2).min(MAX_PAYMENT_TLC_EXPIRY_LIMIT),
                OutgoingFeeLimit {
                    max_fee_amount: fee_budget_sats,
                    max_fee_rate: outgoing_max_fee_rate(creation.amount_sats, fee_budget_sats),
                },
            )
            .await?;
        Ok(())
    }

    async fn complete_receive_btc_order_creation(
        &self,
        creation: CchReceiveBtcOrderCreation,
        preflight_complete: bool,
    ) -> Result<CchOrder, CchError> {
        let (incoming_invoice, initial_status) = match self
            .lnd_invoice_client
            .lookup_invoice(creation.payment_hash)
            .await?
        {
            Some(invoice) => {
                let initial_status = match invoice.state() {
                    lnrpc::invoice::InvoiceState::Accepted => CchOrderStatus::IncomingAccepted,
                    _ => CchOrderStatus::Pending,
                };
                (
                    self.validate_recovered_lnd_invoice(&creation, invoice)?,
                    initial_status,
                )
            }
            None if receive_btc_order_creation_is_expired(
                &creation,
                now_timestamp_as_millis_u64() / 1000,
            ) =>
            {
                return Err(CchError::ReceiveBTCOrderCreationExpired(
                    creation.payment_hash,
                ));
            }
            None => {
                if !preflight_complete {
                    self.preflight_receive_btc_creation(&creation).await?;
                }
                (
                    self.create_or_recover_lnd_invoice(&creation).await?,
                    CchOrderStatus::Pending,
                )
            }
        };

        // `creation.amount_sats` is the outgoing Fiber principal. Public CCH order amounts
        // represent what the incoming Lightning payer owes, including the CCH fee.
        let total_sats = creation
            .amount_sats
            .checked_add(creation.fee_sats)
            .ok_or(CchError::ReceiveBTCOrderAmountTooLarge)?;
        let order = CchOrder {
            created_at: creation.created_at,
            expiry_delta_seconds: creation.order_expiry_delta_seconds,
            failure_reason: None,
            incoming_invoice: CchInvoice::Lightning(incoming_invoice),
            outgoing_pay_req: creation.fiber_pay_req.clone(),
            payment_preimage: None,
            status: initial_status,
            amount_sats: total_sats,
            fee_sats: creation.fee_sats,
            payment_hash: creation.payment_hash,
            wrapped_btc_type_script: creation.wrapped_btc_type_script,
        };

        match self
            .store
            .complete_receive_btc_order_creation(order.clone())
        {
            Ok(()) => Ok(order),
            Err(CchStoreError::Duplicated(_)) => {
                let existing = self.store.get_cch_order(&creation.payment_hash)?;
                self.ensure_receive_btc_request_matches_order(&creation.fiber_pay_req, &existing)?;
                self.store
                    .delete_receive_btc_order_creation(&creation.payment_hash);
                Ok(existing)
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn create_or_recover_lnd_invoice(
        &self,
        creation: &CchReceiveBtcOrderCreation,
    ) -> Result<Bolt11Invoice, CchError> {
        if receive_btc_order_creation_is_expired(creation, now_timestamp_as_millis_u64() / 1000) {
            return Err(CchError::ReceiveBTCOrderCreationExpired(
                creation.payment_hash,
            ));
        }
        let fiber_invoice = CkbInvoice::from_str(&creation.fiber_pay_req)?;
        let refreshed_at = now_timestamp_as_millis_u64() / 1000;
        let fiber_expiry =
            self.remaining_outgoing_invoice_expiry_seconds(&fiber_invoice, refreshed_at)?;
        let order_expiry = receive_btc_order_remaining_expiry_seconds(creation, refreshed_at)?;
        let expiry = fiber_expiry.min(order_expiry);
        let total_msat = receive_btc_total_msat(creation.amount_sats, creation.fee_sats)?;
        let request = invoicesrpc::AddHoldInvoiceRequest {
            hash: creation.payment_hash.as_ref().to_vec(),
            value_msat: total_msat,
            expiry: expiry as i64,
            cltv_expiry: creation.btc_final_tlc_expiry_delta_blocks,
            ..Default::default()
        };

        match self
            .lnd_invoice_client
            .add_hold_invoice(request.clone())
            .await
        {
            Ok(response) => self.validate_lnd_payment_request(creation, &response.payment_request),
            Err(add_error) => {
                // AddHoldInvoice is not transactional with this process. Any transport error is
                // ambiguous, so reconcile by hash before reporting the original failure.
                match self
                    .lnd_invoice_client
                    .lookup_invoice(creation.payment_hash)
                    .await
                {
                    Ok(Some(invoice)) => self.validate_recovered_lnd_invoice(creation, invoice),
                    Ok(None) => Err(add_error),
                    Err(lookup_error) => Err(CchError::LndRpcError(format!(
                        "{}; reconciliation lookup failed: {}",
                        add_error, lookup_error
                    ))),
                }
            }
        }
    }

    fn validate_recovered_lnd_invoice(
        &self,
        creation: &CchReceiveBtcOrderCreation,
        invoice: lnrpc::Invoice,
    ) -> Result<Bolt11Invoice, CchError> {
        let expected_total_msat = receive_btc_total_msat(creation.amount_sats, creation.fee_sats)?;
        if invoice.r_hash.as_slice() != creation.payment_hash.as_ref() {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                "payment hash differs",
            ));
        }
        if invoice.value_msat != expected_total_msat {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                format!(
                    "value_msat is {}, expected {}",
                    invoice.value_msat, expected_total_msat
                ),
            ));
        }
        if invoice.cltv_expiry != creation.btc_final_tlc_expiry_delta_blocks {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                format!(
                    "cltv_expiry is {}, expected {}",
                    invoice.cltv_expiry, creation.btc_final_tlc_expiry_delta_blocks
                ),
            ));
        }
        if matches!(
            invoice.state(),
            lnrpc::invoice::InvoiceState::Canceled | lnrpc::invoice::InvoiceState::Settled
        ) {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                format!("invoice is already {:?}", invoice.state()),
            ));
        }
        self.validate_lnd_payment_request(creation, &invoice.payment_request)
    }

    fn validate_lnd_payment_request(
        &self,
        creation: &CchReceiveBtcOrderCreation,
        payment_request: &str,
    ) -> Result<Bolt11Invoice, CchError> {
        let invoice = Bolt11Invoice::from_str(payment_request)?;
        let expected_total_msat = u64::try_from(receive_btc_total_msat(
            creation.amount_sats,
            creation.fee_sats,
        )?)
        .map_err(|_| CchError::ReceiveBTCOrderAmountTooLarge)?;
        if Hash256::from(*invoice.payment_hash()) != creation.payment_hash {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                "encoded payment hash differs",
            ));
        }
        if invoice.amount_milli_satoshis() != Some(expected_total_msat) {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                format!(
                    "encoded amount is {:?}, expected {}",
                    invoice.amount_milli_satoshis(),
                    expected_total_msat
                ),
            ));
        }
        if invoice.min_final_cltv_expiry_delta() != creation.btc_final_tlc_expiry_delta_blocks {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                format!(
                    "encoded CLTV delta is {}, expected {}",
                    invoice.min_final_cltv_expiry_delta(),
                    creation.btc_final_tlc_expiry_delta_blocks
                ),
            ));
        }
        let fiber_invoice = CkbInvoice::from_str(&creation.fiber_pay_req)?;
        let expected_currency = expected_ln_currency(fiber_invoice.currency);
        if invoice.currency() != expected_currency {
            return Err(lnd_invoice_mismatch(
                creation.payment_hash,
                format!(
                    "encoded currency is {:?}, expected {:?}",
                    invoice.currency(),
                    expected_currency
                ),
            ));
        }
        Ok(invoice)
    }

    fn apply_tracking_event_to_order(
        &self,
        mut order: CchOrder,
        event: CchTrackingEvent,
    ) -> std::result::Result<Option<CchOrder>, CchError> {
        if order.status == CchOrderStatus::Pending
            && matches!(
                &event,
                CchTrackingEvent::InvoiceChanged {
                    status: CkbInvoiceStatus::Received,
                    ..
                }
            )
        {
            let current_time = now_timestamp_as_millis_u64() / 1000;
            if order.update_if_expired_with_reason(current_time, "Order expired") {
                self.store.update_cch_order(order.clone());
                tracing::info!(
                    "Rejected incoming invoice event for expired pending order {:x}",
                    order.payment_hash
                );
                return Ok(Some(order));
            }
        }

        if CchOrderStateMachine::apply(&mut order, event.into())?.is_some() {
            self.store.update_cch_order(order.clone());
            Ok(Some(order))
        } else {
            Ok(None)
        }
    }

    fn reconcile_deferred_send_btc_invoice_status(
        &mut self,
        payment_hash: Hash256,
    ) -> Result<(CchOrder, bool), CchError> {
        let order = self
            .get_order_or_none(&payment_hash)?
            .ok_or(CchStoreError::NotFound(payment_hash))?;
        let Some(deferred) = self
            .deferred_send_btc_invoice_statuses
            .remove(&payment_hash)
        else {
            return Ok((order, false));
        };

        if !deferred_invoice_status_advances_order(order.status, deferred.status) {
            return Ok((order, false));
        }
        let event = CchTrackingEvent::InvoiceChanged {
            payment_hash,
            status: deferred.status,
            failure_reason: deferred.failure_reason,
        };
        match self.apply_tracking_event_to_order(order.clone(), event)? {
            Some(updated_order) => Ok((updated_order, true)),
            None => Ok((order, false)),
        }
    }

    async fn handle_tracking_event(
        &mut self,
        event: CchTrackingEvent,
    ) -> Result<Vec<CchOrderAction>> {
        let payment_hash = *event.payment_hash();
        if self
            .active_send_btc_creation_workers
            .contains(&payment_hash)
            || self
                .pending_send_btc_creation_retries
                .contains(&payment_hash)
        {
            if let CchTrackingEvent::InvoiceChanged {
                status,
                failure_reason,
                ..
            } = &event
            {
                let deferred = DeferredInvoiceStatus {
                    status: *status,
                    failure_reason: failure_reason.clone(),
                };
                self.deferred_send_btc_invoice_statuses
                    .entry(payment_hash)
                    .and_modify(|current| current.merge(deferred.clone()))
                    .or_insert(deferred);
                return Ok(vec![]);
            }
        }

        let order = match self.get_active_order_or_none(&payment_hash)? {
            None => return Ok(vec![]),
            Some(order) => order,
        };

        if let Some(order) = self.apply_tracking_event_to_order(order, event)? {
            self.schedule_job_on_entering(&order);
            Ok(ActionDispatcher::on_entering(&order))
        } else {
            Ok(vec![])
        }
    }

    /// Map a StoreChange into CchTrackingEvent(s).
    /// This replaces the old CchFiberStoreWatcher: the mapping logic now lives in the actor.
    fn map_store_change_to_events(&self, change: &StoreChange) -> Vec<CchTrackingEvent> {
        match change {
            StoreChange::PutCkbInvoiceStatus {
                payment_hash,
                invoice_status,
            } => vec![CchTrackingEvent::InvoiceChanged {
                payment_hash: *payment_hash,
                status: *invoice_status,
                failure_reason: None,
            }],
            StoreChange::PutPaymentSession {
                payment_hash,
                payment_session,
                payment_preimage,
            } => {
                let status = payment_session.status;
                vec![CchTrackingEvent::PaymentChanged {
                    payment_hash: *payment_hash,
                    payment_preimage: *payment_preimage,
                    status,
                    failure_reason: None,
                }]
            }
            StoreChange::PutAttempt {
                payment_hash,
                attempt_status: AttemptStatus::Inflight,
            } => {
                use fiber_types::payment::PaymentStatus;
                vec![CchTrackingEvent::PaymentChanged {
                    payment_hash: *payment_hash,
                    payment_preimage: None,
                    status: PaymentStatus::Inflight,
                    failure_reason: None,
                }]
            }
            StoreChange::PutAttempt { .. } => vec![],
            // Preimages are global to a Fiber node and can be learned from unrelated TLCs
            // that reuse the same payment hash. Only the correlated PaymentSession success
            // above is authoritative for a CCH outgoing payment.
            StoreChange::PutPreimage { .. } => vec![],
        }
    }
}

fn action_can_still_run(order: &CchOrder, action: CchOrderAction) -> bool {
    match action {
        // Most actions are stale once the order reaches a final state. Incoming invoice
        // cancellation is the exception: it is the cleanup action for a failed order.
        CchOrderAction::CancelIncomingInvoice => should_cancel_incoming_invoice(order),
        _ => order_allows_active_action(order),
    }
}

fn order_allows_active_action(order: &CchOrder) -> bool {
    !order.is_final()
}

fn should_cancel_incoming_invoice(order: &CchOrder) -> bool {
    // If the preimage exists, the incoming payment can be settled and must not be cancelled.
    order.status == CchOrderStatus::Failed && order.payment_preimage.is_none()
}

fn append_actions(
    myself: ActorRef<CchMessage>,
    payment_hash: Hash256,
    actions: Vec<CchOrderAction>,
) -> Result<(), ActorProcessingErr> {
    for action in actions {
        myself.send_message(CchMessage::ExecuteAction {
            payment_hash,
            action,
            retry_count: 0,
        })?;
    }
    Ok(())
}

fn is_retryable_receive_btc_creation_error(error: &CchError) -> bool {
    matches!(
        error,
        CchError::LndChannelError(_)
            | CchError::LndRpcError(_)
            | CchError::LndInvoiceTrackerCapacityExceeded(_)
            | CchError::FiberNodeError(_)
    )
}

fn is_retryable_send_btc_creation_error(error: &CchError) -> bool {
    matches!(
        error,
        CchError::LndPaymentTrackerError(_)
            | CchError::LndPaymentTrackerCapacityExceeded(_)
            | CchError::FiberNodeError(_)
    )
}

fn schedule_receive_btc_creation_retry(
    myself: &ActorRef<CchMessage>,
    pending_retries: &mut HashSet<Hash256>,
    payment_hash: Hash256,
    retry_count: u32,
    reason: &str,
) {
    if !pending_retries.insert(payment_hash) {
        tracing::debug!(
            "receive_btc creation retry for payment hash {:x} is already pending",
            payment_hash
        );
        return;
    }
    let delay = calculate_retry_delay(retry_count);
    tracing::error!(
        "receive_btc creation for payment hash {:x} failed (retry {}): {}. Retrying in {:?}",
        payment_hash,
        retry_count,
        reason,
        delay
    );
    myself.send_after(delay, move || CchMessage::ResumeReceiveBTCOrderCreation {
        payment_hash,
        retry_count: retry_count.saturating_add(1),
    });
}

fn schedule_send_btc_creation_retry(
    myself: &ActorRef<CchMessage>,
    pending_retries: &mut HashSet<Hash256>,
    payment_hash: Hash256,
    retry_count: u32,
    reason: &str,
) {
    if !pending_retries.insert(payment_hash) {
        tracing::debug!(
            "send_btc creation retry for payment hash {:x} is already pending",
            payment_hash
        );
        return;
    }
    let delay = calculate_retry_delay(retry_count);
    tracing::error!(
        "send_btc creation for payment hash {:x} failed (retry {}): {}. Retrying in {:?}",
        payment_hash,
        retry_count,
        reason,
        delay
    );
    myself.send_after(delay, move || CchMessage::ResumeSendBTCOrderCreation {
        payment_hash,
        retry_count: retry_count.saturating_add(1),
    });
}

fn schedule_action_retry(
    myself: &ActorRef<CchMessage>,
    payment_hash: Hash256,
    action: CchOrderAction,
    retry_count: u32,
    reason: &str,
) {
    let delay = calculate_retry_delay(retry_count);
    tracing::error!(
        "action {:?} for payment hash {:x} failed (retry {}): {}. Retrying in {:?}",
        action,
        payment_hash,
        retry_count,
        reason,
        delay
    );
    // Retry the action later with exponential backoff. The action executor will
    // cease retrying only when it handles the error internally and returns OK.
    myself.send_after(delay, move || CchMessage::ExecuteAction {
        payment_hash,
        action,
        retry_count: retry_count.saturating_add(1),
    });
}

fn ensure_fiber_http_url(url_opt: Option<String>) -> Result<String> {
    if let Some(url) = url_opt {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(url);
        }
    }
    Err(anyhow!("fiber_rpc_url must start with http:// or https://"))
}

fn ensure_fiber_ws_url(url_opt: Option<String>) -> Result<String> {
    let mut url = ensure_fiber_http_url(url_opt)?;
    // replace http with ws
    url.replace_range(..4, "ws");
    Ok(url)
}

/// Subscribe to store changes from a Fiber node via WebSocket RPC.
/// Reconnects automatically on failure until the cancellation token is triggered.
async fn subscribe_store_changes_ws(
    ws_url: String,
    actor: ActorRef<CchMessage>,
    token: CancellationToken,
) {
    use jsonrpsee::ws_client::WsClientBuilder;

    loop {
        if token.is_cancelled() {
            return;
        }

        tracing::info!(
            "Connecting to Fiber node WebSocket for store changes at {}",
            ws_url
        );

        let client = match WsClientBuilder::default().build(&ws_url).await {
            Ok(client) => client,
            Err(err) => {
                tracing::error!(
                    "Failed to connect to Fiber WebSocket at {}: {}. Retrying in 5s.",
                    ws_url,
                    err
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    _ = token.cancelled() => return,
                }
            }
        };

        use jsonrpsee::core::client::SubscriptionClientT as _;
        let mut subscription = match client
            .subscribe::<StoreChange, _>(
                "subscribe_store_changes",
                jsonrpsee::rpc_params![],
                "unsubscribe_store_changes",
            )
            .await
        {
            Ok(sub) => sub,
            Err(err) => {
                tracing::error!(
                    "Failed to subscribe to store changes: {}. Retrying in 5s.",
                    err
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    _ = token.cancelled() => return,
                }
            }
        };

        tracing::info!("Successfully subscribed to Fiber node store changes");
        if let Err(err) = actor.send_message(CchMessage::ReconcileFiberPayments) {
            tracing::error!("Failed to schedule Fiber payment reconciliation: {}", err);
            return;
        }

        loop {
            tokio::select! {
                item = subscription.next() => {
                    match item {
                        Some(Ok(change)) => {
                            let summary = redacted_store_change_summary(&change);
                            tracing::debug!(
                                "received store change via websocket kind={} payment_hash={:x} has_payment_preimage={}",
                                summary.kind,
                                summary.payment_hash,
                                summary.has_payment_preimage
                            );
                            if let Err(err) = actor.send_message(CchMessage::StoreChangeEvent(change)) {
                                tracing::error!("Failed to forward store change to CCH actor: {}", err);
                                return;
                            }
                        }
                        Some(Err(err)) => {
                            tracing::error!("WebSocket subscription error: {}. Reconnecting...", err);
                            break;
                        }
                        None => {
                            tracing::warn!("WebSocket subscription ended. Reconnecting...");
                            break;
                        }
                    }
                }
                _ = token.cancelled() => {
                    tracing::info!("Cancellation received, stopping WebSocket subscription");
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct RedactedStoreChangeSummary {
    pub kind: &'static str,
    pub payment_hash: Hash256,
    pub has_payment_preimage: bool,
}

pub(crate) fn redacted_store_change_summary(change: &StoreChange) -> RedactedStoreChangeSummary {
    match change {
        StoreChange::PutCkbInvoiceStatus { payment_hash, .. } => RedactedStoreChangeSummary {
            kind: "PutCkbInvoiceStatus",
            payment_hash: *payment_hash,
            has_payment_preimage: false,
        },
        StoreChange::PutPaymentSession {
            payment_hash,
            payment_preimage,
            ..
        } => RedactedStoreChangeSummary {
            kind: "PutPaymentSession",
            payment_hash: *payment_hash,
            has_payment_preimage: payment_preimage.is_some(),
        },
        StoreChange::PutAttempt { payment_hash, .. } => RedactedStoreChangeSummary {
            kind: "PutAttempt",
            payment_hash: *payment_hash,
            has_payment_preimage: false,
        },
        StoreChange::PutPreimage { payment_hash, .. } => RedactedStoreChangeSummary {
            kind: "PutPreimage",
            payment_hash: *payment_hash,
            has_payment_preimage: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{is_lnd_invoice_not_found, DeferredInvoiceStatus, LND_NO_INVOICES_CREATED_ERROR};
    use crate::invoice::CkbInvoiceStatus;

    #[test]
    fn test_deferred_invoice_status_merge_does_not_regress() {
        let mut deferred = DeferredInvoiceStatus {
            status: CkbInvoiceStatus::Received,
            failure_reason: None,
        };

        deferred.merge(DeferredInvoiceStatus {
            status: CkbInvoiceStatus::Open,
            failure_reason: None,
        });

        assert_eq!(deferred.status, CkbInvoiceStatus::Received);
    }

    #[test]
    fn test_lnd_invoice_not_found_status_classification() {
        assert!(is_lnd_invoice_not_found(&tonic::Status::not_found(
            "unable to locate invoice",
        )));
        assert!(is_lnd_invoice_not_found(&tonic::Status::unknown(
            LND_NO_INVOICES_CREATED_ERROR,
        )));
        assert!(!is_lnd_invoice_not_found(&tonic::Status::unknown(
            "database unavailable",
        )));
    }
}
