use std::sync::Arc;

use crate::cch::trackers::{
    InvoiceTrackingReservationResult, LndConnectionInfo, LndTrackerActor, LndTrackerArgs,
    LndTrackerMessage, MAX_TRACKED_INVOICES,
};
use fiber_types::Hash256;
use ractor::{concurrency::Duration as RactorDuration, Actor, ActorRef, OutputPort};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

// Helper function to create test arguments
fn create_test_args() -> LndTrackerArgs {
    let port = Arc::new(OutputPort::default());
    let tracker = TaskTracker::new();
    let token = CancellationToken::new();
    let lnd_connection = LndConnectionInfo::new(
        // Tracker will keep running because this URI is unreachable
        "https://localhost:10009".parse().unwrap(),
        None,
        None,
    );

    LndTrackerArgs {
        port,
        lnd_connection,
        token,
        tracker,
    }
}

// Helper function to create a test payment hash
fn test_payment_hash(value: u8) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    Hash256::from(bytes)
}

// Helper function to create a test `LndTrackerActor` (without spawning trackers)
async fn create_test_actor() -> (ActorRef<LndTrackerMessage>, tokio::task::JoinHandle<()>) {
    // Use spawn instead of spawn_linked to avoid needing a root actor
    let args = create_test_args();
    let (actor_ref, actor_handle) = Actor::spawn(None, LndTrackerActor, args)
        .await
        .expect("Failed to spawn test actor");

    (actor_ref, actor_handle)
}

async fn reserve_invoice_tracking(
    actor_ref: &ActorRef<LndTrackerMessage>,
    payment_hash: Hash256,
) -> InvoiceTrackingReservationResult {
    ractor::call!(actor_ref, |reply| {
        LndTrackerMessage::ReserveInvoiceTracking(payment_hash, reply)
    })
    .expect("Failed to reserve invoice tracking")
}

struct TestStateSnapshot {
    invoice_queue_len: usize,
    active_invoice_trackers: usize,
    reserved_invoice_trackers: usize,
    stopping_invoice_trackers: usize,
    tracked_invoices: usize,
}

async fn get_state(actor_ref: &ActorRef<LndTrackerMessage>) -> TestStateSnapshot {
    let state = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Failed to get state")
        .expect("Failed to get state");
    TestStateSnapshot {
        invoice_queue_len: state.invoice_queue_len,
        active_invoice_trackers: state.active_invoice_trackers,
        reserved_invoice_trackers: state.reserved_invoice_trackers,
        stopping_invoice_trackers: state.stopping_invoice_trackers,
        tracked_invoices: state.tracked_invoices,
    }
}

#[tokio::test]
async fn test_invoice_tracking_reservation_enforces_global_limit() {
    let (actor_ref, _handle) = create_test_actor().await;

    for value in 0..MAX_TRACKED_INVOICES {
        assert_eq!(
            reserve_invoice_tracking(&actor_ref, test_payment_hash(value as u8)).await,
            InvoiceTrackingReservationResult::Reserved
        );
    }
    assert_eq!(
        reserve_invoice_tracking(&actor_ref, test_payment_hash(MAX_TRACKED_INVOICES as u8)).await,
        InvoiceTrackingReservationResult::CapacityExceeded
    );

    let state = get_state(&actor_ref).await;
    assert_eq!(state.reserved_invoice_trackers, MAX_TRACKED_INVOICES);
    assert_eq!(state.tracked_invoices, MAX_TRACKED_INVOICES);
    assert_eq!(state.invoice_queue_len, 0);
    assert_eq!(state.active_invoice_trackers, 0);
}

#[tokio::test]
async fn test_duplicate_invoice_tracking_reservation_is_coalesced() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(1);

    assert_eq!(
        reserve_invoice_tracking(&actor_ref, payment_hash).await,
        InvoiceTrackingReservationResult::Reserved
    );
    assert_eq!(
        reserve_invoice_tracking(&actor_ref, payment_hash).await,
        InvoiceTrackingReservationResult::AlreadyTracked
    );

    let state = get_state(&actor_ref).await;
    assert_eq!(state.reserved_invoice_trackers, 1);
    assert_eq!(state.tracked_invoices, 1);
}

#[tokio::test]
async fn test_stopping_reservation_releases_global_capacity() {
    let (actor_ref, _handle) = create_test_actor().await;

    for value in 0..MAX_TRACKED_INVOICES {
        assert_eq!(
            reserve_invoice_tracking(&actor_ref, test_payment_hash(value as u8)).await,
            InvoiceTrackingReservationResult::Reserved
        );
    }

    actor_ref
        .cast(LndTrackerMessage::StopTracking(test_payment_hash(0)))
        .expect("Failed to send StopTracking");
    assert_eq!(
        reserve_invoice_tracking(&actor_ref, test_payment_hash(MAX_TRACKED_INVOICES as u8)).await,
        InvoiceTrackingReservationResult::Reserved
    );

    let state = get_state(&actor_ref).await;
    assert_eq!(state.reserved_invoice_trackers, MAX_TRACKED_INVOICES);
    assert_eq!(state.tracked_invoices, MAX_TRACKED_INVOICES);
}

#[tokio::test]
async fn test_tracking_commits_invoice_tracking_reservation() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(1);

    assert_eq!(
        reserve_invoice_tracking(&actor_ref, payment_hash).await,
        InvoiceTrackingReservationResult::Reserved
    );
    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(payment_hash))
        .expect("Failed to send TrackInvoice");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state = get_state(&actor_ref).await;
    assert_eq!(state.reserved_invoice_trackers, 0);
    assert_eq!(state.tracked_invoices, 1);
    assert_eq!(state.invoice_queue_len, 0);
    assert_eq!(state.active_invoice_trackers, 1);
}

#[tokio::test]
async fn test_stopping_active_tracker_releases_capacity_after_task_exit() {
    let (actor_ref, _handle) = create_test_actor().await;
    let active_payment_hash = test_payment_hash(0);

    assert_eq!(
        reserve_invoice_tracking(&actor_ref, active_payment_hash).await,
        InvoiceTrackingReservationResult::Reserved
    );
    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(active_payment_hash))
        .expect("Failed to send TrackInvoice");
    for value in 1..MAX_TRACKED_INVOICES {
        assert_eq!(
            reserve_invoice_tracking(&actor_ref, test_payment_hash(value as u8)).await,
            InvoiceTrackingReservationResult::Reserved
        );
    }

    actor_ref
        .cast(LndTrackerMessage::StopTracking(active_payment_hash))
        .expect("Failed to send StopTracking");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state = get_state(&actor_ref).await;
    assert_eq!(state.active_invoice_trackers, 0);
    assert_eq!(state.stopping_invoice_trackers, 0);
    assert_eq!(state.reserved_invoice_trackers, MAX_TRACKED_INVOICES - 1);
    assert_eq!(state.tracked_invoices, MAX_TRACKED_INVOICES - 1);

    assert_eq!(
        reserve_invoice_tracking(&actor_ref, test_payment_hash(MAX_TRACKED_INVOICES as u8)).await,
        InvoiceTrackingReservationResult::Reserved
    );
}

#[tokio::test]
async fn test_retracking_before_stopped_tracker_completes_does_not_duplicate_tracker() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(1);

    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(payment_hash))
        .expect("Failed to send TrackInvoice");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    actor_ref
        .cast(LndTrackerMessage::StopTracking(payment_hash))
        .expect("Failed to send StopTracking");
    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(payment_hash))
        .expect("Failed to send TrackInvoice");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state = get_state(&actor_ref).await;
    assert_eq!(state.reserved_invoice_trackers, 0);
    assert_eq!(state.tracked_invoices, 1);
    assert_eq!(state.invoice_queue_len, 0);
    assert_eq!(state.active_invoice_trackers, 1);
}

// Test completion decrements active_invoice_trackers counter
#[tokio::test]
async fn test_completion_decrements_counter() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(1);

    // Add invoice to queue (without processing to avoid LND calls)
    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(payment_hash))
        .expect("Failed to send TrackInvoice");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Send completion message (simulating a tracker that finished)
    actor_ref
        .cast(LndTrackerMessage::InvoiceTrackerCompleted {
            payment_hash,
            completed_successfully: true,
        })
        .expect("Failed to send completion");

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Verify counter behavior (should handle completion gracefully)
    let final_state = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Actor should be responsive after completion");

    assert!(final_state.is_success());
    let final_state = final_state.unwrap();
    assert_eq!(final_state.invoice_queue_len, 0);
    assert_eq!(final_state.active_invoice_trackers, 0);
}

// All globally admitted invoices start immediately instead of waiting behind five long-lived slots.
#[tokio::test]
async fn test_all_admitted_invoices_start_tracking() {
    let (actor_ref, _handle) = create_test_actor().await;

    for i in 0..MAX_TRACKED_INVOICES {
        let payment_hash = test_payment_hash(i as u8);
        actor_ref
            .cast(LndTrackerMessage::TrackInvoice(payment_hash))
            .expect("Failed to send TrackInvoice");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Failed to get state")
        .expect("Failed to get state");

    assert_eq!(state.invoice_queue_len, 0);
    assert_eq!(state.active_invoice_trackers, MAX_TRACKED_INVOICES);
}

// Persisted orders from before the admission limit may exceed it. Restore them without opening
// more than MAX_TRACKED_INVOICES concurrent subscriptions.
#[tokio::test]
async fn test_restored_invoices_above_global_limit_are_queued() {
    let (actor_ref, _handle) = create_test_actor().await;

    for i in 0..=MAX_TRACKED_INVOICES {
        actor_ref
            .cast(LndTrackerMessage::TrackInvoice(test_payment_hash(i as u8)))
            .expect("Failed to send TrackInvoice");
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state_before = get_state(&actor_ref).await;
    assert_eq!(state_before.invoice_queue_len, 1);
    assert_eq!(state_before.active_invoice_trackers, MAX_TRACKED_INVOICES);

    let completed_hash = test_payment_hash(1);
    actor_ref
        .cast(LndTrackerMessage::InvoiceTrackerCompleted {
            payment_hash: completed_hash,
            completed_successfully: true,
        })
        .expect("Failed to send completion");

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state_after = get_state(&actor_ref).await;
    assert_eq!(state_after.invoice_queue_len, 0);
    assert_eq!(state_after.active_invoice_trackers, MAX_TRACKED_INVOICES);
}

// An unexpected tracker exit restarts without releasing the invoice's admission.
#[tokio::test]
async fn test_failed_tracker_restarts() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(1);

    // Add invoice to queue (without processing to avoid LND calls)
    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(payment_hash))
        .expect("Failed to send TrackInvoice");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Send completion message (simulating a tracker that finished)
    actor_ref
        .cast(LndTrackerMessage::InvoiceTrackerCompleted {
            payment_hash,
            completed_successfully: false,
        })
        .expect("Failed to send completion");

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Verify counter behavior (should handle completion gracefully)
    let final_state = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Actor should be responsive after completion");

    assert!(final_state.is_success());
    let final_state = final_state.unwrap();
    assert_eq!(final_state.invoice_queue_len, 0);
    assert_eq!(final_state.active_invoice_trackers, 1);
}

#[tokio::test]
async fn test_duplicate_tracking_requests_are_coalesced() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(2);

    for _ in 0..6 {
        actor_ref
            .cast(LndTrackerMessage::TrackInvoice(payment_hash))
            .expect("Failed to send TrackInvoice");
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Failed to get state")
        .expect("Failed to get state");

    assert_eq!(state.invoice_queue_len, 0);
    assert_eq!(state.active_invoice_trackers, 1);
}

#[tokio::test]
async fn test_stopped_tracker_is_not_requeued_after_failure() {
    let (actor_ref, _handle) = create_test_actor().await;
    let payment_hash = test_payment_hash(3);

    actor_ref
        .cast(LndTrackerMessage::TrackInvoice(payment_hash))
        .expect("Failed to send TrackInvoice");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    actor_ref
        .cast(LndTrackerMessage::StopTracking(payment_hash))
        .expect("Failed to send StopTracking");
    actor_ref
        .cast(LndTrackerMessage::InvoiceTrackerCompleted {
            payment_hash,
            completed_successfully: false,
        })
        .expect("Failed to send completion");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let state = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Failed to get state")
        .expect("Failed to get state");

    assert_eq!(state.invoice_queue_len, 0);
    assert_eq!(state.active_invoice_trackers, 0);
}
