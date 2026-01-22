//! Unit tests for event conversions
//!
//! Tests cover:
//! - CchTrackingEvent -> CchOrderEvent conversions
//! - CkbInvoiceStatus -> CchOrderStatus mappings
//! - PaymentStatus -> CchOrderStatus mappings

use crate::cch::order::{state_machine::CchOrderEvent, CchOrderStatus};
use crate::cch::trackers::CchTrackingEvent;
use crate::fiber::payment::PaymentStatus;
use crate::fiber::types::Hash256;
use crate::invoice::CkbInvoiceStatus;

/// Helper function to create a test payment hash
fn test_payment_hash(value: u8) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    Hash256::from(bytes)
}

// =============================================================================
// CchTrackingEvent -> CchOrderEvent conversion tests
// =============================================================================

#[test]
fn test_invoice_changed_event_converts_to_incoming_invoice_changed() {
    let payment_hash = test_payment_hash(1);
    let tracking_event = CchTrackingEvent::InvoiceChanged {
        payment_hash,
        status: CkbInvoiceStatus::Received,
        failure_reason: None,
    };

    let order_event: CchOrderEvent = tracking_event.into();

    match order_event {
        CchOrderEvent::IncomingInvoiceChanged {
            status,
            failure_reason,
        } => {
            assert_eq!(status, CkbInvoiceStatus::Received);
            assert!(failure_reason.is_none());
        }
        _ => panic!("Expected IncomingInvoiceChanged event"),
    }
}

#[test]
fn test_invoice_changed_event_preserves_failure_reason() {
    let payment_hash = test_payment_hash(1);
    let tracking_event = CchTrackingEvent::InvoiceChanged {
        payment_hash,
        status: CkbInvoiceStatus::Cancelled,
        failure_reason: Some("cancelled by user".to_string()),
    };

    let order_event: CchOrderEvent = tracking_event.into();

    match order_event {
        CchOrderEvent::IncomingInvoiceChanged {
            status,
            failure_reason,
        } => {
            assert_eq!(status, CkbInvoiceStatus::Cancelled);
            assert_eq!(failure_reason, Some("cancelled by user".to_string()));
        }
        _ => panic!("Expected IncomingInvoiceChanged event"),
    }
}

#[test]
fn test_payment_changed_event_converts_to_outgoing_payment_changed() {
    let payment_hash = test_payment_hash(1);
    let preimage = test_payment_hash(42);
    let tracking_event = CchTrackingEvent::PaymentChanged {
        payment_hash,
        status: PaymentStatus::Success,
        payment_preimage: Some(preimage),
        failure_reason: None,
    };

    let order_event: CchOrderEvent = tracking_event.into();

    match order_event {
        CchOrderEvent::OutgoingPaymentChanged {
            status,
            payment_preimage,
            failure_reason,
        } => {
            assert_eq!(status, PaymentStatus::Success);
            assert_eq!(payment_preimage, Some(preimage));
            assert!(failure_reason.is_none());
        }
        _ => panic!("Expected OutgoingPaymentChanged event"),
    }
}

#[test]
fn test_payment_changed_event_preserves_failure_reason() {
    let payment_hash = test_payment_hash(1);
    let tracking_event = CchTrackingEvent::PaymentChanged {
        payment_hash,
        status: PaymentStatus::Failed,
        payment_preimage: None,
        failure_reason: Some("no route found".to_string()),
    };

    let order_event: CchOrderEvent = tracking_event.into();

    match order_event {
        CchOrderEvent::OutgoingPaymentChanged {
            status,
            payment_preimage,
            failure_reason,
        } => {
            assert_eq!(status, PaymentStatus::Failed);
            assert!(payment_preimage.is_none());
            assert_eq!(failure_reason, Some("no route found".to_string()));
        }
        _ => panic!("Expected OutgoingPaymentChanged event"),
    }
}

#[test]
fn test_tracking_event_payment_hash_accessor() {
    let payment_hash = test_payment_hash(123);

    let invoice_event = CchTrackingEvent::InvoiceChanged {
        payment_hash,
        status: CkbInvoiceStatus::Open,
        failure_reason: None,
    };
    assert_eq!(*invoice_event.payment_hash(), payment_hash);

    let payment_event = CchTrackingEvent::PaymentChanged {
        payment_hash,
        status: PaymentStatus::Created,
        payment_preimage: None,
        failure_reason: None,
    };
    assert_eq!(*payment_event.payment_hash(), payment_hash);
}

// =============================================================================
// CkbInvoiceStatus -> CchOrderStatus conversion tests
// =============================================================================

#[test]
fn test_invoice_status_open_maps_to_pending() {
    let order_status: CchOrderStatus = CkbInvoiceStatus::Open.into();
    assert_eq!(order_status, CchOrderStatus::Pending);
}

#[test]
fn test_invoice_status_received_maps_to_incoming_accepted() {
    let order_status: CchOrderStatus = CkbInvoiceStatus::Received.into();
    assert_eq!(order_status, CchOrderStatus::IncomingAccepted);
}

#[test]
fn test_invoice_status_paid_maps_to_succeeded() {
    let order_status: CchOrderStatus = CkbInvoiceStatus::Paid.into();
    assert_eq!(order_status, CchOrderStatus::Succeeded);
}

#[test]
fn test_invoice_status_cancelled_maps_to_failed() {
    let order_status: CchOrderStatus = CkbInvoiceStatus::Cancelled.into();
    assert_eq!(order_status, CchOrderStatus::Failed);
}

#[test]
fn test_invoice_status_expired_maps_to_failed() {
    let order_status: CchOrderStatus = CkbInvoiceStatus::Expired.into();
    assert_eq!(order_status, CchOrderStatus::Failed);
}

// =============================================================================
// PaymentStatus -> CchOrderStatus conversion tests
// =============================================================================

#[test]
fn test_payment_status_created_maps_to_incoming_accepted() {
    let order_status: CchOrderStatus = PaymentStatus::Created.into();
    assert_eq!(order_status, CchOrderStatus::IncomingAccepted);
}

#[test]
fn test_payment_status_inflight_maps_to_outgoing_in_flight() {
    let order_status: CchOrderStatus = PaymentStatus::Inflight.into();
    assert_eq!(order_status, CchOrderStatus::OutgoingInFlight);
}

#[test]
fn test_payment_status_success_maps_to_outgoing_succeeded() {
    let order_status: CchOrderStatus = PaymentStatus::Success.into();
    assert_eq!(order_status, CchOrderStatus::OutgoingSucceeded);
}

#[test]
fn test_payment_status_failed_maps_to_failed() {
    let order_status: CchOrderStatus = PaymentStatus::Failed.into();
    assert_eq!(order_status, CchOrderStatus::Failed);
}

// =============================================================================
// All invoice statuses covered
// =============================================================================

#[test]
fn test_all_invoice_statuses_have_mappings() {
    // This test ensures all CkbInvoiceStatus variants are covered
    let statuses = [
        CkbInvoiceStatus::Open,
        CkbInvoiceStatus::Cancelled,
        CkbInvoiceStatus::Expired,
        CkbInvoiceStatus::Received,
        CkbInvoiceStatus::Paid,
    ];

    for status in statuses {
        let _: CchOrderStatus = status.into();
        // If this compiles and runs without panic, all variants are handled
    }
}

// =============================================================================
// All payment statuses covered
// =============================================================================

#[test]
fn test_all_payment_statuses_have_mappings() {
    // This test ensures all PaymentStatus variants are covered
    let statuses = [
        PaymentStatus::Created,
        PaymentStatus::Inflight,
        PaymentStatus::Success,
        PaymentStatus::Failed,
    ];

    for status in statuses {
        let _: CchOrderStatus = status.into();
        // If this compiles and runs without panic, all variants are handled
    }
}
