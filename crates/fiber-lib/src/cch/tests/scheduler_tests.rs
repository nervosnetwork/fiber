//! Tests for the CCH order scheduler actor
//!
//! These tests validate the heap-based scheduling system for order expiry and pruning.

use crate::cch::trackers::LndTrackerMessage;
use crate::cch::{
    order::{CchInvoice, CchOrder, CchOrderStatus, CchOrderStore},
    scheduler::{CchOrderSchedulerActor, SchedulerArgs, SchedulerMessage, PRUNE_DELAY_SECONDS},
    tests::actor_tests::MockCchOrderStore,
    CchStoreError,
};
use crate::fiber::types::Hash256;
use crate::invoice::{Attribute, CkbInvoice, Currency, InvoiceData};
use crate::tests::test_utils::get_test_root_actor;
use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use ractor::{Actor, ActorRef};
use secp256k1::{Secp256k1, SecretKey};

/// Mock LND tracker actor for testing
#[derive(Default)]
struct MockLndTrackerActor;

#[async_trait::async_trait]
impl Actor for MockLndTrackerActor {
    type Msg = LndTrackerMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ractor::ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        _message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ractor::ActorProcessingErr> {
        // Mock tracker - just accept all messages
        Ok(())
    }
}

/// Helper function to create a test payment hash
fn test_payment_hash(value: u8) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    Hash256::from(bytes)
}

/// Helper function to create a test Fiber invoice
fn create_test_fiber_invoice(payment_hash: Hash256) -> CkbInvoice {
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

/// Helper function to create a minimal test order
fn create_test_order(
    payment_hash: Hash256,
    created_at: u64,
    expiry_delta_seconds: u64,
    status: CchOrderStatus,
) -> CchOrder {
    let invoice = create_test_fiber_invoice(payment_hash);

    CchOrder {
        created_at,
        expiry_delta_seconds,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script::default(),
        outgoing_pay_req: "test".to_string(),
        incoming_invoice: CchInvoice::Fiber(invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 1000,
        fee_sats: 0,
        status,
        failure_reason: None,
    }
}

/// Setup test scheduler actor
async fn setup_scheduler() -> (
    ActorRef<SchedulerMessage>,
    MockCchOrderStore,
    ActorRef<LndTrackerMessage>,
) {
    let store = MockCchOrderStore::new();
    let (lnd_tracker, _) = Actor::spawn(None, MockLndTrackerActor, ())
        .await
        .expect("spawn mock lnd tracker");

    let root_actor = get_test_root_actor().await;
    let scheduler = CchOrderSchedulerActor::start(
        SchedulerArgs {
            store: store.clone(),
            lnd_tracker: lnd_tracker.clone(),
        },
        root_actor.get_cell(),
    )
    .await
    .expect("spawn scheduler actor");

    (scheduler, store, lnd_tracker)
}

#[tokio::test]
async fn test_schedule_expiry_job() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash = test_payment_hash(1);
    // Use a recent created_at time so that prune time is in the future
    // But expiry time should be in the past
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 3600; // 1 hour
    let created_at = current_time - expiry_delta_seconds - 100; // Order created long enough ago that it's expired

    // Create a non-final order
    let order = create_test_order(
        payment_hash,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );
    store.insert_cch_order(order).unwrap();

    // Schedule expiry job
    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    // Wait a bit for message processing
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify order was expired
    let order = store.get_cch_order(&payment_hash).unwrap();
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert_eq!(order.failure_reason, Some("Order expired".to_string()));
}

#[tokio::test]
async fn test_expiry_skips_final_orders() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash = test_payment_hash(2);
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 3600;
    let created_at = current_time - expiry_delta_seconds - 100;

    // Create a final order (already succeeded)
    let order = create_test_order(
        payment_hash,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Succeeded,
    );
    store.insert_cch_order(order.clone()).unwrap();

    // Schedule expiry job
    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify order status unchanged (still Succeeded)
    let order_after = store.get_cch_order(&payment_hash).unwrap();
    assert_eq!(order_after.status, CchOrderStatus::Succeeded);
}

#[tokio::test]
async fn test_schedule_prune_job() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash = test_payment_hash(3);
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 3600;
    let created_at = current_time - expiry_delta_seconds - PRUNE_DELAY_SECONDS - 100;

    // Create a final order
    let order = create_test_order(
        payment_hash,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Succeeded,
    );
    store.insert_cch_order(order).unwrap();

    // Schedule prune job
    scheduler
        .send_message(SchedulerMessage::SchedulePrune {
            payment_hash,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let order_result_after = store.get_cch_order(&payment_hash);
    assert!(order_result_after.is_err());
    assert!(order_result_after.unwrap_err() == CchStoreError::NotFound(payment_hash));
}

#[tokio::test]
async fn test_prune_skips_non_final_orders() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash = test_payment_hash(4);
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 3600;

    // Create a non-final order
    let order = create_test_order(
        payment_hash,
        current_time - expiry_delta_seconds - PRUNE_DELAY_SECONDS - 100,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );
    store.insert_cch_order(order.clone()).unwrap();

    // Schedule prune job
    scheduler
        .send_message(SchedulerMessage::SchedulePrune {
            payment_hash,
            created_at: current_time,
            expiry_delta_seconds,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify order still exists (not pruned because not final)
    let order_after = store.get_cch_order(&payment_hash).unwrap();
    assert_eq!(order_after.status, CchOrderStatus::Pending);
}

#[tokio::test]
async fn test_process_next_due_jobs() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash1 = test_payment_hash(6);
    let payment_hash2 = test_payment_hash(7);

    // Use a recent created_at time so that prune time is in the future
    // But expiry time should be in the past
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 500;
    let created_at = current_time - expiry_delta_seconds - 100; // Order created long enough ago that it's expired

    // Create orders
    let order1 = create_test_order(
        payment_hash1,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );
    let order2 = create_test_order(
        payment_hash2,
        current_time,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );

    store.insert_cch_order(order1).unwrap();
    store.insert_cch_order(order2).unwrap();

    // Schedule both jobs
    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash: payment_hash1,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash: payment_hash2,
            created_at: current_time,
            expiry_delta_seconds,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Process jobs - both should be expired
    scheduler
        .send_message(SchedulerMessage::ProcessJobs)
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both orders should be expired
    let order1 = store.get_cch_order(&payment_hash1).unwrap();
    let order2 = store.get_cch_order(&payment_hash2).unwrap();
    assert_eq!(order1.status, CchOrderStatus::Failed);
    assert_eq!(order2.status, CchOrderStatus::Pending);
}

#[tokio::test]
async fn test_process_multiple_due_jobs() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash1 = test_payment_hash(8);
    let payment_hash2 = test_payment_hash(9);

    // Use a recent created_at time so that prune time is in the future
    // But expiry time should be in the past
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 500;
    let created_at = current_time - expiry_delta_seconds - 100; // Order created long enough ago that it's expired

    // Create orders
    let order1 = create_test_order(
        payment_hash1,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );
    let order2 = create_test_order(
        payment_hash2,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );

    store.insert_cch_order(order1).unwrap();
    store.insert_cch_order(order2).unwrap();

    // Schedule both jobs
    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash: payment_hash1,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash: payment_hash2,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Process jobs - both should be expired
    scheduler
        .send_message(SchedulerMessage::ProcessJobs)
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both orders should be expired
    let order1 = store.get_cch_order(&payment_hash1).unwrap();
    let order2 = store.get_cch_order(&payment_hash2).unwrap();
    assert_eq!(order1.status, CchOrderStatus::Failed);
    assert_eq!(order2.status, CchOrderStatus::Failed);
}

#[tokio::test]
async fn test_expiry_schedules_prune_job() {
    let (scheduler, store, _) = setup_scheduler().await;

    let payment_hash = test_payment_hash(11);
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry_delta_seconds = 3600;
    let created_at = current_time - expiry_delta_seconds - PRUNE_DELAY_SECONDS - 100;

    // Create a non-final order
    let order = create_test_order(
        payment_hash,
        created_at,
        expiry_delta_seconds,
        CchOrderStatus::Pending,
    );
    store.insert_cch_order(order).unwrap();

    // Schedule expiry job
    scheduler
        .send_message(SchedulerMessage::ScheduleExpiry {
            payment_hash,
            created_at,
            expiry_delta_seconds,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let order_result_after = store.get_cch_order(&payment_hash);
    assert!(order_result_after.is_err());
    assert!(order_result_after.unwrap_err() == CchStoreError::NotFound(payment_hash));
}
