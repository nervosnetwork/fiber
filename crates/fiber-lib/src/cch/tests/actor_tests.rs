//! Happy path integration tests for CCH orders
//!
//! These tests simulate the complete order lifecycle through the CchActor,
//! validating the flow that occurs when SendBTC and ReceiveBTC messages are processed
//! by the CchActor.
//!
//! SendBTC Flow (User pays Lightning invoice via Fiber):
//!   Pending → IncomingAccepted → OutgoingInFlight → OutgoingSuccess → Success
//!
//! ReceiveBTC Flow (User receives BTC via Lightning, pays Fiber invoice):
//!   Pending → IncomingAccepted → OutgoingInFlight → OutgoingSuccess → Success

use crate::cch::{
    actions::CchOrderAction,
    actor::{CchActor, CchArgs, CchMessage, BTC_BLOCK_TIME_MILLIS},
    config::{
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS, DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS,
    },
    order::CchOrderStore,
    trackers::CchTrackingEvent,
    CchConfig, CchError, CchStoreError,
};
use crate::fiber::{
    graph::NetworkGraphStateStore,
    network::SendPaymentResponse,
    payment::{PaymentSessionExt, SendPaymentCommand, SendPaymentDataBuilder},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::invoice::{
    Attribute, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceData, SettleInvoiceError,
};
use crate::store::{store_impl::StoreChange, Store};
use crate::tests::test_utils::{generate_store, TempDir};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use fiber_types::{
    AttemptStatus, CchInvoice, CchOrder, CchOrderStatus, Hash256, HashAlgorithm, PaymentHopData,
    PaymentStatus,
};
use ractor::{call, port::OutputPortSubscriberTrait, Actor, ActorRef, OutputPort};
use secp256k1::{Secp256k1, SecretKey};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Bitcoin block interval in seconds (see [`BTC_BLOCK_TIME_MILLIS`] in `cch::actor`).
const BTC_BLOCK_TIME_SECS: u64 = BTC_BLOCK_TIME_MILLIS / 1_000;

/// Mock order store using an in-memory HashMap for testing
#[derive(Clone, Default)]
pub struct MockCchOrderStore {
    orders: Arc<Mutex<HashMap<Hash256, CchOrder>>>,
}

impl MockCchOrderStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CchOrderStore for MockCchOrderStore {
    fn get_cch_order(&self, payment_hash: &Hash256) -> Result<CchOrder, CchStoreError> {
        self.orders
            .lock()
            .unwrap()
            .get(payment_hash)
            .ok_or(CchStoreError::NotFound(*payment_hash))
            .cloned()
    }

    fn insert_cch_order(&self, order: CchOrder) -> Result<(), CchStoreError> {
        let mut orders = self.orders.lock().unwrap();
        let payment_hash = order.payment_hash;
        match orders.insert(payment_hash, order) {
            Some(_) => Err(CchStoreError::Duplicated(payment_hash)),
            None => Ok(()),
        }
    }

    fn update_cch_order(&self, order: CchOrder) {
        let mut orders = self.orders.lock().unwrap();
        orders.insert(order.payment_hash, order);
    }

    fn get_cch_order_keys_iter(&self) -> impl IntoIterator<Item = Hash256> {
        self.orders
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>()
    }

    fn delete_cch_order(&self, payment_hash: &Hash256) {
        let mut orders = self.orders.lock().unwrap();
        orders.remove(payment_hash);
    }
}

/// Helper function to create a test payment hash
fn test_payment_hash(value: u8) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    Hash256::from(bytes)
}

/// Helper function to create a valid preimage/payment hash pair.
/// The preimage will hash to the payment hash using SHA256.
fn create_valid_preimage_pair(seed: u8) -> (Hash256, Hash256) {
    use fiber_types::HashAlgorithm;
    // Generate a preimage from the seed
    let mut preimage_bytes = [0u8; 32];
    preimage_bytes[0] = seed;
    preimage_bytes[1] = seed.wrapping_mul(2);
    preimage_bytes[2] = seed.wrapping_add(1);
    let preimage = Hash256::from(preimage_bytes);

    // Compute the payment hash from the preimage
    let hash_algorithm = HashAlgorithm::Sha256;
    let payment_hash = Hash256::from(hash_algorithm.hash(preimage));

    (preimage, payment_hash)
}

/// Shared state for the mock network actor
#[derive(Clone)]
struct MockNetworkState {
    /// Reference to CchActor to send callbacks
    cch_actor: Arc<Mutex<Option<ActorRef<CchMessage>>>>,
    /// Event port to inject events (simulates FiberStoreWatcher/LndTrackerActor)
    event_port: Arc<OutputPort<CchTrackingEvent>>,
    /// Tracks payment hashes for which SendPayment was called (outgoing Fiber payments)
    sent_fiber_payments: Arc<Mutex<std::collections::HashSet<Hash256>>>,
    /// Tracks payment hashes for which CancelInvoice was called (incoming Fiber invoices)
    cancelled_fiber_invoices: Arc<Mutex<std::collections::HashSet<Hash256>>>,
    /// Tracks payment hashes for which a dry-run Fiber payment was requested.
    preflighted_fiber_payments: Arc<Mutex<std::collections::HashSet<Hash256>>>,
    /// Optional error returned by dry-run Fiber payments.
    fiber_preflight_error: Arc<Mutex<Option<String>>>,
    /// Artificial delay before returning a dry-run Fiber payment response.
    fiber_preflight_delay: Arc<Mutex<Duration>>,
    /// Tracks the `max_fee_amount` of each outgoing Fiber SendPayment, keyed by payment hash.
    sent_fiber_payment_fees: Arc<Mutex<std::collections::HashMap<Hash256, Option<u128>>>>,
    /// Status returned by mocked outgoing Fiber SendPayment.
    send_payment_status: Arc<Mutex<PaymentStatus>>,
    /// Whether mocked Fiber SettleInvoice should return an already-paid error.
    settle_invoice_already_paid: Arc<Mutex<bool>>,
}

/// Mock network actor that handles commands from action executors
struct MockNetworkActor;

#[async_trait::async_trait]
impl Actor for MockNetworkActor {
    type Msg = NetworkActorMessage;
    type State = MockNetworkState;
    type Arguments = MockNetworkState;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ractor::ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ractor::ActorProcessingErr> {
        match message {
            NetworkActorMessage::Command(cmd) => match cmd {
                NetworkActorCommand::AddInvoice(_invoice, _opt_hash, reply) => {
                    // Accept all invoices
                    let _ = reply.send(Ok(()));
                }
                NetworkActorCommand::SendPayment(cmd, reply) => {
                    // Extract payment hash from invoice
                    let payment_hash = extract_payment_hash_from_command(&cmd);

                    if cmd.dry_run {
                        state
                            .preflighted_fiber_payments
                            .lock()
                            .unwrap()
                            .insert(payment_hash);
                        let delay = *state.fiber_preflight_delay.lock().unwrap();
                        tokio::time::sleep(delay).await;
                        if let Some(error) = state.fiber_preflight_error.lock().unwrap().clone() {
                            let _ = reply.send(Err(error));
                            return Ok(());
                        }
                    }

                    if !cmd.dry_run {
                        // Track that this payment was sent
                        state
                            .sent_fiber_payments
                            .lock()
                            .unwrap()
                            .insert(payment_hash);
                        state
                            .sent_fiber_payment_fees
                            .lock()
                            .unwrap()
                            .insert(payment_hash, cmd.max_fee_amount);
                    }

                    // Return success response - the executor will create CchTrackingEvent
                    let status = if cmd.dry_run {
                        PaymentStatus::Created
                    } else {
                        *state.send_payment_status.lock().unwrap()
                    };
                    let response = SendPaymentResponse {
                        payment_hash,
                        status,
                        created_at: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                        last_updated_at: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                        failed_error: None,
                        custom_records: None,
                        fee: 0,
                        #[cfg(any(debug_assertions, test, feature = "bench"))]
                        routers: vec![],
                    };
                    let _ = reply.send(Ok(response));
                }
                NetworkActorCommand::SettleInvoice(payment_hash, _preimage, reply) => {
                    if *state.settle_invoice_already_paid.lock().unwrap() {
                        let _ = reply.send(Err(SettleInvoiceError::InvoiceAlreadyPaid));
                        return Ok(());
                    }
                    // Accept settlement - the InvoiceChanged(Paid) event will be sent
                    // via the event_port by the test (simulating FiberStoreWatcher)
                    let _ = reply.send(Ok(()));

                    // Simulate FiberStoreWatcher detecting the settlement
                    state.event_port.send(CchTrackingEvent::InvoiceChanged {
                        payment_hash,
                        status: CkbInvoiceStatus::Paid,
                        failure_reason: None,
                    });
                }
                NetworkActorCommand::CancelInvoice(payment_hash, reply) => {
                    state
                        .cancelled_fiber_invoices
                        .lock()
                        .unwrap()
                        .insert(payment_hash);
                    let _ = reply.send(Ok(()));
                }
                _ => {
                    // Ignore other commands
                }
            },
            _ => {
                // Ignore non-command messages
            }
        }
        Ok(())
    }
}

/// Extract payment hash from SendPaymentCommand
fn extract_payment_hash_from_command(cmd: &SendPaymentCommand) -> Hash256 {
    if let Some(invoice_str) = &cmd.invoice {
        if let Ok(invoice) = CkbInvoice::from_str_allowing_unsigned(invoice_str) {
            return *invoice.payment_hash();
        }
        if let Ok(ln_invoice) = lightning_invoice::Bolt11Invoice::from_str(invoice_str) {
            return Hash256::from(*ln_invoice.payment_hash());
        }
    }
    cmd.payment_hash.unwrap_or_else(|| test_payment_hash(0))
}

/// Test harness that provides controlled access to CchActor and event injection
struct TestHarness {
    /// The CchActor reference
    actor: ActorRef<CchMessage>,
    /// Event port to inject external events (simulates trackers)
    event_port: Arc<OutputPort<CchTrackingEvent>>,
    /// Shared mock state for tracking sent payments
    mock_state: MockNetworkState,
}

struct StoreBackedTestHarness {
    actor: ActorRef<CchMessage>,
    event_port: Arc<OutputPort<CchTrackingEvent>>,
    _store_change_port: Arc<OutputPort<StoreChange>>,
    store: Store,
    _store_dir: TempDir,
    mock_state: MockNetworkState,
}

impl TestHarness {
    /// Get an order from the actor
    async fn get_order(&self, payment_hash: Hash256) -> Result<CchOrder, CchError> {
        call!(self.actor, CchMessage::GetCchOrder, payment_hash).expect("actor call failed")
    }

    /// Wait for an order to reach a specific status
    async fn wait_for_order_status(
        &self,
        payment_hash: Hash256,
        expected_status: CchOrderStatus,
        timeout_ms: u64,
    ) -> CchOrder {
        let start = std::time::Instant::now();
        let poll_interval = tokio::time::Duration::from_millis(10);
        let timeout = tokio::time::Duration::from_millis(timeout_ms);

        loop {
            let order = self.get_order(payment_hash).await.unwrap();

            if order.status == expected_status {
                return order;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for order status {:?}. Current status: {:?}",
                    expected_status, order.status
                );
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Simulate incoming invoice being paid (e.g., user pays Fiber invoice or LN invoice)
    /// This injects the event via OutputPort, simulating what FiberStoreWatcher/LndTrackerActor would do
    fn simulate_incoming_invoice_received(&self, payment_hash: Hash256) {
        self.event_port.send(CchTrackingEvent::InvoiceChanged {
            payment_hash,
            status: CkbInvoiceStatus::Received,
            failure_reason: None,
        });
    }

    /// Check if an outgoing Fiber payment was actually sent via MockNetworkActor
    fn was_fiber_payment_sent(&self, payment_hash: Hash256) -> bool {
        self.mock_state
            .sent_fiber_payments
            .lock()
            .unwrap()
            .contains(&payment_hash)
    }

    fn was_fiber_invoice_cancelled(&self, payment_hash: Hash256) -> bool {
        self.mock_state
            .cancelled_fiber_invoices
            .lock()
            .unwrap()
            .contains(&payment_hash)
    }

    async fn wait_for_fiber_invoice_cancelled(&self, payment_hash: Hash256, timeout_ms: u64) {
        let start = std::time::Instant::now();
        let poll_interval = tokio::time::Duration::from_millis(10);
        let timeout = tokio::time::Duration::from_millis(timeout_ms);

        loop {
            if self.was_fiber_invoice_cancelled(payment_hash) {
                return;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for Fiber invoice {:x} to be cancelled",
                    payment_hash
                );
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Return the `max_fee_amount` of the outgoing Fiber SendPayment for `payment_hash`, if sent.
    fn fiber_payment_max_fee(&self, payment_hash: Hash256) -> Option<Option<u128>> {
        self.mock_state
            .sent_fiber_payment_fees
            .lock()
            .unwrap()
            .get(&payment_hash)
            .copied()
    }

    fn set_send_payment_status(&self, status: PaymentStatus) {
        *self.mock_state.send_payment_status.lock().unwrap() = status;
    }

    fn set_settle_invoice_already_paid(&self, already_paid: bool) {
        *self.mock_state.settle_invoice_already_paid.lock().unwrap() = already_paid;
    }

    fn fail_fiber_payment_preflight(&self, error: impl Into<String>) {
        *self.mock_state.fiber_preflight_error.lock().unwrap() = Some(error.into());
    }

    fn delay_fiber_payment_preflight(&self, delay: Duration) {
        *self.mock_state.fiber_preflight_delay.lock().unwrap() = delay;
    }

    fn was_fiber_payment_preflighted(&self, payment_hash: Hash256) -> bool {
        self.mock_state
            .preflighted_fiber_payments
            .lock()
            .unwrap()
            .contains(&payment_hash)
    }

    fn simulate_fiber_attempt_status(&self, payment_hash: Hash256, attempt_status: AttemptStatus) {
        self.actor
            .send_message(CchMessage::StoreChangeEvent(StoreChange::PutAttempt {
                payment_hash,
                attempt_status,
            }))
            .expect("actor should accept store change");
    }

    /// Simulate outgoing Fiber payment succeeding with preimage
    /// Only works if the payment was actually sent via MockNetworkActor
    /// This injects the event via OutputPort, simulating what FiberStoreWatcher would do
    fn simulate_fiber_payment_success(&self, payment_hash: Hash256, preimage: Hash256) {
        assert!(
            self.was_fiber_payment_sent(payment_hash),
            "Cannot simulate Fiber payment success: payment was not sent. \
             The order must reach OutgoingInFlight before simulating success."
        );
        self.event_port.send(CchTrackingEvent::PaymentChanged {
            payment_hash,
            status: PaymentStatus::Success,
            payment_preimage: Some(preimage),
            failure_reason: None,
        });
    }

    /// Simulate outgoing Lightning payment events (Inflight then Success)
    /// For SendBTC flow where we can't mock LND gRPC
    /// This should only be called after confirming the order is in IncomingAccepted state
    fn simulate_lightning_payment_success(&self, payment_hash: Hash256, preimage: Hash256) {
        // First send Inflight event (simulating what SendLightningOutgoingPaymentExecutor would do)
        self.event_port.send(CchTrackingEvent::PaymentChanged {
            payment_hash,
            status: PaymentStatus::Inflight,
            payment_preimage: None,
            failure_reason: None,
        });
        // Then send Success event (simulating LND tracking detecting payment completion)
        self.event_port.send(CchTrackingEvent::PaymentChanged {
            payment_hash,
            status: PaymentStatus::Success,
            payment_preimage: Some(preimage),
            failure_reason: None,
        });
    }

    /// Simulate Lightning invoice being settled (Paid)
    /// For ReceiveBTC flow after the preimage is obtained
    fn simulate_lightning_invoice_settled(&self, payment_hash: Hash256) {
        self.event_port.send(CchTrackingEvent::InvoiceChanged {
            payment_hash,
            status: CkbInvoiceStatus::Paid,
            failure_reason: None,
        });
    }

    /// Create a SendBTC order via CchMessage
    /// Returns both the order and the preimage that hashes to its payment hash
    async fn create_send_btc_order_with_preimage(&self) -> Result<(CchOrder, Hash256), CchError> {
        // Generate a valid preimage/payment hash pair first
        let (preimage, payment_hash) = create_valid_preimage_pair(200);
        let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);
        let btc_pay_req = lightning_invoice.to_string();

        let order = call!(
            self.actor,
            CchMessage::SendBTC,
            crate::cch::actor::SendBTC {
                btc_pay_req,
                currency: Currency::Fibb,
            }
        )
        .expect("actor call failed")?;

        Ok((order, preimage))
    }

    /// Insert an order directly into the database (for testing without LND)
    async fn insert_order_directly(&self, order: CchOrder) -> Result<(), CchError> {
        call!(self.actor, CchMessage::InsertOrder, order).expect("actor call failed")
    }
}

impl StoreBackedTestHarness {
    async fn get_order(&self, payment_hash: Hash256) -> Result<CchOrder, CchError> {
        call!(self.actor, CchMessage::GetCchOrder, payment_hash).expect("actor call failed")
    }

    async fn wait_for_order_status(
        &self,
        payment_hash: Hash256,
        expected_status: CchOrderStatus,
        timeout_ms: u64,
    ) -> CchOrder {
        let start = std::time::Instant::now();
        let poll_interval = tokio::time::Duration::from_millis(10);
        let timeout = tokio::time::Duration::from_millis(timeout_ms);

        loop {
            let order = self.get_order(payment_hash).await.unwrap();

            if order.status == expected_status {
                return order;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for order status {:?}. Current status: {:?}",
                    expected_status, order.status
                );
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    fn set_send_payment_status(&self, status: PaymentStatus) {
        *self.mock_state.send_payment_status.lock().unwrap() = status;
    }

    fn simulate_incoming_invoice_received(&self, payment_hash: Hash256) {
        self.event_port.send(CchTrackingEvent::InvoiceChanged {
            payment_hash,
            status: CkbInvoiceStatus::Received,
            failure_reason: None,
        });
    }

    fn was_fiber_payment_sent(&self, payment_hash: Hash256) -> bool {
        self.mock_state
            .sent_fiber_payments
            .lock()
            .unwrap()
            .contains(&payment_hash)
    }

    async fn insert_order_directly(&self, order: CchOrder) -> Result<(), CchError> {
        call!(self.actor, CchMessage::InsertOrder, order).expect("actor call failed")
    }
}

/// Set up a test harness with mocked dependencies
async fn setup_test_harness() -> TestHarness {
    setup_test_harness_with_store(MockCchOrderStore::new()).await
}

async fn setup_test_harness_with_config(config: CchConfig) -> TestHarness {
    setup_test_harness_with_config_and_store(config, MockCchOrderStore::new()).await
}

async fn setup_test_harness_with_store(store: MockCchOrderStore) -> TestHarness {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ..Default::default()
    };
    setup_test_harness_with_config_and_store(config, store).await
}

async fn setup_test_harness_with_config_and_store(
    config: CchConfig,
    store: MockCchOrderStore,
) -> TestHarness {
    let event_port = Arc::new(OutputPort::<CchTrackingEvent>::default());

    let mock_state = MockNetworkState {
        cch_actor: Arc::new(Mutex::new(None)),
        event_port: event_port.clone(),
        sent_fiber_payments: Arc::new(Mutex::new(std::collections::HashSet::new())),
        cancelled_fiber_invoices: Arc::new(Mutex::new(std::collections::HashSet::new())),
        preflighted_fiber_payments: Arc::new(Mutex::new(std::collections::HashSet::new())),
        fiber_preflight_error: Arc::new(Mutex::new(None)),
        fiber_preflight_delay: Arc::new(Mutex::new(Duration::from_secs(0))),
        sent_fiber_payment_fees: Arc::new(Mutex::new(std::collections::HashMap::new())),
        send_payment_status: Arc::new(Mutex::new(PaymentStatus::Inflight)),
        settle_invoice_already_paid: Arc::new(Mutex::new(false)),
    };

    let (network_actor, _) = Actor::spawn(None, MockNetworkActor, mock_state.clone())
        .await
        .expect("spawn mock network actor");

    let args = CchArgs {
        config,
        tracker: TaskTracker::new(),
        token: CancellationToken::new(),
        network_actor: Some(network_actor),
        node_keypair: Some(crate::fiber::KeyPair::try_from([42u8; 32].as_slice()).unwrap()),
        store,
        currency: Currency::Fibb,
    };

    let (actor_ref, _handle) = Actor::spawn(None, CchActor::default(), args)
        .await
        .expect("spawn cch actor");

    actor_ref.subscribe_to_port(&event_port);
    *mock_state.cch_actor.lock().unwrap() = Some(actor_ref.clone());

    TestHarness {
        actor: actor_ref,
        event_port,
        mock_state,
    }
}

async fn setup_store_backed_test_harness() -> StoreBackedTestHarness {
    let event_port = Arc::new(OutputPort::<CchTrackingEvent>::default());
    let store_change_port = Arc::new(OutputPort::<StoreChange>::default());
    let (mut store, store_dir) = generate_store();
    let store_change_port_clone = store_change_port.clone();
    store.set_watcher(Arc::new(move |change| {
        store_change_port_clone.send(change);
    }));

    let mock_state = MockNetworkState {
        cch_actor: Arc::new(Mutex::new(None)),
        event_port: event_port.clone(),
        sent_fiber_payments: Arc::new(Mutex::new(std::collections::HashSet::new())),
        cancelled_fiber_invoices: Arc::new(Mutex::new(std::collections::HashSet::new())),
        preflighted_fiber_payments: Arc::new(Mutex::new(std::collections::HashSet::new())),
        fiber_preflight_error: Arc::new(Mutex::new(None)),
        fiber_preflight_delay: Arc::new(Mutex::new(Duration::from_secs(0))),
        sent_fiber_payment_fees: Arc::new(Mutex::new(std::collections::HashMap::new())),
        send_payment_status: Arc::new(Mutex::new(PaymentStatus::Inflight)),
        settle_invoice_already_paid: Arc::new(Mutex::new(false)),
    };

    let (network_actor, _) = Actor::spawn(None, MockNetworkActor, mock_state.clone())
        .await
        .expect("spawn mock network actor");

    let args = CchArgs {
        config: CchConfig {
            lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
            wrapped_btc_type_script_args: "0x".to_string(),
            min_outgoing_invoice_expiry_delta_seconds: 60,
            ..Default::default()
        },
        tracker: TaskTracker::new(),
        token: CancellationToken::new(),
        network_actor: Some(network_actor),
        node_keypair: Some(crate::fiber::KeyPair::try_from([42u8; 32].as_slice()).unwrap()),
        store: store.clone(),
        currency: Currency::Fibb,
    };

    let (actor_ref, _handle) = Actor::spawn(None, CchActor::default(), args)
        .await
        .expect("spawn cch actor");

    actor_ref.subscribe_to_port(&event_port);
    actor_ref.subscribe_to_port(&store_change_port);
    *mock_state.cch_actor.lock().unwrap() = Some(actor_ref.clone());

    StoreBackedTestHarness {
        actor: actor_ref,
        event_port,
        _store_change_port: store_change_port,
        store,
        _store_dir: store_dir,
        mock_state,
    }
}

/// Create a test Lightning invoice with a specific payment hash
fn create_test_lightning_invoice_with_payment_hash(
    payment_hash: Hash256,
) -> lightning_invoice::Bolt11Invoice {
    let duration_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time");
    create_test_lightning_invoice_with_payment_hash_and_timestamp(
        payment_hash,
        duration_since_epoch,
    )
}

fn create_test_lightning_invoice_with_payment_hash_and_timestamp(
    payment_hash: Hash256,
    duration_since_epoch: std::time::Duration,
) -> lightning_invoice::Bolt11Invoice {
    use bitcoin::hashes::Hash as _;
    use lightning_invoice::{Currency as LnCurrency, InvoiceBuilder as LnInvoiceBuilder};

    // Use bitcoin's secp256k1 types to match lightning_invoice's expectations
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let private_key = bitcoin::secp256k1::SecretKey::from_slice(&[43u8; 32]).unwrap();

    // Convert Hash256 to bitcoin's sha256::Hash (now unified with lightning-invoice)
    let payment_hash_btc = bitcoin::hashes::sha256::Hash::from_slice(payment_hash.as_ref())
        .expect("valid 32-byte hash");

    // Create a payment secret (required for build_signed)
    let payment_secret = lightning_invoice::PaymentSecret([0u8; 32]);

    // Build the invoice with current timestamp (will be valid for 1 hour)
    // Use 36 blocks (~6 hours) for final CLTV, which is less than half of the default
    // CKB final TLC expiry (20 hours), satisfying the cross-chain safety requirement.
    LnInvoiceBuilder::new(LnCurrency::Bitcoin)
        .description("test invoice".to_string())
        .payment_hash(payment_hash_btc)
        .payment_secret(payment_secret)
        .duration_since_epoch(duration_since_epoch)
        .min_final_cltv_expiry_delta(36)
        .amount_milli_satoshis(100_000_000) // 100k sats
        .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &private_key))
        .expect("build lightning invoice")
}

/// Create a test Fiber invoice for testing
fn create_test_fiber_invoice(payment_hash: Hash256) -> CkbInvoice {
    create_test_fiber_invoice_with_amount(payment_hash, 100000)
}

/// Create a test Fiber invoice with a specific amount
fn create_test_fiber_invoice_with_amount(payment_hash: Hash256, amount: u128) -> CkbInvoice {
    // Create a deterministic keypair for tests
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);

    let default_expiry_delta_ms = DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS * 1000;
    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(amount),
        signature: None,
        data: InvoiceData {
            payment_hash,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcMinimumExpiryDelta(default_expiry_delta_ms),
                Attribute::Description("test".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::PayeePublicKey(public_key),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    invoice
}

// =============================================================================
// SendBTC Happy Path Test
// =============================================================================

/// Tests the complete happy path for a SendBTC order.
///
/// Flow: User wants to pay a Lightning invoice using wrapped BTC on Fiber.
/// 1. Hub creates a Fiber invoice for the user to pay
/// 2. User pays the Fiber invoice → IncomingAccepted
/// 3. Hub sends Lightning payment → OutgoingInFlight (via SendLightningOutgoingPaymentExecutor)
/// 4. Lightning payment succeeds with preimage → OutgoingSuccess
/// 5. Hub settles the Fiber invoice with preimage → Success (via SettleFiberIncomingInvoiceExecutor)
#[tokio::test]
async fn test_send_btc_happy_path() {
    // Set up test harness
    let harness = setup_test_harness().await;

    // Step 1: Create order via SendBTC message with a known preimage
    let (order, preimage) = harness.create_send_btc_order_with_preimage().await.unwrap();
    assert_eq!(order.status, CchOrderStatus::Pending);
    let payment_hash = order.payment_hash;

    // Step 2: Simulate user paying the Fiber invoice
    // This event comes from FiberStoreWatcher in production
    harness.simulate_incoming_invoice_received(payment_hash);

    // Wait for order to reach IncomingAccepted status
    // CchActor dispatches SendOutgoingPayment action, executor will run
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::IncomingAccepted, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::IncomingAccepted);

    // Step 3-4: Simulate Lightning payment succeeding with preimage
    // In production, SendLightningOutgoingPaymentExecutor calls LND and sends the event
    // For SendBTC, the outgoing payment is Lightning, so we simulate LND response
    // Note: We can only simulate after IncomingAccepted confirms the order is ready for outgoing payment
    harness.simulate_lightning_payment_success(payment_hash, preimage);

    // Step 5: The state machine transitions through:
    //   OutgoingSuccess (after payment success) → SettleInvoice dispatched
    //   MockNetworkActor handles SettleInvoice and sends InvoiceChanged(Paid)
    //   → Success (final state)
    // Note: These transitions happen quickly, so we wait for the final Success status
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Success, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Success);
    assert!(order.is_final());
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(order.failure_reason.is_none());
}

// =============================================================================
// ReceiveBTC Happy Path Test
// =============================================================================

/// Tests the complete happy path for a ReceiveBTC order.
/// This test creates the order directly in the database, bypassing LND hold invoice creation.
///
/// Flow: User wants to receive BTC on Lightning by providing a Fiber invoice.
/// 1. Order created directly in database (bypassing LND hold invoice creation)
/// 2. Payer pays the Lightning invoice → IncomingAccepted
/// 3. Hub sends Fiber payment → OutgoingInFlight (via SendFiberOutgoingPaymentExecutor)
/// 4. Fiber payment succeeds with preimage → OutgoingSuccess
/// 5. Hub settles the Lightning invoice with preimage → Success
#[tokio::test]
async fn test_receive_btc_happy_path() {
    // Generate a valid preimage/payment hash pair
    let (preimage, payment_hash) = create_valid_preimage_pair(99);

    // Set up test harness
    let harness = setup_test_harness().await;

    // Step 1: Create order directly in the database (bypassing LND hold invoice creation)
    // In production, ReceiveBTC creates a hold invoice via LND, but we skip that for testing.
    // Use a small final TLC expiry delta (10,000 ms = 10 seconds) so it fits
    // within the default incoming budget (half of DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS
    // in seconds at BTC_BLOCK_TIME_SECS per block).
    let fiber_invoice = create_test_fiber_invoice_with_expiry(payment_hash, 10_000);
    // The incoming Lightning invoice must carry min_final_cltv_expiry_delta matching
    // DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS so the stored invoice reflects a
    // realistic inbound HTLC budget.
    let lightning_invoice = create_test_lightning_invoice_with_cltv(
        payment_hash,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS,
    );
    let order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };
    harness.insert_order_directly(order).await.unwrap();

    // Step 2: Simulate payer paying the Lightning invoice
    // This event comes from LndTrackerActor in production
    harness.simulate_incoming_invoice_received(payment_hash);

    // Step 2-3: Wait for OutgoingInFlight to confirm the payment was actually sent
    // Note: IncomingAccepted → OutgoingInFlight transition happens very fast because
    // MockNetworkActor immediately handles SendPayment and returns Inflight status.
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingInFlight, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);

    // Step 4: Simulate Fiber payment succeeding with preimage
    // In production, FiberStoreWatcher detects this and sends event
    // IMPORTANT: We only simulate success after confirming the payment was actually sent
    // (OutgoingInFlight proves MockNetworkActor received SendPayment)
    harness.simulate_fiber_payment_success(payment_hash, preimage);

    // Wait for order to reach OutgoingSuccess status
    // CchActor dispatches SettleIncomingInvoice action
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingSuccess, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingSuccess);
    assert_eq!(order.payment_preimage, Some(preimage));

    // Step 5: Simulate LND invoice settlement
    // In production, SettleLightningIncomingInvoiceExecutor calls LND, then LndTrackerActor sends this event
    harness.simulate_lightning_invoice_settled(payment_hash);

    // Wait for order to reach Success status
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Success, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Success);

    // Verify final state
    assert!(order.is_final());
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(order.failure_reason.is_none());
}

#[tokio::test]
async fn test_receive_btc_outgoing_fiber_attempt_inflight_marks_outgoing_inflight() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(98);
    let harness = setup_test_harness().await;
    harness.set_send_payment_status(PaymentStatus::Created);

    let fiber_invoice = create_test_fiber_invoice_with_expiry(payment_hash, 10_000);
    let lightning_invoice = create_test_lightning_invoice_with_cltv(
        payment_hash,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS,
    );
    let order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };
    harness.insert_order_directly(order).await.unwrap();

    harness.simulate_incoming_invoice_received(payment_hash);
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert!(harness.was_fiber_payment_sent(payment_hash));
    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::IncomingAccepted);

    harness.simulate_fiber_attempt_status(payment_hash, AttemptStatus::Inflight);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingInFlight, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
}

#[tokio::test]
async fn test_receive_btc_store_attempt_inflight_updates_payment_and_cch_status() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(99);
    let harness = setup_store_backed_test_harness().await;
    harness.set_send_payment_status(PaymentStatus::Created);

    let fiber_invoice = create_test_fiber_invoice_with_expiry(payment_hash, 10_000);
    let lightning_invoice = create_test_lightning_invoice_with_cltv(
        payment_hash,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS,
    );
    let order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };
    harness.insert_order_directly(order).await.unwrap();

    harness.simulate_incoming_invoice_received(payment_hash);
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert!(harness.was_fiber_payment_sent(payment_hash));
    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::IncomingAccepted);

    let payment_data =
        SendPaymentDataBuilder::new(crate::gen_rand_fiber_public_key(), 100_000, payment_hash)
            .final_tlc_expiry_delta(10_000)
            .tlc_expiry_limit(10_000)
            .timeout(Some(10))
            .max_fee_amount(Some(1_000))
            .build()
            .expect("valid payment_data");
    let payment_session =
        fiber_types::PaymentSession::new_session(&harness.store, payment_data, 10);
    harness
        .store
        .insert_payment_session(payment_session.clone());

    let route_hops = vec![PaymentHopData {
        amount: 100_000,
        expiry: 10_000,
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::default(),
        funding_tx_hash: crate::gen_rand_sha256_hash(),
        next_hop: None,
        custom_records: None,
    }];
    let mut attempt = payment_session.new_attempt(
        1,
        crate::gen_rand_fiber_public_key(),
        crate::gen_rand_fiber_public_key(),
        route_hops,
    );
    harness.store.insert_attempt(attempt.clone());
    attempt.set_inflight_status();
    harness.store.insert_attempt(attempt);

    let payment = harness
        .store
        .get_payment_session(payment_hash)
        .expect("payment session should exist");
    assert_eq!(payment.status, PaymentStatus::Inflight);

    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingInFlight, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
}

async fn insert_receive_btc_order_with_expiry(
    harness: &TestHarness,
    payment_hash: Hash256,
    expiry_delta_seconds: u64,
) {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    insert_receive_btc_order_with_created_at_and_expiry(
        harness,
        payment_hash,
        created_at,
        expiry_delta_seconds,
    )
    .await;
}

async fn insert_receive_btc_order_with_created_at_and_expiry(
    harness: &TestHarness,
    payment_hash: Hash256,
    created_at: u64,
    expiry_delta_seconds: u64,
) {
    let fiber_invoice = create_test_fiber_invoice_with_expiry(payment_hash, 60_000);
    let lightning_invoice = create_test_lightning_invoice_with_cltv(
        payment_hash,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS,
    );
    let order = CchOrder {
        created_at,
        expiry_delta_seconds,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };
    harness.insert_order_directly(order).await.unwrap();
}

#[tokio::test]
async fn test_receive_btc_expired_pending_order_rejects_late_incoming_invoice() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(210);
    let order_expiry_delta_seconds = 1u64;
    let harness = setup_test_harness().await;
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let created_at = current_time - order_expiry_delta_seconds - 1;

    insert_receive_btc_order_with_created_at_and_expiry(
        &harness,
        payment_hash,
        created_at,
        order_expiry_delta_seconds,
    )
    .await;

    harness.simulate_incoming_invoice_received(payment_hash);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Failed, 1000)
        .await;

    assert_eq!(order.status, CchOrderStatus::Failed);
    assert!(order
        .failure_reason
        .as_deref()
        .unwrap_or_default()
        .contains("expired"));
    assert!(
        !harness.was_fiber_payment_sent(payment_hash),
        "expired pending order must not dispatch outgoing Fiber payment"
    );
}

#[tokio::test]
async fn test_receive_btc_pending_order_rejects_incoming_invoice_at_expiry_cutoff() {
    let order_expiry_delta_seconds = 1u64;
    let harness = setup_test_harness().await;

    for seed in 213u8..223 {
        let (_preimage, payment_hash) = create_valid_preimage_pair(seed);
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cutoff_time = current_time + 1;
        let created_at = cutoff_time - order_expiry_delta_seconds;

        insert_receive_btc_order_with_created_at_and_expiry(
            &harness,
            payment_hash,
            created_at,
            order_expiry_delta_seconds,
        )
        .await;

        loop {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            if now >= cutoff_time {
                if now != cutoff_time {
                    break;
                }

                harness.simulate_incoming_invoice_received(payment_hash);
                let order = harness
                    .wait_for_order_status(payment_hash, CchOrderStatus::Failed, 1000)
                    .await;

                assert_eq!(order.status, CchOrderStatus::Failed);
                assert!(order
                    .failure_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("expired"));
                assert!(
                    !harness.was_fiber_payment_sent(payment_hash),
                    "pending order at expiry cutoff must not dispatch outgoing Fiber payment"
                );
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }

    panic!("could not deliver incoming invoice event at the exact expiry cutoff");
}

#[tokio::test]
async fn test_receive_btc_late_preimage_after_order_expiry_is_reconciled() {
    let (preimage, payment_hash) = create_valid_preimage_pair(211);
    let order_expiry_delta_seconds = 2u64;
    let harness = setup_test_harness().await;

    insert_receive_btc_order_with_expiry(&harness, payment_hash, order_expiry_delta_seconds).await;

    harness.simulate_incoming_invoice_received(payment_hash);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingInFlight, 2000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
    assert!(harness.was_fiber_payment_sent(payment_hash));

    tokio::time::sleep(tokio::time::Duration::from_secs(
        order_expiry_delta_seconds + 2,
    ))
    .await;

    harness.simulate_fiber_payment_success(payment_hash, preimage);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingSuccess, 2000)
        .await;

    assert_eq!(
        order.status,
        CchOrderStatus::OutgoingSuccess,
        "late outgoing Fiber preimage should keep receive_btc reconciliation active"
    );
    assert_eq!(
        order.payment_preimage,
        Some(preimage),
        "late outgoing Fiber preimage should be persisted for incoming settlement"
    );

    harness.simulate_lightning_invoice_settled(payment_hash);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Success, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Success);
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(order.failure_reason.is_none());
}

#[tokio::test]
async fn test_receive_btc_order_expiry_does_not_fail_after_incoming_accepted() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(212);
    let order_expiry_delta_seconds = 2u64;
    let harness = setup_test_harness().await;

    insert_receive_btc_order_with_expiry(&harness, payment_hash, order_expiry_delta_seconds).await;

    harness.simulate_incoming_invoice_received(payment_hash);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingInFlight, 2000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
    assert!(harness.was_fiber_payment_sent(payment_hash));

    tokio::time::sleep(tokio::time::Duration::from_secs(
        order_expiry_delta_seconds + 2,
    ))
    .await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(
        order.status,
        CchOrderStatus::OutgoingInFlight,
        "receive_btc order should remain active after incoming payment is accepted"
    );
    assert!(
        !order.is_final(),
        "accepted receive_btc order must not become final from the original quote TTL"
    );
    assert!(order.failure_reason.is_none());
}

/// Drive a ReceiveBTC-style order (incoming Lightning, outgoing Fiber) up to the point where
/// the outgoing Fiber `SendPayment` has been dispatched, and return the `max_fee_amount` that
/// was attached to the `SendPaymentCommand`.
///
/// `amount_sats`/`fee_sats` configure the order economics so callers can exercise the fee
/// budget binding (the outgoing route fee must be capped at the collected CCH fee).
async fn dispatch_fiber_outgoing_and_capture_fee(
    harness: &TestHarness,
    seed: u8,
    amount_sats: u128,
    fee_sats: u128,
) -> Option<u128> {
    let (_preimage, payment_hash) = create_valid_preimage_pair(seed);

    let fiber_invoice = create_test_fiber_invoice_with_expiry(payment_hash, 10_000);
    let lightning_invoice = create_test_lightning_invoice_with_cltv(
        payment_hash,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS,
    );
    let order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats,
        fee_sats,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };
    harness.insert_order_directly(order).await.unwrap();

    harness.simulate_incoming_invoice_received(payment_hash);

    // Reaching OutgoingInFlight proves MockNetworkActor received the SendPayment command.
    harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingInFlight, 1000)
        .await;

    harness
        .fiber_payment_max_fee(payment_hash)
        .expect("outgoing Fiber payment should have been dispatched")
}

/// The outgoing Fiber payment must carry `max_fee_amount` equal to the order's collected fee.
///
/// This is a regression test for the issue where CCH did not bind its fee budget to the
/// outgoing payment, allowing the default user-payment fee cap (0.5% of amount) to authorize a
/// route fee far larger than the fee the operator charged.
#[tokio::test]
async fn test_receive_btc_outgoing_fiber_fee_capped_at_collected_fee() {
    let harness = setup_test_harness().await;

    // Pick economics where the default Fiber cap (0.5% * amount = 5000) vastly exceeds the
    // tiny collected CCH fee (100). Without binding, the route could spend up to 5000.
    let amount_sats = 1_000_000;
    let fee_sats = 100;
    let max_fee =
        dispatch_fiber_outgoing_and_capture_fee(&harness, 60, amount_sats, fee_sats).await;

    assert_eq!(
        max_fee,
        Some(fee_sats),
        "outgoing Fiber payment must be capped at the collected CCH fee, not the default 0.5% cap"
    );

    // Sanity check: the default user-payment cap would have been much larger than fee_sats.
    let default_cap = amount_sats * 5 / 1000;
    assert!(
        default_cap > fee_sats,
        "test scenario must have default cap exceeding collected fee to be meaningful"
    );
}

/// The `max_outgoing_fee_percentage` config knob must scale the outgoing fee budget.
#[tokio::test]
async fn test_receive_btc_outgoing_fiber_fee_scaled_by_percentage() {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        max_outgoing_fee_percentage: 50,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let fee_sats = 1_000;
    let max_fee = dispatch_fiber_outgoing_and_capture_fee(&harness, 61, 1_000_000, fee_sats).await;

    assert_eq!(
        max_fee,
        Some(fee_sats * 50 / 100),
        "outgoing Fiber fee budget must be scaled by max_outgoing_fee_percentage"
    );
}

#[tokio::test]
async fn test_resume_expired_order_marked_as_failed() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(150);
    let store = MockCchOrderStore::new();

    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expired_order = CchOrder {
        created_at: current_time - 7200,
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };

    store.insert_cch_order(expired_order.clone()).unwrap();

    let harness = setup_test_harness_with_store(store).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert!(order.failure_reason.is_some());
    assert!(order
        .failure_reason
        .unwrap()
        .contains("Order expired on startup"));
    harness
        .wait_for_fiber_invoice_cancelled(payment_hash, 1000)
        .await;
}

#[tokio::test]
async fn test_scheduled_expiry_cancels_fiber_incoming_invoice() {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        order_expiry_delta_seconds: 1,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (order, _preimage) = harness.create_send_btc_order_with_preimage().await.unwrap();
    let payment_hash = order.payment_hash;

    harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Failed, 2500)
        .await;
    harness
        .wait_for_fiber_invoice_cancelled(payment_hash, 1000)
        .await;
}

#[tokio::test]
async fn test_resume_expired_outgoing_success_does_not_cancel_incoming_invoice() {
    let (preimage, payment_hash) = create_valid_preimage_pair(154);
    let store = MockCchOrderStore::new();

    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expired_outgoing_success = CchOrder {
        created_at: current_time - 7200,
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: Some(preimage),
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::OutgoingSuccess,
        failure_reason: None,
    };

    store.insert_cch_order(expired_outgoing_success).unwrap();

    let harness = setup_test_harness_with_store(store).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_ne!(order.status, CchOrderStatus::Failed);
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(
        !harness.was_fiber_invoice_cancelled(payment_hash),
        "an order with a preimage must be settled or retried, never cancelled on startup expiry"
    );
}

#[tokio::test]
async fn test_live_expiry_ignores_outgoing_success_with_preimage() {
    let (preimage, payment_hash) = create_valid_preimage_pair(155);
    let harness = setup_test_harness().await;

    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let outgoing_success_order = CchOrder {
        created_at: current_time - 7200,
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: Some(preimage),
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::OutgoingSuccess,
        failure_reason: None,
    };

    harness
        .insert_order_directly(outgoing_success_order)
        .await
        .unwrap();
    harness
        .actor
        .send_message(CchMessage::ExpireOrder(payment_hash))
        .expect("actor should accept expiry message");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::OutgoingSuccess);
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(
        !harness.was_fiber_invoice_cancelled(payment_hash),
        "live order expiry must not cancel an incoming invoice after outgoing success"
    );
}

#[tokio::test]
async fn test_incoming_cancel_does_not_finalize_outgoing_in_flight_order() {
    let (preimage, payment_hash) = create_valid_preimage_pair(156);
    let harness = setup_test_harness().await;

    let order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::OutgoingInFlight,
        failure_reason: None,
    };

    harness.insert_order_directly(order).await.unwrap();
    harness.event_port.send(CchTrackingEvent::InvoiceChanged {
        payment_hash,
        status: CkbInvoiceStatus::Cancelled,
        failure_reason: Some("stale incoming cancellation".to_string()),
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
    assert!(order.failure_reason.is_none());
    assert!(!harness.was_fiber_invoice_cancelled(payment_hash));

    harness.simulate_lightning_payment_success(payment_hash, preimage);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Success, 1000)
        .await;
    assert_eq!(order.payment_preimage, Some(preimage));
}

#[tokio::test]
async fn test_settle_incoming_invoice_already_paid_marks_order_success() {
    let (preimage, payment_hash) = create_valid_preimage_pair(157);
    let harness = setup_test_harness().await;
    harness.set_settle_invoice_already_paid(true);

    let order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: Some(preimage),
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::OutgoingSuccess,
        failure_reason: None,
    };

    harness.insert_order_directly(order).await.unwrap();
    harness
        .actor
        .send_message(CchMessage::ExecuteAction {
            payment_hash,
            action: CchOrderAction::SettleIncomingInvoice,
            retry_count: 0,
        })
        .expect("actor should accept settle action");

    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Success, 1000)
        .await;
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(order.failure_reason.is_none());
}

#[tokio::test]
async fn test_resume_expired_non_pending_order_is_not_marked_as_failed() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(154);
    let store = MockCchOrderStore::new();

    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expired_order = CchOrder {
        created_at: current_time - 7200,
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::IncomingAccepted,
        failure_reason: None,
    };

    store.insert_cch_order(expired_order).unwrap();

    let harness = setup_test_harness_with_store(store).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::IncomingAccepted);
    assert!(order.failure_reason.is_none());
}

/// Tests that non-expired active orders have tracking resumed on startup.
#[tokio::test]
async fn test_resume_active_order_tracking_resumed() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(151);
    let store = MockCchOrderStore::new();

    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let active_order = CchOrder {
        created_at: current_time - 100,
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Pending,
        failure_reason: None,
    };

    store.insert_cch_order(active_order.clone()).unwrap();

    let harness = setup_test_harness_with_store(store).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_eq!(order.status, CchOrderStatus::Pending);
    assert!(order.failure_reason.is_none());

    harness.simulate_incoming_invoice_received(payment_hash);
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::IncomingAccepted, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::IncomingAccepted);
}

/// Tests that final Success orders are left alone and final Failed orders recover cleanup.
#[tokio::test]
async fn test_resume_final_orders() {
    let (preimage1, payment_hash1) = create_valid_preimage_pair(152);
    let (_preimage2, payment_hash2) = create_valid_preimage_pair(153);
    let store = MockCchOrderStore::new();

    let succeeded_order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash1)),
        payment_hash: payment_hash1,
        payment_preimage: Some(preimage1),
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Success,
        failure_reason: None,
    };

    let failed_order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash2)),
        payment_hash: payment_hash2,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::Failed,
        failure_reason: Some("Test failure".to_string()),
    };

    store.insert_cch_order(succeeded_order.clone()).unwrap();
    store.insert_cch_order(failed_order.clone()).unwrap();

    let harness = setup_test_harness_with_store(store).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order1 = harness.get_order(payment_hash1).await.unwrap();
    assert_eq!(order1.status, CchOrderStatus::Success);
    assert_eq!(order1.payment_preimage, Some(preimage1));

    let order2 = harness.get_order(payment_hash2).await.unwrap();
    assert_eq!(order2.status, CchOrderStatus::Failed);
    assert_eq!(order2.failure_reason, Some("Test failure".to_string()));
    harness
        .wait_for_fiber_invoice_cancelled(payment_hash2, 1000)
        .await;
}

// =============================================================================
// Invoice Validation Tests
// =============================================================================

/// Create a test Lightning invoice with a specific payment hash and currency/network
fn create_test_lightning_invoice_with_currency(
    payment_hash: Hash256,
    ln_currency: lightning_invoice::Currency,
) -> lightning_invoice::Bolt11Invoice {
    use bitcoin::hashes::Hash as _;
    use lightning_invoice::InvoiceBuilder as LnInvoiceBuilder;

    let secp = bitcoin::secp256k1::Secp256k1::new();
    let private_key = bitcoin::secp256k1::SecretKey::from_slice(&[43u8; 32]).unwrap();

    let payment_hash_btc = bitcoin::hashes::sha256::Hash::from_slice(payment_hash.as_ref())
        .expect("valid 32-byte hash");

    let payment_secret = lightning_invoice::PaymentSecret([0u8; 32]);

    let duration_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time");
    LnInvoiceBuilder::new(ln_currency)
        .description("test invoice".to_string())
        .payment_hash(payment_hash_btc)
        .payment_secret(payment_secret)
        .duration_since_epoch(duration_since_epoch)
        .min_final_cltv_expiry_delta(36)
        .amount_milli_satoshis(100_000_000) // 100k sats
        .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &private_key))
        .expect("build lightning invoice")
}

/// Create a test Fiber invoice with a specified currency
fn create_test_fiber_invoice_with_currency(
    payment_hash: Hash256,
    currency: Currency,
) -> CkbInvoice {
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);

    let mut invoice = CkbInvoice {
        currency,
        amount: Some(100000),
        signature: None,
        data: InvoiceData {
            payment_hash,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcMinimumExpiryDelta(12),
                Attribute::Description("test".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::PayeePublicKey(public_key),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    invoice
}

/// Tests that send_btc rejects when the currency parameter doesn't match the configured network.
/// Issue #981: send_btc should fail when currency (e.g. Fibb) doesn't match node's network (e.g. Fibd)
#[tokio::test]
async fn test_send_btc_rejects_currency_mismatch() {
    let harness = setup_test_harness().await;
    // The harness is configured with Currency::Fibb

    let (_, payment_hash) = create_valid_preimage_pair(100);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);
    let btc_pay_req = lightning_invoice.to_string();

    // Try to send with Fibd currency (node is configured for Fibb)
    let result = call!(
        harness.actor,
        CchMessage::SendBTC,
        crate::cch::actor::SendBTC {
            btc_pay_req,
            currency: Currency::Fibd,
        }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::CKBInvoiceNetworkMismatch { expected, actual }) => {
            assert_eq!(expected, Currency::Fibb);
            assert_eq!(actual, Currency::Fibd);
        }
        other => panic!("Expected CKBInvoiceNetworkMismatch, got {:?}", other),
    }
}

/// Tests that send_btc rejects when the BTC invoice network doesn't match the expected network.
/// Issue #978: send_btc should fail when BTC invoice is for regtest but node expects mainnet
#[tokio::test]
async fn test_send_btc_rejects_btc_invoice_network_mismatch() {
    let harness = setup_test_harness().await;
    // The harness is configured with Currency::Fibb, expecting LnCurrency::Bitcoin

    let (_, payment_hash) = create_valid_preimage_pair(101);
    // Create a regtest invoice (but node expects mainnet)
    let lightning_invoice = create_test_lightning_invoice_with_currency(
        payment_hash,
        lightning_invoice::Currency::Regtest,
    );
    let btc_pay_req = lightning_invoice.to_string();

    let result = call!(
        harness.actor,
        CchMessage::SendBTC,
        crate::cch::actor::SendBTC {
            btc_pay_req,
            currency: Currency::Fibb, // Matches configured currency
        }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::BTCInvoiceNetworkMismatch { expected, actual }) => {
            assert_eq!(expected, "Bitcoin");
            assert_eq!(actual, "Regtest");
        }
        other => panic!("Expected BTCInvoiceNetworkMismatch, got {:?}", other),
    }
}

#[tokio::test]
async fn test_send_btc_rejects_future_btc_invoice_timestamp() {
    let harness = setup_test_harness().await;

    let (_, payment_hash) = create_valid_preimage_pair(104);
    let future_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        + std::time::Duration::from_secs(3600);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash_and_timestamp(
        payment_hash,
        future_timestamp,
    );
    let btc_pay_req = lightning_invoice.to_string();

    let result = call!(
        harness.actor,
        CchMessage::SendBTC,
        crate::cch::actor::SendBTC {
            btc_pay_req,
            currency: Currency::Fibb,
        }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::BTCInvoiceCreationTimeInFuture {
            invoice_created_at,
            order_created_at,
        }) => {
            assert!(invoice_created_at > order_created_at);
        }
        other => panic!("Expected BTCInvoiceCreationTimeInFuture, got {:?}", other),
    }
}

/// Tests that receive_btc rejects when the CKB invoice currency doesn't match the configured network.
/// Issue #982: receive_btc should fail when invoice currency (e.g. Fibt) doesn't match node's network (e.g. Fibd)
#[tokio::test]
async fn test_receive_btc_rejects_currency_mismatch() {
    let harness = setup_test_harness().await;
    // The harness is configured with Currency::Fibb

    let (_, payment_hash) = create_valid_preimage_pair(102);
    // Create a Fibt invoice (wrong currency for this node)
    let invoice = create_test_fiber_invoice_with_currency(payment_hash, Currency::Fibt);
    let fiber_pay_req = invoice.to_string();

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::actor::ReceiveBTC { fiber_pay_req }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::CKBInvoiceNetworkMismatch { expected, actual }) => {
            assert_eq!(expected, Currency::Fibb);
            assert_eq!(actual, Currency::Fibt);
        }
        other => panic!("Expected CKBInvoiceNetworkMismatch, got {:?}", other),
    }
}

#[tokio::test]
async fn test_receive_btc_rejects_future_fiber_invoice_timestamp() {
    let harness = setup_test_harness().await;

    let (_, payment_hash) = create_valid_preimage_pair(105);
    let mut invoice = create_test_fiber_invoice_with_expiry(payment_hash, 10_000);
    invoice.data.timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + u128::from(60_000u64);
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::actor::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::CKBInvoiceCreationTimeInFuture {
            invoice_created_at_ms,
            order_created_at_ms,
        }) => {
            assert!(invoice_created_at_ms > order_created_at_ms);
        }
        other => panic!("Expected CKBInvoiceCreationTimeInFuture, got {:?}", other),
    }
}

/// Tests that receive_btc rejects unsigned Fiber invoices before creating orders.
#[tokio::test]
async fn test_receive_btc_rejects_unsigned_fiber_invoice() {
    let harness = setup_test_harness().await;
    let (_, payment_hash) = create_valid_preimage_pair(101);
    let mut invoice = create_test_fiber_invoice(payment_hash);
    invoice.signature = None;

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::actor::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    let err = result.expect_err("unsigned Fiber invoice should be rejected");
    assert!(
        matches!(
            err,
            CchError::CKBInvoiceError(crate::invoice::InvoiceError::MissingSignature)
        ),
        "expected CKBInvoiceError(MissingSignature), got: {:?}",
        err
    );
}

/// Tests that receive_btc rejects a plain CKB invoice without UDT type script.
/// Issue #983: receive_btc should fail when invoice has no wrapped BTC UDT type script
#[tokio::test]
async fn test_receive_btc_rejects_ckb_invoice_without_udt() {
    let harness = setup_test_harness().await;
    // The harness is configured with Currency::Fibb

    let (_, payment_hash) = create_valid_preimage_pair(103);
    // create_test_fiber_invoice_with_currency creates an invoice WITHOUT UDT type script
    let invoice = create_test_fiber_invoice_with_currency(payment_hash, Currency::Fibb);
    let fiber_pay_req = invoice.to_string();

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::actor::ReceiveBTC { fiber_pay_req }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::WrappedBTCTypescriptMismatch) => {} // Expected
        other => panic!("Expected WrappedBTCTypescriptMismatch, got {:?}", other),
    }
}

/// Tests that receive_btc rejects an invoice where amount + fee overflows i64 in msat.
/// The total_msat = (amount_sats + fee_sats) * 1000 must fit in i64.
#[tokio::test]
async fn test_receive_btc_amount_too_large() {
    let harness = setup_test_harness().await;

    let (_preimage, payment_hash) = create_valid_preimage_pair(170);
    // i64::MAX / 1000 + 1 = 9_223_372_036_854_776 sats, which makes total_msat overflow i64
    let large_amount: u128 = (i64::MAX / 1_000) as u128 + 1;
    let invoice = create_test_fiber_invoice_with_amount(payment_hash, large_amount);

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, CchError::ReceiveBTCOrderAmountTooLarge),
        "expected ReceiveBTCOrderAmountTooLarge, got: {:?}",
        err
    );
}

/// Tests that receive_btc rejects an invoice where amount_sats + fee_sats overflows u128.
#[tokio::test]
async fn test_receive_btc_amount_overflow_u128() {
    let harness = setup_test_harness().await;

    let (_preimage, payment_hash) = create_valid_preimage_pair(171);
    // u128::MAX will cause amount_sats * fee_rate to wrap and checked_add/checked_mul to fail
    let invoice = create_test_fiber_invoice_with_amount(payment_hash, u128::MAX);

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, CchError::ReceiveBTCOrderAmountTooLarge),
        "expected ReceiveBTCOrderAmountTooLarge, got: {:?}",
        err
    );
}

/// An unroutable Fiber invoice must be rejected before CCH creates an LND hold invoice.
#[tokio::test]
async fn test_receive_btc_preflights_fiber_sendability_before_lnd() {
    use crate::ckb::contracts::{get_script_by_contract, Contract};
    use crate::invoice::CkbScript;

    let harness = setup_test_harness().await;
    harness.fail_fiber_payment_preflight("Failed to build route, no path found");

    let (_preimage, payment_hash) = create_valid_preimage_pair(172);
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);
    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(100_000),
        signature: None,
        data: InvoiceData {
            payment_hash,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcMinimumExpiryDelta(12),
                Attribute::Description("unroutable invoice".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::PayeePublicKey(public_key),
                Attribute::UdtScript(CkbScript(get_script_by_contract(Contract::SimpleUDT, &[]))),
                Attribute::HashAlgorithm(HashAlgorithm::Sha256),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    let error = result.expect_err("unroutable Fiber invoice must be rejected");
    assert!(
        matches!(error, CchError::FiberNodeError(_)),
        "sendability failure must be returned before contacting LND, got: {:?}",
        error
    );
    assert!(
        harness.was_fiber_payment_preflighted(payment_hash),
        "receive_btc must dry-run the outgoing Fiber payment"
    );
}

/// A Fiber invoice that becomes too short-lived during preflight must be rejected before CCH
/// creates an LND hold invoice with a longer relative expiry.
#[tokio::test]
async fn test_receive_btc_rechecks_fiber_invoice_expiry_after_preflight() {
    use crate::ckb::contracts::{get_script_by_contract, Contract};
    use crate::invoice::CkbScript;

    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 1,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;
    harness.delay_fiber_payment_preflight(Duration::from_millis(2_100));

    let (_preimage, payment_hash) = create_valid_preimage_pair(173);
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);
    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(100_000),
        signature: None,
        data: InvoiceData {
            payment_hash,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcMinimumExpiryDelta(12),
                Attribute::Description("short-lived invoice".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(2)),
                Attribute::PayeePublicKey(public_key),
                Attribute::UdtScript(CkbScript(get_script_by_contract(Contract::SimpleUDT, &[]))),
                Attribute::HashAlgorithm(HashAlgorithm::Sha256),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    assert!(
        harness.was_fiber_payment_preflighted(payment_hash),
        "receive_btc must complete preflight before rechecking expiry"
    );
    assert!(
        matches!(result, Err(CchError::OutgoingInvoiceExpiryTooShort)),
        "invoice that expires during preflight must be rejected, got: {:?}",
        result
    );
}

/// Tests that the send_btc proxy Fiber invoice includes the fee in its amount.
///
/// In the SendBTC flow, the hub creates a Fiber invoice (the proxy invoice) for the
/// user to pay. Its amount must be `ceil(btc_amount_msat / 1000) + fee_sats` so
/// the hub collects enough to cover the outgoing Lightning payment plus its fee.
#[tokio::test]
async fn test_send_btc_proxy_invoice_includes_fee() {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        base_fee_sats: 1_000, // 1000 sat base fee to make the fee clearly visible
        fee_rate_per_million_sats: 10_000, // 1% proportional fee
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    // The lightning invoice has 100_000_000 msat = 100_000 sats
    let (order, _preimage) = harness.create_send_btc_order_with_preimage().await.unwrap();
    let btc_amount_sats: u128 = 100_000; // 100_000_000 msat / 1000

    // fee_sats = amount_msat * fee_rate / 1_000_000_000 + base_fee
    //          = 100_000_000 * 10_000 / 1_000_000_000 + 1_000
    //          = 1_000 + 1_000
    //          = 2_000
    let expected_fee: u128 = 2_000;
    assert_eq!(
        order.fee_sats, expected_fee,
        "fee_sats should be calculated from rate + base"
    );

    // The proxy invoice amount must include the fee
    let expected_total = btc_amount_sats + expected_fee;
    assert_eq!(
        order.amount_sats, expected_total,
        "proxy invoice amount should be btc_amount + fee"
    );

    // Verify the Fiber invoice stored in the order also has the correct amount
    let fiber_invoice = match &order.incoming_invoice {
        CchInvoice::Fiber(inv) => inv.clone(),
        other => panic!("expected Fiber invoice, got: {:?}", other),
    };
    assert_eq!(
        fiber_invoice.amount(),
        Some(expected_total),
        "Fiber proxy invoice amount should include the fee"
    );
}

/// Tests that the receive_btc order correctly calculates fee_sats and total_msat
/// that would be used for the LND hold invoice.
///
/// Note: We cannot directly test the LND hold invoice creation since it requires
/// an LND server. Instead we verify that the fee calculation and amount validation
/// pass correctly (the call fails only at LND), confirming the hold invoice would
/// be created with `value_msat = (amount_sats + fee_sats) * 1000`.
#[tokio::test]
async fn test_receive_btc_fee_calculation() {
    use crate::ckb::contracts::{get_script_by_contract, Contract};
    use crate::invoice::CkbScript;

    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        base_fee_sats: 500,
        fee_rate_per_million_sats: 5_000, // 0.5% proportional fee
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (_preimage, payment_hash) = create_valid_preimage_pair(180);
    let amount_sats: u128 = 200_000;

    // Build a Fiber invoice with the correct UDT type script and SHA256 hash algorithm
    // to pass all validations before the LND call.
    let wrapped_btc_type_script = get_script_by_contract(Contract::SimpleUDT, &[]);
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);
    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(amount_sats),
        signature: None,
        data: InvoiceData {
            payment_hash,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcMinimumExpiryDelta(12),
                Attribute::Description("test".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::PayeePublicKey(public_key),
                Attribute::UdtScript(CkbScript(wrapped_btc_type_script)),
                Attribute::HashAlgorithm(HashAlgorithm::Sha256),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let fiber_pay_req = invoice.to_string();

    // receive_btc will fail at the LND call, but all prior validations
    // (amount, fee, UDT script, hash algorithm) should pass.
    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC {
            fiber_pay_req: fiber_pay_req.clone(),
        }
    )
    .expect("actor call failed");

    // The call should fail due to LND being unavailable, not due to amount validation.
    // This confirms the fee calculation and overflow checks passed successfully,
    // meaning the hold invoice would have been created with the correct total_msat.
    let err = result.unwrap_err();

    // fee_sats = 200_000 * 5_000 / 1_000_000 + 500 = 1_000 + 500 = 1_500
    // total_msat = (200_000 + 1_500) * 1_000 = 201_500_000
    let expected_fee: u128 = 1_500;
    let expected_total_msat: i64 = ((amount_sats + expected_fee) * 1_000) as i64;
    assert_eq!(expected_total_msat, 201_500_000);

    match err {
        CchError::LndRpcError(msg) => {
            assert!(
                msg.contains(&format!("value_msat: {}", expected_total_msat)),
                "hold invoice request should contain value_msat={}, got: {}",
                expected_total_msat,
                msg
            );
        }
        other => panic!(
            "expected LND connection error (no LND server), got: {:?}. \
             If this is an amount error, the fee calculation may be wrong.",
            other
        ),
    }

    // A failed LND invoice creation must release its reservation. Retrying the
    // same request should reach LND again instead of being rejected as tracked.
    let retry_err = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC { fiber_pay_req }
    )
    .expect("actor call failed")
    .unwrap_err();
    assert!(
        matches!(
            &retry_err,
            CchError::LndRpcError(_) | CchError::LndChannelError(_)
        ),
        "failed LND invoice creation should release its tracker reservation, got: {:?}",
        retry_err
    );
}

// =============================================================================
// Insufficient Expiry Delta Tests (#1000)
// =============================================================================

/// Create a test Lightning invoice with a custom min_final_cltv_expiry_delta.
fn create_test_lightning_invoice_with_cltv(
    payment_hash: Hash256,
    min_final_cltv: u64,
) -> lightning_invoice::Bolt11Invoice {
    use bitcoin::hashes::Hash as _;
    use lightning_invoice::{Currency as LnCurrency, InvoiceBuilder as LnInvoiceBuilder};

    let secp = bitcoin::secp256k1::Secp256k1::new();
    let private_key = bitcoin::secp256k1::SecretKey::from_slice(&[43u8; 32]).unwrap();
    let payment_hash_btc = bitcoin::hashes::sha256::Hash::from_slice(payment_hash.as_ref())
        .expect("valid 32-byte hash");
    let payment_secret = lightning_invoice::PaymentSecret([0u8; 32]);
    let duration_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time");

    LnInvoiceBuilder::new(LnCurrency::Bitcoin)
        .description("test invoice".to_string())
        .payment_hash(payment_hash_btc)
        .payment_secret(payment_secret)
        .duration_since_epoch(duration_since_epoch)
        .min_final_cltv_expiry_delta(min_final_cltv)
        .amount_milli_satoshis(100_000_000)
        .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &private_key))
        .expect("build lightning invoice")
}

/// Create a test Fiber invoice with a custom final_tlc_minimum_expiry_delta (in milliseconds).
fn create_test_fiber_invoice_with_expiry(
    payment_hash: Hash256,
    final_tlc_expiry_delta_ms: u64,
) -> CkbInvoice {
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);

    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(100000),
        signature: None,
        data: InvoiceData {
            payment_hash,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcMinimumExpiryDelta(final_tlc_expiry_delta_ms),
                Attribute::Description("test".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::PayeePublicKey(public_key),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    invoice
}

/// Tests that a SendBTC order fails when the incoming CKB TLC does not have enough
/// remaining time to safely settle after the outgoing BTC payment completes.
///
/// Scenario: The order was created a long time ago, so the remaining incoming time
/// is too short to cover the outgoing BTC payment's min_final_cltv_expiry_delta.
///
/// This addresses issue #1000: the check accounts for elapsed time since order creation,
/// not just comparing final expiry deltas statically.
#[tokio::test]
async fn test_send_btc_fails_insufficient_expiry_delta() {
    let (_, payment_hash) = create_valid_preimage_pair(250);
    let store = MockCchOrderStore::new();

    const OUTGOING_MIN_FINAL_CLTV_BLOCKS: u64 = 36;
    const INCOMING_CKB_FINAL_TLC_SECS: u64 = 50_000;
    const ELAPSED_SINCE_ORDER_CREATED_SECS: u64 = 20_000;

    // Create a BTC invoice with min_final_cltv_expiry_delta = OUTGOING_MIN_FINAL_CLTV_BLOCKS
    // (i.e. OUTGOING_MIN_FINAL_CLTV_BLOCKS * BTC_BLOCK_TIME_SECS seconds of CLTV).
    let lightning_invoice =
        create_test_lightning_invoice_with_cltv(payment_hash, OUTGOING_MIN_FINAL_CLTV_BLOCKS);

    // The incoming Fiber invoice uses INCOMING_CKB_FINAL_TLC_SECS for ckb_final_tlc_expiry_delta.
    // Initial static check: outgoing_btc_cltv_secs < INCOMING_CKB_FINAL_TLC_SECS / 2 → passes.
    // But if the order was created ELAPSED_SINCE_ORDER_CREATED_SECS ago:
    //   remaining = INCOMING_CKB_FINAL_TLC_SECS - ELAPSED_SINCE_ORDER_CREATED_SECS
    //   max_outgoing = remaining / 2
    //   needed = outgoing_btc_cltv_secs → fails when max_outgoing < needed
    let ckb_final_tlc_seconds: u64 = INCOMING_CKB_FINAL_TLC_SECS;
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ckb_final_tlc_expiry_delta_seconds: ckb_final_tlc_seconds,
        ..Default::default()
    };

    // Create an order with created_at ELAPSED_SINCE_ORDER_CREATED_SECS in the past.
    // The incoming invoice's final expiry delta must match the config value
    // because compute_max_outgoing_expiry_seconds reads from the stored invoice.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let order = CchOrder {
        created_at: now - ELAPSED_SINCE_ORDER_CREATED_SECS,
        expiry_delta_seconds: 100_000, // large enough not to expire
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: lightning_invoice.to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice_with_expiry(
            payment_hash,
            ckb_final_tlc_seconds * 1000,
        )),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::IncomingAccepted,
        failure_reason: None,
    };

    store.insert_cch_order(order).unwrap();
    let harness = setup_test_harness_with_config_and_store(config, store).await;

    // The CchActor should detect the IncomingAccepted order on startup, dispatch
    // SendOutgoingPayment, and the expiry check should fail the order.
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Failed, 2000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert!(order.failure_reason.is_some());
    let reason = order.failure_reason.unwrap();
    assert!(
        reason.contains("Insufficient HTLC expiry delta"),
        "Expected expiry delta failure message, got: {}",
        reason,
    );
}

/// Tests that a ReceiveBTC order fails when the incoming BTC HTLC does not have enough
/// remaining time for the outgoing CKB payment's final TLC expiry delta.
///
/// Scenario: The outgoing CKB invoice has a very large final_tlc_minimum_expiry_delta
/// that exceeds half the remaining incoming time.
#[tokio::test]
async fn test_receive_btc_fails_insufficient_expiry_delta() {
    let (_, payment_hash) = create_valid_preimage_pair(251);
    let store = MockCchOrderStore::new();

    let incoming_cltv_budget_secs = DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS * BTC_BLOCK_TIME_SECS;
    let max_outgoing_secs = incoming_cltv_budget_secs / 2;
    // Final delta one second past max_outgoing (attribute is in milliseconds).
    let fiber_invoice =
        create_test_fiber_invoice_with_expiry(payment_hash, (max_outgoing_secs + 1) * 1000);

    // DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS sets the incoming CLTV budget in seconds
    // (blocks * BTC_BLOCK_TIME_SECS). The incoming Lightning invoice must carry the same
    // CLTV value because compute_max_outgoing_expiry_seconds reads from the stored invoice.
    // If order was just created: remaining = incoming_cltv_budget_secs, max_outgoing =
    // max_outgoing_secs, outgoing needs max_outgoing_secs + 1 → fails.
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ..Default::default()
    };

    // Create a Lightning invoice whose min_final_cltv_expiry_delta matches
    // DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS.
    let lightning_invoice = create_test_lightning_invoice_with_cltv(
        payment_hash,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS,
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let order = CchOrder {
        created_at: now,
        expiry_delta_seconds: 200_000,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::IncomingAccepted,
        failure_reason: None,
    };

    store.insert_cch_order(order).unwrap();
    let harness = setup_test_harness_with_config_and_store(config, store).await;

    // The CchActor should detect the IncomingAccepted order on startup, dispatch
    // SendOutgoingPayment, and the expiry check should fail the order.
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Failed, 2000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert!(order.failure_reason.is_some());
    let reason = order.failure_reason.unwrap();
    assert!(
        reason.contains("Insufficient HTLC expiry delta"),
        "Expected expiry delta failure message, got: {}",
        reason,
    );
}

/// Tests that a SendBTC order succeeds when there is sufficient remaining incoming time
/// to cover the outgoing payment's CLTV plus settle the incoming payment.
/// This verifies that the expiry check doesn't incorrectly reject valid orders.
#[tokio::test]
async fn test_send_btc_passes_sufficient_expiry_delta() {
    let (_preimage, payment_hash) = create_valid_preimage_pair(252);
    let store = MockCchOrderStore::new();

    const OUTGOING_MIN_FINAL_CLTV_BLOCKS: u64 = 3;
    const INCOMING_CKB_FINAL_TLC_SECS: u64 = 100_000;
    const ELAPSED_SINCE_ORDER_CREATED_SECS: u64 = 10_000;

    let lightning_invoice =
        create_test_lightning_invoice_with_cltv(payment_hash, OUTGOING_MIN_FINAL_CLTV_BLOCKS);

    // The incoming Fiber invoice uses INCOMING_CKB_FINAL_TLC_SECS for ckb_final_tlc_expiry_delta.
    // Even with ELAPSED_SINCE_ORDER_CREATED_SECS elapsed:
    //   remaining = INCOMING_CKB_FINAL_TLC_SECS - ELAPSED_SINCE_ORDER_CREATED_SECS
    //   max_outgoing = remaining / 2
    //   needed = OUTGOING_MIN_FINAL_CLTV_BLOCKS * BTC_BLOCK_TIME_SECS
    //   max_outgoing > needed → passes ✓
    let ckb_final_tlc_seconds: u64 = INCOMING_CKB_FINAL_TLC_SECS;
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ckb_final_tlc_expiry_delta_seconds: ckb_final_tlc_seconds,
        ..Default::default()
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let order = CchOrder {
        created_at: now - ELAPSED_SINCE_ORDER_CREATED_SECS,
        expiry_delta_seconds: 200_000,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: lightning_invoice.to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice_with_expiry(
            payment_hash,
            ckb_final_tlc_seconds * 1000,
        )),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100_000,
        fee_sats: 1_000,
        status: CchOrderStatus::IncomingAccepted,
        failure_reason: None,
    };

    store.insert_cch_order(order).unwrap();
    let harness = setup_test_harness_with_config_and_store(config, store).await;

    // The order should NOT fail from the expiry check. Since outgoing is Lightning (BTC),
    // the SendLightningOutgoingPaymentExecutor will try to call LND (which isn't running).
    // That will cause a transient error and retry, but the order should NOT be Failed.
    // Wait briefly and confirm the order is NOT in Failed state.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let order = harness.get_order(payment_hash).await.unwrap();
    assert_ne!(
        order.status,
        CchOrderStatus::Failed,
        "Order should not have failed - expiry check should have passed. \
         Failure reason: {:?}",
        order.failure_reason,
    );
}
