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
    actor::{CchActor, CchArgs, CchMessage, BTC_BLOCK_TIME_MILLIS},
    config::{
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS, DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS,
    },
    order::CchOrderStore,
    trackers::CchTrackingEvent,
    CchConfig, CchError, CchStoreError,
};
use crate::fiber::{
    network::SendPaymentResponse, payment::SendPaymentCommand, NetworkActorCommand,
    NetworkActorMessage,
};
use crate::invoice::{Attribute, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceData};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use fiber_types::{
    CchInvoice, CchOrder, CchOrderStatus, Hash256, HashAlgorithm, PaymentStatus, SwapProposal,
};
use ractor::{call, call_t, port::OutputPortSubscriberTrait, Actor, ActorRef, OutputPort};
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
    pending_proposals: Arc<Mutex<HashMap<Hash256, SwapProposal>>>,
}

impl MockCchOrderStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a pending proposal directly into the backing map, bypassing the
    /// actor. Used to simulate proposals persisted before a restart.
    pub fn seed_pending_proposal(&self, proposal: SwapProposal) {
        self.pending_proposals
            .lock()
            .unwrap()
            .insert(proposal.payment_hash, proposal);
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

    fn get_cch_pending_proposal(
        &self,
        payment_hash: &Hash256,
    ) -> Result<SwapProposal, CchStoreError> {
        self.pending_proposals
            .lock()
            .unwrap()
            .get(payment_hash)
            .ok_or(CchStoreError::NotFound(*payment_hash))
            .cloned()
    }

    fn insert_cch_pending_proposal(&self, proposal: SwapProposal) -> Result<(), CchStoreError> {
        let mut proposals = self.pending_proposals.lock().unwrap();
        let payment_hash = proposal.payment_hash;
        match proposals.insert(payment_hash, proposal) {
            Some(_) => Err(CchStoreError::Duplicated(payment_hash)),
            None => Ok(()),
        }
    }

    fn get_cch_pending_proposal_keys_iter(&self) -> impl IntoIterator<Item = Hash256> {
        self.pending_proposals
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>()
    }

    fn delete_cch_pending_proposal(&self, payment_hash: &Hash256) {
        let mut proposals = self.pending_proposals.lock().unwrap();
        proposals.remove(payment_hash);
    }
}

/// Unwrap a `send_btc` / `receive_btc` result as a created order, panicking if
/// the request entered the proposal flow instead. Used by fast-path tests
/// where a fixed-rate asset is expected to mint the order inline.
fn expect_order(result: fiber_types::NewOrderResult) -> CchOrder {
    match result {
        fiber_types::NewOrderResult::Order(order) => order,
        fiber_types::NewOrderResult::PendingProposal(_) => {
            panic!("expected an order, got a pending proposal")
        }
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
#[derive(Clone, Default)]
struct MockNetworkState {
    /// Reference to CchActor to send callbacks
    cch_actor: Arc<Mutex<Option<ActorRef<CchMessage>>>>,
    /// Event port to inject events (simulates FiberStoreWatcher/LndTrackerActor)
    event_port: Arc<OutputPort<CchTrackingEvent>>,
    /// Tracks payment hashes for which SendPayment was called (outgoing Fiber payments)
    sent_fiber_payments: Arc<Mutex<std::collections::HashSet<Hash256>>>,
    /// Tracks the `max_fee_amount` of each outgoing Fiber SendPayment, keyed by payment hash.
    sent_fiber_payment_fees: Arc<Mutex<std::collections::HashMap<Hash256, Option<u128>>>>,
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
                    state
                        .sent_fiber_payment_fees
                        .lock()
                        .unwrap()
                        .insert(payment_hash, cmd.max_fee_amount);

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

    /// Wait until `get_cch_order` reports the order as absent (NotFound). Used
    /// for the proposal reject/timeout paths, where the pending proposal is
    /// deleted and no order is ever created.
    async fn wait_for_order_absent(&self, payment_hash: Hash256, timeout_ms: u64) {
        let start = std::time::Instant::now();
        let poll_interval = tokio::time::Duration::from_millis(10);
        let timeout = tokio::time::Duration::from_millis(timeout_ms);

        loop {
            match self.get_order(payment_hash).await {
                Err(CchError::StoreError(CchStoreError::NotFound(_))) => return,
                other => {
                    if start.elapsed() > timeout {
                        panic!(
                            "Timeout waiting for order {:x} to be absent. Last result: {:?}",
                            payment_hash, other
                        );
                    }
                }
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

    /// Return the `max_fee_amount` of the outgoing Fiber SendPayment for `payment_hash`, if sent.
    fn fiber_payment_max_fee(&self, payment_hash: Hash256) -> Option<Option<u128>> {
        self.mock_state
            .sent_fiber_payment_fees
            .lock()
            .unwrap()
            .get(&payment_hash)
            .copied()
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

        let result = call!(
            self.actor,
            CchMessage::SendBTC,
            crate::cch::actor::SendBTC {
                btc_pay_req,
                currency: Currency::Fibb,
                fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
            }
        )
        .expect("actor call failed")?;

        Ok((expect_order(result), preimage))
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

/// Poll the actor's pending-proposal map until a proposal id appears, returning
/// the first one. Panics if none is registered within two seconds.
async fn wait_for_pending_proposal_id(actor: &ActorRef<CchMessage>) -> Hash256 {
    let start = std::time::Instant::now();
    loop {
        let ids: Vec<Hash256> =
            ractor::call!(actor, CchMessage::TestPendingProposalIds).expect("query pending ids");
        if let Some(id) = ids.into_iter().next() {
            return id;
        }
        if start.elapsed() > std::time::Duration::from_secs(2) {
            panic!("actor never registered a pending proposal");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn setup_test_harness_with_config(config: CchConfig) -> TestHarness {
    setup_test_harness_with_config_and_store(config, MockCchOrderStore::new()).await
}

async fn setup_test_harness_with_store(store: MockCchOrderStore) -> TestHarness {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        ..Default::default()
    };
    setup_test_harness_with_config_and_store(config, store).await
}

async fn setup_test_harness_with_config_and_store(
    config: CchConfig,
    store: MockCchOrderStore,
) -> TestHarness {
    // Ensure tests have at least one Fiber asset configured. Default the
    // allowlist to the empty (default) script when nothing is set, and
    // synthesise a 1:1 fixed-rate entry for every allowlisted asset when the
    // caller did not supply ANY fixed-rate config (preserving the legacy
    // wrapped-BTC fee-math semantics: 1 sat = 1 smallest unit). When the
    // caller supplies a non-empty `fixed_rate_assets`, we leave it untouched
    // so tests can deliberately exercise the allowlisted-but-not-fixed-rate
    // rejection path.
    let mut config = if config.fiber_asset_allowlist.is_empty() {
        CchConfig {
            fiber_asset_allowlist: vec![Some(ckb_jsonrpc_types::Script::default())],
            ..config
        }
    } else {
        config
    };
    if config.fixed_rate_assets.is_empty() {
        for asset in config.fiber_asset_allowlist.clone() {
            config
                .fixed_rate_assets
                .push(crate::cch::config::FixedRateAsset {
                    fiber_asset: asset,
                    smallest_units_per_sat: 1,
                });
        }
    }

    let event_port = Arc::new(OutputPort::<CchTrackingEvent>::default());

    let mock_state = MockNetworkState {
        cch_actor: Arc::new(Mutex::new(None)),
        event_port: event_port.clone(),
        sent_fiber_payments: Arc::new(Mutex::new(std::collections::HashSet::new())),
        sent_fiber_payment_fees: Arc::new(Mutex::new(std::collections::HashMap::new())),
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
    create_test_fiber_invoice_with_amount(payment_hash, 100000)
}

/// Create a test Fiber invoice with a specific amount
fn create_test_fiber_invoice_with_amount(payment_hash: Hash256, amount: u128) -> CkbInvoice {
    use crate::invoice::CkbScript;
    use ckb_types::packed::Script;

    // Create a deterministic keypair for tests
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);

    let default_expiry_delta_ms = DEFAULT_CKB_FINAL_TLC_EXPIRY_DELTA_SECONDS * 1000;
    // Pick a final-TLC expiry that always satisfies the ReceiveBTC validation
    // (`ckb_final_tlc_millis < btc_final_cltv_millis / 2`). Half the
    // configured BTC budget converted to milliseconds gives plenty of headroom
    // while staying well below the limit; `min` against the CKB default keeps
    // SendBTC-flow callers (which compare against the CKB budget) happy too.
    let safe_final_tlc_ms = std::cmp::min(
        default_expiry_delta_ms,
        DEFAULT_BTC_FINAL_TLC_EXPIRY_DELTA_BLOCKS * BTC_BLOCK_TIME_MILLIS / 4,
    );
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
                Attribute::FinalHtlcMinimumExpiryDelta(safe_final_tlc_ms),
                Attribute::Description("test".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::PayeePublicKey(public_key),
                Attribute::HashAlgorithm(HashAlgorithm::Sha256),
                // Match the default test-harness allowlist (Some(default Script)).
                Attribute::UdtScript(CkbScript(Script::default())),
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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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

/// Drive a ReceiveBTC-style order (incoming Lightning, outgoing Fiber) up to the point where
/// the outgoing Fiber `SendPayment` has been dispatched, and return the `max_fee_amount` that
/// was attached to the `SendPaymentCommand`.
///
/// `amount_sats`/`fee_sats` configure the BTC-leg economics; `fiber_invoice_amount` sets the
/// Fiber-leg amount so callers can exercise the cross-asset fee conversion. The captured budget is
/// denominated in the Fiber asset's smallest unit (not satoshis).
async fn dispatch_fiber_outgoing_and_capture_fee(
    harness: &TestHarness,
    seed: u8,
    amount_sats: u128,
    fee_sats: u128,
    fiber_invoice_amount: u128,
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
        fiber_type_script: None,
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: amount_sats * 1000,
        btc_fee_msat: fee_sats * 1000,
        fiber_invoice_amount,
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
/// The outgoing Fiber route fee must be capped at the value the operator collected, converted
/// into the Fiber asset's smallest unit at the order's net exchange rate (excluding the hub fee),
/// not at Fiber's default 0.5%-of-amount cap.
#[tokio::test]
async fn test_receive_btc_outgoing_fiber_fee_capped_at_collected_fee() {
    let harness = setup_test_harness().await;

    // Pick economics where the default Fiber cap (0.5% * amount) vastly exceeds the tiny collected
    // CCH fee. The Fiber leg uses a finer-grained unit than a satoshi (10 units per net-sat) so the
    // budget is genuinely converted into the Fiber asset, not passed through as raw sats.
    let amount_sats = 1_000_000u128;
    let fee_sats = 100u128;
    // net_btc_msat = (amount_sats - fee_sats) * 1000 = 999_900_000.
    let fiber_invoice_amount = (amount_sats - fee_sats) * 10; // 10 fiber units per net-sat
    let max_fee = dispatch_fiber_outgoing_and_capture_fee(
        &harness,
        60,
        amount_sats,
        fee_sats,
        fiber_invoice_amount,
    )
    .await;

    // budget = btc_fee_msat * pct/100 * fiber_units / net_btc_msat, with pct = 100 (default).
    let net_btc_msat = (amount_sats - fee_sats) * 1000;
    let expected = (fee_sats * 1000) * fiber_invoice_amount / net_btc_msat;
    assert_eq!(expected, 1_000, "sanity: expected budget is in fiber units");
    assert_eq!(
        max_fee,
        Some(expected),
        "outgoing Fiber payment must be capped at the collected CCH fee converted to fiber units"
    );

    // Sanity check: the default user-payment cap (in fiber units) would have been much larger.
    let default_cap = fiber_invoice_amount * 5 / 1000;
    assert!(
        default_cap > expected,
        "test scenario must have default cap exceeding collected fee budget to be meaningful"
    );
}

/// The `max_outgoing_fee_percentage` config knob must scale the outgoing fee budget.
#[tokio::test]
async fn test_receive_btc_outgoing_fiber_fee_scaled_by_percentage() {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        max_outgoing_fee_percentage: 50,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let amount_sats = 1_000_000u128;
    let fee_sats = 1_000u128;
    let fiber_invoice_amount = (amount_sats - fee_sats) * 10; // 10 fiber units per net-sat
    let max_fee = dispatch_fiber_outgoing_and_capture_fee(
        &harness,
        61,
        amount_sats,
        fee_sats,
        fiber_invoice_amount,
    )
    .await;

    // budget = btc_fee_msat * 50/100 * fiber_units / net_btc_msat.
    let net_btc_msat = (amount_sats - fee_sats) * 1000;
    let expected = (fee_sats * 1000) * 50 / 100 * fiber_invoice_amount / net_btc_msat;
    assert_eq!(
        max_fee,
        Some(expected),
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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash)),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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

/// Tests that final orders (Success/Failed) are skipped when resuming.
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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash1)),
        payment_hash: payment_hash1,
        payment_preimage: Some(preimage1),
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
        status: CchOrderStatus::Success,
        failure_reason: None,
    };

    let failed_order = CchOrder {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expiry_delta_seconds: 3600,
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice(payment_hash2)),
        payment_hash: payment_hash2,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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
            fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
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
            fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
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

/// Tests that send_btc rejects a `fiber_type_script` that is not in the
/// configured allowlist.
#[tokio::test]
async fn test_send_btc_rejects_unallowlisted_fiber_asset() {
    let harness = setup_test_harness().await;
    // Harness allowlist contains only Some(default Script).

    let (_, payment_hash) = create_valid_preimage_pair(120);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);

    // Build a UDT script that is *not* in the allowlist.
    let other_script = ckb_jsonrpc_types::Script {
        code_hash: [1u8; 32].into(),
        hash_type: ckb_jsonrpc_types::ScriptHashType::Type,
        args: ckb_jsonrpc_types::JsonBytes::from_vec(vec![0xab]),
    };

    let result = call!(
        harness.actor,
        CchMessage::SendBTC,
        crate::cch::actor::SendBTC {
            btc_pay_req: lightning_invoice.to_string(),
            currency: Currency::Fibb,
            fiber_type_script: Some(other_script),
        }
    )
    .expect("actor call failed");

    match result {
        Err(CchError::FiberAssetNotAllowlisted) => {} // Expected
        other => panic!("Expected FiberAssetNotAllowlisted, got {:?}", other),
    }
}

/// Tests that send_btc accepts a `fiber_type_script: None` (native CKB) when
/// `None` is in the allowlist, and that the resulting proxy Fiber invoice has
/// no `UdtScript` attribute.
#[tokio::test]
async fn test_send_btc_accepts_native_ckb_when_allowlisted() {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        // Allowlist native CKB only.
        fiber_asset_allowlist: vec![None],
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (_, payment_hash) = create_valid_preimage_pair(121);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);

    let result = call!(
        harness.actor,
        CchMessage::SendBTC,
        crate::cch::actor::SendBTC {
            btc_pay_req: lightning_invoice.to_string(),
            currency: Currency::Fibb,
            fiber_type_script: None,
        }
    )
    .expect("actor call failed")
    .expect("send_btc should succeed for allowlisted native CKB");
    let order = expect_order(result);

    assert!(
        order.fiber_type_script.is_none(),
        "order.fiber_type_script should be None for native CKB"
    );
    let fiber_invoice = match &order.incoming_invoice {
        CchInvoice::Fiber(inv) => inv.clone(),
        other => panic!("expected Fiber invoice, got: {:?}", other),
    };
    assert!(
        fiber_invoice.udt_type_script().is_none(),
        "native-CKB proxy invoice should have no UdtScript attribute"
    );
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

/// Tests that receive_btc rejects a Fiber invoice whose asset is not in the
/// configured allowlist. The harness only allowlists a default UDT script, so
/// a plain CKB invoice (no UDT script attribute) is rejected.
#[tokio::test]
async fn test_receive_btc_rejects_ckb_invoice_without_udt() {
    let harness = setup_test_harness().await;
    // The harness is configured with Currency::Fibb and only Some(default UDT)
    // in the allowlist; native CKB (None) is therefore not allowlisted.

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
        Err(CchError::FiberAssetNotAllowlisted) => {} // Expected
        other => panic!("Expected FiberAssetNotAllowlisted, got {:?}", other),
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

/// Tests that the send_btc proxy Fiber invoice includes the fee in its amount.
///
/// In the SendBTC flow, the hub creates a Fiber invoice (the proxy invoice) for
/// the user to pay. The Fiber leg is priced off the gross BTC amount
/// (Bolt11 amount + fee) so the hub collects enough (in the Fiber asset's
/// smallest unit) to cover the outgoing Lightning payment plus its fee, while
/// `lightning_invoice_amount` itself stays equal to the (fee-exclusive) Bolt11
/// amount the hub pays.
#[tokio::test]
async fn test_send_btc_proxy_invoice_includes_fee() {
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        base_fee_sats: 1_000, // 1000 sat base fee to make the fee clearly visible
        fee_rate_per_million_sats: 10_000, // 1% proportional fee
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    // The lightning invoice has 100_000_000 msat = 100_000 sats
    let (order, _preimage) = harness.create_send_btc_order_with_preimage().await.unwrap();

    // btc_fee_msat = amount_msat * fee_rate_per_million / 1_000_000 + base_fee_sats * 1_000
    //              = 100_000_000 * 10_000 / 1_000_000 + 1_000 * 1_000
    //              = 1_000_000 + 1_000_000
    //              = 2_000_000 msat
    let expected_fee_msat: u128 = 2_000_000;
    assert_eq!(
        order.btc_fee_msat, expected_fee_msat,
        "btc_fee_msat should be calculated from rate + base"
    );

    // For SendBTC the Lightning leg is the outgoing Bolt11 the hub pays, so
    // lightning_invoice_amount equals the (fee-exclusive) Bolt11 amount.
    let expected_lightning_invoice_amount: u128 = 100_000_000;
    assert_eq!(
        order.lightning_invoice_amount, expected_lightning_invoice_amount,
        "lightning_invoice_amount should equal the fee-exclusive Bolt11 amount"
    );

    // The Fiber leg is priced off the gross BTC amount (amount + fee):
    //   fiber_invoice_amount = ceil((amount_msat + btc_fee_msat) * rate / 1000)
    //                        = (100_000_000 + 2_000_000) * 1 / 1000 = 102_000
    let expected_fiber_amount: u128 = 102_000;
    assert_eq!(
        order.fiber_invoice_amount, expected_fiber_amount,
        "fiber_invoice_amount should be priced off the gross BTC amount (amount + fee)"
    );

    // Verify the Fiber invoice stored in the order also has the correct amount
    let fiber_invoice = match &order.incoming_invoice {
        CchInvoice::Fiber(inv) => inv.clone(),
        other => panic!("expected Fiber invoice, got: {:?}", other),
    };
    assert_eq!(
        fiber_invoice.amount(),
        Some(expected_fiber_amount),
        "Fiber proxy invoice amount should match fiber_invoice_amount"
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

    let fiber_type_script = get_script_by_contract(Contract::SimpleUDT, &[]);
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        base_fee_sats: 500,
        fee_rate_per_million_sats: 5_000, // 0.5% proportional fee
        fiber_asset_allowlist: vec![Some(fiber_type_script.clone().into())],
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (_preimage, payment_hash) = create_valid_preimage_pair(180);
    let amount_sats: u128 = 200_000;

    // Build a Fiber invoice with the correct UDT type script and SHA256 hash algorithm
    // to pass all validations before the LND call.
    let wrapped_btc_type_script = fiber_type_script;
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

    // receive_btc will fail at the LND call, but all prior validations
    // (amount, fee, UDT script, hash algorithm) should pass.
    let result = call!(
        harness.actor,
        CchMessage::ReceiveBTC,
        crate::cch::ReceiveBTC {
            fiber_pay_req: invoice.to_string(),
        }
    )
    .expect("actor call failed");

    // The call should fail due to LND being unavailable, not due to amount validation.
    // This confirms the fee calculation and overflow checks passed successfully,
    // meaning the hold invoice would have been created with the correct total_msat.
    let err = result.unwrap_err();

    // btc_fee_msat = amount_sats * fee_rate_per_million / 1_000 + base_fee_sats * 1_000
    //              = 200_000 * 5_000 / 1_000 + 500 * 1_000
    //              = 1_000_000 + 500_000
    //              = 1_500_000 msat
    // lightning_invoice_amount = amount_sats * 1_000 + btc_fee_msat = 201_500_000 msat
    let expected_total_msat: i64 = 201_500_000;

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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: lightning_invoice.to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice_with_expiry(
            payment_hash,
            ckb_final_tlc_seconds * 1000,
        )),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: fiber_invoice.to_string(),
        incoming_invoice: CchInvoice::Lightning(lightning_invoice),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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
        fiber_type_script: Some(ckb_jsonrpc_types::Script::default()),
        outgoing_pay_req: lightning_invoice.to_string(),
        incoming_invoice: CchInvoice::Fiber(create_test_fiber_invoice_with_expiry(
            payment_hash,
            ckb_final_tlc_seconds * 1000,
        )),
        payment_hash,
        payment_preimage: None,
        lightning_invoice_amount: 100_000_000,
        btc_fee_msat: 1_000_000,
        fiber_invoice_amount: 100_000,
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

/// Tests that send_btc applies a non-1:1 fixed-rate when computing the Fiber
/// proxy invoice amount. With `smallest_units_per_sat = 100` (Fiber asset is
/// 100x more numerous per sat), the Fiber-leg amount must be
/// `lightning_invoice_amount * 100 / 1000 = lightning_invoice_amount / 10`.
#[tokio::test]
async fn test_send_btc_applies_fixed_rate_non_one_to_one() {
    use crate::cch::config::FixedRateAsset;

    let asset_script = ckb_jsonrpc_types::Script::default();
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        // Zero fees so the amount math is unambiguous.
        base_fee_sats: 0,
        fee_rate_per_million_sats: 0,
        fiber_asset_allowlist: vec![Some(asset_script.clone())],
        fixed_rate_assets: vec![FixedRateAsset {
            fiber_asset: Some(asset_script.clone()),
            smallest_units_per_sat: 100,
        }],
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (order, _preimage) = harness.create_send_btc_order_with_preimage().await.unwrap();

    // The harness lightning invoice is 100_000_000 msat.
    assert_eq!(order.btc_fee_msat, 0);
    assert_eq!(order.lightning_invoice_amount, 100_000_000);
    // fiber = 100_000_000 * 100 / 1000 = 10_000_000
    assert_eq!(
        order.fiber_invoice_amount, 10_000_000,
        "non-1:1 rate should scale the Fiber-leg amount accordingly"
    );
}

/// Tests that receive_btc applies a non-1:1 fixed-rate when deriving the
/// BTC-leg amount from the Fiber invoice. With `smallest_units_per_sat = 100`,
/// `btc_amount_msat_before_fee = fiber * 1000 / 100 = fiber * 10`.
#[tokio::test]
async fn test_receive_btc_applies_fixed_rate_non_one_to_one() {
    use crate::cch::config::FixedRateAsset;
    use crate::invoice::CkbScript;

    let asset_script = ckb_jsonrpc_types::Script::default();
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        base_fee_sats: 0,
        fee_rate_per_million_sats: 0,
        fiber_asset_allowlist: vec![Some(asset_script.clone())],
        fixed_rate_assets: vec![FixedRateAsset {
            fiber_asset: Some(asset_script.clone()),
            smallest_units_per_sat: 100,
        }],
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    // Build an invoice with a small final-TLC delta so the expiry sanity check
    // passes and we reach the LND call.
    let (_, payment_hash) = create_valid_preimage_pair(200);
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);
    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(1_000_000),
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
                Attribute::UdtScript(CkbScript(ckb_types::packed::Script::default())),
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

    // LND is not actually running so the call fails at the AddHoldInvoice
    // step, but only after the rate-driven fee math has been computed and the
    // BTC-leg total assembled. We assert the failure surface comes from LND,
    // not from anything earlier in the pipeline.
    match result {
        Err(CchError::LndChannelError(_)) | Err(CchError::LndRpcError(_)) => {}
        other => panic!("expected LND failure after rate math, got: {:?}", other),
    }
}

/// Tests that send_btc routes an allowlisted-but-not-fixed-rate asset to the
/// proposal flow. The call returns immediately with a pending-proposal result;
/// with no operator subscribed, the proposal times out and is dropped — no
/// order is ever persisted, so `get_cch_order` reports it absent.
#[tokio::test]
async fn test_send_btc_rejects_allowlisted_without_fixed_rate() {
    let asset_script = ckb_jsonrpc_types::Script::default();
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        // Asset is allowlisted...
        fiber_asset_allowlist: vec![Some(asset_script.clone())],
        // ...but there is no fixed-rate entry for it (and the harness only
        // auto-fills entries when a non-empty allowlist would otherwise lack
        // them — we deliberately set fixed_rate_assets to a NON-matching entry
        // to defeat that logic).
        fixed_rate_assets: vec![crate::cch::config::FixedRateAsset {
            fiber_asset: None,
            smallest_units_per_sat: 1,
        }],
        // Use a tiny timeout so the test exercises the proposal-timeout path
        // without taking the full default 30s.
        swap_proposal_timeout_seconds: 1,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (_, payment_hash) = create_valid_preimage_pair(201);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);

    // The call returns immediately with a pending-proposal result — no blocking.
    let result = call_t!(
        harness.actor,
        CchMessage::SendBTC,
        1_000,
        crate::cch::actor::SendBTC {
            btc_pay_req: lightning_invoice.to_string(),
            currency: Currency::Fibb,
            fiber_type_script: Some(asset_script),
        }
    )
    .expect("actor call failed")
    .expect("send_btc should return a pending proposal");
    assert!(
        matches!(result, fiber_types::NewOrderResult::PendingProposal(_)),
        "expected PendingProposal, got {:?}",
        result
    );

    // With no operator subscribed, the proposal times out and is dropped; the
    // order never materialises, so it is reported absent.
    harness.wait_for_order_absent(payment_hash, 5_000).await;
}

/// Tests that an operator's accept response on a `SendBTC` proposal causes
/// the hub to mint the Fiber-leg invoice with the operator-supplied amount and
/// flip the order to `Pending`.
#[tokio::test]
async fn test_send_btc_proposal_path_accept() {
    use fiber_types::SwapProposalResponse;

    let asset_script = ckb_jsonrpc_types::Script::default();
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        fiber_asset_allowlist: vec![Some(asset_script.clone())],
        // No fixed-rate entry for `Some(asset_script)` — proposal path.
        fixed_rate_assets: vec![crate::cch::config::FixedRateAsset {
            fiber_asset: None,
            smallest_units_per_sat: 1,
        }],
        swap_proposal_timeout_seconds: 5,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (_, payment_hash) = create_valid_preimage_pair(202);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);

    // The call returns immediately with a pending-proposal result.
    let result = call_t!(
        harness.actor,
        CchMessage::SendBTC,
        1_000,
        crate::cch::actor::SendBTC {
            btc_pay_req: lightning_invoice.to_string(),
            currency: Currency::Fibb,
            fiber_type_script: Some(asset_script.clone()),
        }
    )
    .expect("actor call failed")
    .expect("send_btc should return a pending proposal");
    assert!(
        matches!(result, fiber_types::NewOrderResult::PendingProposal(_)),
        "expected PendingProposal, got {:?}",
        result
    );

    // Discover the actual proposal id from the actor's pending map.
    let proposal_id = wait_for_pending_proposal_id(&harness.actor).await;

    let resp = SwapProposalResponse {
        proposal_id,
        accept: true,
        counterparty_leg_amount: Some(123_456),
        reject_reason: None,
    };
    let result = call_t!(
        harness.actor,
        CchMessage::SubmitSwapProposalResponse,
        1_000,
        resp
    )
    .expect("submit call failed");
    result.expect("submit accepted");

    // The order resumes to `Pending` with the operator-supplied Fiber amount.
    let resumed = harness
        .wait_for_order_status(payment_hash, CchOrderStatus::Pending, 5_000)
        .await;
    assert_eq!(resumed.fiber_invoice_amount, 123_456);
    assert_eq!(resumed.fiber_type_script, Some(asset_script));
    assert!(matches!(resumed.incoming_invoice, CchInvoice::Fiber(_)));
}

/// Tests that an operator's reject response on a `ReceiveBTC` proposal causes
/// the hub to fail the order. We exercise the reject path (rather than accept)
/// because accepting would then require LND to mint a hold invoice, which the
/// test harness intentionally does not provide.
#[tokio::test]
async fn test_receive_btc_proposal_path_reject() {
    use fiber_types::SwapProposalResponse;

    let asset_script = ckb_jsonrpc_types::Script::default();
    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        fiber_asset_allowlist: vec![Some(asset_script.clone())],
        // No fixed-rate entry → ReceiveBTC must take the proposal path.
        fixed_rate_assets: vec![crate::cch::config::FixedRateAsset {
            fiber_asset: None,
            smallest_units_per_sat: 1,
        }],
        swap_proposal_timeout_seconds: 5,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config(config).await;

    let (_, payment_hash) = create_valid_preimage_pair(33);
    // Build a Fiber invoice that (a) carries a `UdtScript` so its
    // `udt_type_script` matches the test's allowlist of `Some(default Script)`,
    // and (b) uses a small `FinalHtlcMinimumExpiryDelta` so the
    // `ckb_final_tlc_millis < btc_final_cltv_millis / 2` guard passes with
    // the default `btc_final_tlc_expiry_delta_blocks`.
    let fiber_invoice = {
        use crate::invoice::CkbScript;
        use ckb_types::packed::Script;
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
                    Attribute::Description("test".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(3600)),
                    Attribute::PayeePublicKey(public_key),
                    Attribute::HashAlgorithm(HashAlgorithm::Sha256),
                    Attribute::UdtScript(CkbScript(Script::default())),
                ],
            },
        };
        invoice
            .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();
        invoice
    };

    // The call returns immediately with a pending-proposal result.
    let result = call_t!(
        harness.actor,
        CchMessage::ReceiveBTC,
        1_000,
        crate::cch::actor::ReceiveBTC {
            fiber_pay_req: fiber_invoice.to_string(),
        }
    )
    .expect("actor call failed")
    .expect("receive_btc should return a pending proposal");
    assert!(
        matches!(result, fiber_types::NewOrderResult::PendingProposal(_)),
        "expected PendingProposal, got {:?}",
        result
    );

    // Discover the actual proposal id from the actor's pending map.
    let proposal_id = wait_for_pending_proposal_id(&harness.actor).await;

    let resp = SwapProposalResponse {
        proposal_id,
        accept: false,
        counterparty_leg_amount: None,
        reject_reason: Some("operator declined".to_string()),
    };
    let result = call_t!(
        harness.actor,
        CchMessage::SubmitSwapProposalResponse,
        1_000,
        resp
    )
    .expect("submit call failed");
    result.expect("submit accepted");

    // The proposal is dropped on reject; no order is created, so it is absent.
    harness.wait_for_order_absent(payment_hash, 5_000).await;

    // A response for an unknown proposal id must be rejected with
    // `SwapProposalUnknown`.
    let bogus_id = fiber_types::Hash256::from([0xAB; 32]);
    let bogus_resp = SwapProposalResponse {
        proposal_id: bogus_id,
        accept: true,
        counterparty_leg_amount: Some(1),
        reject_reason: None,
    };
    let result = call_t!(
        harness.actor,
        CchMessage::SubmitSwapProposalResponse,
        500,
        bogus_resp
    )
    .expect("submit call failed");
    assert!(
        matches!(result, Err(CchError::SwapProposalUnknown)),
        "expected SwapProposalUnknown for unknown id, got {:?}",
        result
    );
}

/// Tests that a pending proposal persisted before a restart is resumed:
/// re-registered in the pending map, re-broadcast, and—since no operator
/// answers—dropped once its proposal deadline elapses (no order is created).
#[tokio::test]
async fn test_pending_proposal_resumed_on_restart() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let (_, payment_hash) = create_valid_preimage_pair(77);
    let lightning_invoice = create_test_lightning_invoice_with_payment_hash(payment_hash);
    let proposal_id = Hash256::from([0x77u8; 32]);
    let proposal = SwapProposal {
        proposal_id,
        order_id: payment_hash,
        direction: fiber_types::SwapDirection::SendBTC,
        payment_hash,
        fiber_asset: None,
        fiber_invoice_amount: None,
        lightning_invoice_amount: Some(100_000_000),
        configured_fee_rate_per_million_sats: 0,
        configured_base_fee_sats: 0,
        fee_on_btc_side_msat: Some(0),
        submitted_invoice: lightning_invoice.to_string(),
        // Proposal expires shortly so the resumed timeout fires during the test.
        expires_at: now + 1,
        created_at: now,
    };

    let store = MockCchOrderStore::new();
    store.seed_pending_proposal(proposal);

    let config = CchConfig {
        lnd_rpc_url: "https://127.0.0.1:10009".to_string(),
        min_outgoing_invoice_expiry_delta_seconds: 60,
        swap_proposal_timeout_seconds: 1,
        ..Default::default()
    };
    let harness = setup_test_harness_with_config_and_store(config, store).await;

    // The persisted proposal is re-registered in the pending map on startup.
    let resumed_id = wait_for_pending_proposal_id(&harness.actor).await;
    assert_eq!(resumed_id, proposal_id);

    // Once the proposal deadline elapses with no operator response, the
    // pending proposal is dropped and no order is created.
    harness.wait_for_order_absent(payment_hash, 5_000).await;
}
