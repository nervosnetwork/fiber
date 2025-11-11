use std::sync::Arc;

use crate::{
    cch::{LndConnectionInfo, LndTrackerActor, LndTrackerArgs, LndTrackerMessage},
    fiber::types::Hash256,
};

use ractor::{concurrency::Duration as RactorDuration, Actor, ActorRef, OutputPort};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

// Helper function to create test arguments
fn create_test_args() -> LndTrackerArgs {
    let port = Arc::new(OutputPort::default());
    let tracker = TaskTracker::new();
    let token = CancellationToken::new();
    let lnd_connection = LndConnectionInfo {
        // Tracker will keep running because this URI is unreachable
        uri: "https://localhost:10009".parse().unwrap(),
        cert: None,
        macaroon: None,
    };

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

// Test completion triggers queue processing for waiting invoices
#[tokio::test]
async fn test_completion_triggers_queue_processing() {
    let (actor_ref, _handle) = create_test_actor().await;

    // Add 6 invoices to queue
    for i in 0..6 {
        let payment_hash = test_payment_hash(i);
        actor_ref
            .cast(LndTrackerMessage::TrackInvoice(payment_hash))
            .expect("Failed to send TrackInvoice");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Verify invoices are queued
    let state_before = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Failed to get state")
        .expect("Failed to get state");

    assert_eq!(
        state_before.invoice_queue_len, 1,
        "Should have 1 invoice in queue"
    );
    assert_eq!(
        state_before.active_invoice_trackers, 5,
        "Should have 5 active invoice trackers"
    );

    let completed_hash = test_payment_hash(1);
    actor_ref
        .cast(LndTrackerMessage::InvoiceTrackerCompleted {
            payment_hash: completed_hash,
            completed_successfully: true,
        })
        .expect("Failed to send completion");

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Verify actor is still responsive
    let state_after = actor_ref
        .call(
            LndTrackerMessage::GetState,
            Some(RactorDuration::from_millis(1000)),
        )
        .await
        .expect("Failed to get state")
        .expect("Failed to get state");

    assert_eq!(
        state_after.invoice_queue_len, 0,
        "Should have 0 invoices in queue"
    );
    assert_eq!(
        state_after.active_invoice_trackers, 5,
        "Should have 5 active invoice trackers"
    );
}

// Test timeout re-queues active invoices to end of queue
#[tokio::test]
async fn test_timeout_requeues_active_invoices() {
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
