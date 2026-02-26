//! Happy path integration tests for CCH orders
//!
//! These tests simulate the complete order lifecycle through the CchActor,
//! validating the flow that occurs when SendBTC and ReceiveBTC messages are processed
//! by the CchActor.
//!
//! SendBTC Flow (User pays Lightning invoice via Fiber):
//!   Pending → IncomingAccepted → OutgoingInFlight → OutgoingSucceeded → Succeeded
//!
//! ReceiveBTC Flow (User receives BTC via Lightning, pays Fiber invoice):
//!   Pending → IncomingAccepted → OutgoingInFlight → OutgoingSucceeded → Succeeded

use crate::cch::{
    actor::{CchActor, CchArgs, CchMessage},
    order::{CchInvoice, CchOrder, CchOrderStatus, CchOrderStore},
    trackers::CchTrackingEvent,
    CchConfig, CchError, CchStoreError,
};
use crate::fiber::{
    network::SendPaymentResponse,
    payment::{PaymentStatus, SendPaymentCommand},
    types::Hash256,
    NetworkActorCommand, NetworkActorMessage,
};
use crate::invoice::{Attribute, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceData};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use ractor::{call, port::OutputPortSubscriberTrait, Actor, ActorRef, OutputPort};
use secp256k1::{Secp256k1, SecretKey};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

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
    use crate::fiber::hash_algorithm::HashAlgorithm;
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
#[derive(Clone, Default)]
struct MockNetworkState {
    /// Reference to CchActor to send callbacks
    cch_actor: Arc<Mutex<Option<ActorRef<CchMessage>>>>,
    /// Event port to inject events (simulates FiberStoreWatcher/LndTrackerActor)
    event_port: Arc<OutputPort<CchTrackingEvent>>,
    /// Tracks payment hashes for which SendPayment was called (outgoing Fiber payments)
    sent_fiber_payments: Arc<Mutex<std::collections::HashSet<Hash256>>>,
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

                    // Track that this payment was sent
                    state
                        .sent_fiber_payments
                        .lock()
                        .unwrap()
                        .insert(payment_hash);

                    // Return success response - the executor will create CchTrackingEvent
                    let response = SendPaymentResponse {
                        payment_hash,
                        status: PaymentStatus::Inflight,
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
        if let Ok(invoice) = CkbInvoice::from_str(invoice_str) {
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

/// Set up a test harness with mocked dependencies
async fn setup_test_harness() -> TestHarness {
    setup_test_harness_with_store(MockCchOrderStore::new()).await
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
    };

    let (network_actor, _) = Actor::spawn(None, MockNetworkActor, mock_state.clone())
        .await
        .expect("spawn mock network actor");

    let args = CchArgs {
        config,
        tracker: TaskTracker::new(),
        token: CancellationToken::new(),
        network_actor,
        node_keypair: crate::fiber::KeyPair::try_from([42u8; 32].as_slice()).unwrap(),
        store,
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

async fn setup_test_harness_with_store(store: MockCchOrderStore) -> TestHarness {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ..Default::default()
    };
    setup_test_harness_with_config_and_store(config, store).await
}

/// Create a test Lightning invoice with a specific payment hash
fn create_test_lightning_invoice_with_payment_hash(
    payment_hash: Hash256,
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
    let duration_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time");
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
    // Create a deterministic keypair for tests
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

// =============================================================================
// SendBTC Happy Path Test
// =============================================================================

/// Tests the complete happy path for a SendBTC order.
///
/// Flow: User wants to pay a Lightning invoice using wrapped BTC on Fiber.
/// 1. Hub creates a Fiber invoice for the user to pay
/// 2. User pays the Fiber invoice → IncomingAccepted
/// 3. Hub sends Lightning payment → OutgoingInFlight (via SendLightningOutgoingPaymentExecutor)
/// 4. Lightning payment succeeds with preimage → OutgoingSucceeded
/// 5. Hub settles the Fiber invoice with preimage → Succeeded (via SettleFiberIncomingInvoiceExecutor)
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
    //   OutgoingSucceeded (after payment success) → SettleInvoice dispatched
    //   MockNetworkActor handles SettleInvoice and sends InvoiceChanged(Paid)
    //   → Succeeded (final state)
    // Note: These transitions happen quickly, so we wait for the final Succeeded status
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Succeeded, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Succeeded);
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
/// 4. Fiber payment succeeds with preimage → OutgoingSucceeded
/// 5. Hub settles the Lightning invoice with preimage → Succeeded
#[tokio::test]
async fn test_receive_btc_happy_path() {
    // Generate a valid preimage/payment hash pair
    let (preimage, payment_hash) = create_valid_preimage_pair(99);

    // Set up test harness
    let harness = setup_test_harness().await;

    // Step 1: Create order directly in the database (bypassing LND hold invoice creation)
    // In production, ReceiveBTC creates a hold invoice via LND, but we skip that for testing
    let fiber_invoice = create_test_fiber_invoice(payment_hash);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);
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

    // Wait for order to reach OutgoingSucceeded status
    // CchActor dispatches SettleIncomingInvoice action
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::OutgoingSucceeded, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::OutgoingSucceeded);
    assert_eq!(order.payment_preimage, Some(preimage));

    // Step 5: Simulate LND invoice settlement
    // In production, SettleLightningIncomingInvoiceExecutor calls LND, then LndTrackerActor sends this event
    harness.simulate_lightning_invoice_settled(payment_hash);

    // Wait for order to reach Succeeded status
    let order = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Succeeded, 1000)
        .await;
    assert_eq!(order.status, CchOrderStatus::Succeeded);

    // Verify final state
    assert!(order.is_final());
    assert_eq!(order.payment_preimage, Some(preimage));
    assert!(order.failure_reason.is_none());
}

/// Tests that expired orders are marked as Failed when resuming from the store.
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

/// Tests that final orders (Succeeded/Failed) are skipped when resuming.
#[tokio::test]
async fn test_resume_skips_final_orders() {
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
        status: CchOrderStatus::Succeeded,
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
    assert_eq!(order1.status, CchOrderStatus::Succeeded);
    assert_eq!(order1.payment_preimage, Some(preimage1));

    let order2 = harness.get_order(payment_hash2).await.unwrap();
    assert_eq!(order2.status, CchOrderStatus::Failed);
    assert_eq!(order2.failure_reason, Some("Test failure".to_string()));
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

    // Create a BTC invoice with min_final_cltv_expiry_delta = 36 blocks (= 21,600 seconds)
    let lightning_invoice = create_test_lightning_invoice_with_cltv(payment_hash, 36);

    // Use a config with ckb_final_tlc_expiry_delta_seconds = 50,000 seconds (~14 hours).
    // Initial static check: 36 * 600 = 21,600 < 50,000 / 2 = 25,000 → passes.
    // But if the order was created 20,000 seconds ago:
    //   remaining = 50,000 - 20,000 = 30,000 seconds
    //   max_outgoing = 30,000 / 2 = 15,000 seconds
    //   needed = 21,600 seconds
    //   15,000 < 21,600 → fails!
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ckb_final_tlc_expiry_delta_seconds: 50_000,
        ..Default::default()
    };

    // Create an order with created_at 20,000 seconds in the past
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let order = CchOrder {
        created_at: now - 20_000,
        expiry_delta_seconds: 100_000, // large enough not to expire
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: lightning_invoice.to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
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

    // Create a CKB invoice with a very large final TLC expiry delta (100,000 seconds = 100M ms)
    let fiber_invoice = create_test_fiber_invoice_with_expiry(payment_hash, 100_000_000);

    // Use a config with btc_final_tlc_expiry_delta_blocks = 180 blocks (= 108,000 seconds).
    // If order was just created:
    //   remaining = 108,000 seconds
    //   max_outgoing = 54,000 seconds
    //   outgoing needs 100,000 seconds
    //   54,000 < 100,000 → fails!
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ..Default::default()
    };

    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);
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

    // Create a BTC invoice with small min_final_cltv_expiry_delta = 3 blocks (= 1,800 seconds)
    let lightning_invoice = create_test_lightning_invoice_with_cltv(payment_hash, 3);

    // Use a config with ckb_final_tlc_expiry_delta_seconds = 100,000 seconds.
    // Even with 10,000 seconds elapsed:
    //   remaining = 100,000 - 10,000 = 90,000 seconds
    //   max_outgoing = 45,000 seconds
    //   needed = 3 * 600 = 1,800 seconds
    //   45,000 > 1,800 → passes ✓
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        wrapped_btc_type_script_args: "0x".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ckb_final_tlc_expiry_delta_seconds: 100_000,
        ..Default::default()
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let order = CchOrder {
        created_at: now - 10_000,
        expiry_delta_seconds: 200_000,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: lightning_invoice.to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
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
