//! Unit tests for CchOrderStateMachine
//!
//! Tests cover:
//! - Valid state transitions
//! - Invalid state transitions
//! - on_entering actions for each status
//! - Failure transitions from any state

use crate::cch::order::{
    state_machine::CchOrderEvent, CchInvoice, CchOrder, CchOrderStateMachine, CchOrderStatus,
};
use crate::cch::CchError;
use crate::fiber::payment::PaymentStatus;
use crate::fiber::types::Hash256;
use crate::invoice::CkbInvoiceStatus;

/// Helper function to create a test payment hash
fn test_payment_hash(value: u8) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    Hash256::from(bytes)
}

/// Helper function to create a test CchOrder with configurable status
fn create_test_order(status: CchOrderStatus) -> CchOrder {
    // Create a minimal valid Lightning invoice string for testing
    // This is a mainnet invoice format that parses correctly
    let btc_invoice_str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap9us6v52vjjsrvywa6rt52cm9r9zqt8r2t7mlcwspyetp5h2tztugp9lfyql";
    let btc_invoice: lightning_invoice::Bolt11Invoice = btc_invoice_str.parse().unwrap();

    CchOrder {
        created_at: 1700000000,
        expiry_delta_seconds: 3600,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script {
            code_hash: Default::default(),
            hash_type: ckb_jsonrpc_types::ScriptHashType::Data,
            args: Default::default(),
        },
        outgoing_pay_req: btc_invoice_str.to_string(),
        incoming_invoice: CchInvoice::Lightning(btc_invoice),
        payment_hash: test_payment_hash(1),
        payment_preimage: None,
        amount_sats: 100000,
        fee_sats: 100,
        status,
        failure_reason: None,
    }
}

// ============================================================================
// Tests for valid state transitions via invoice events
// ============================================================================

#[test]
fn test_transition_pending_to_incoming_accepted_via_invoice_received() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Received,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::IncomingAccepted);
}

#[test]
fn test_transition_pending_to_failed_via_invoice_cancelled() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Cancelled,
        failure_reason: Some("cancelled by user".to_string()),
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert_eq!(order.failure_reason, Some("cancelled by user".to_string()));
}

#[test]
fn test_transition_pending_to_failed_via_invoice_expired() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Expired,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert!(order.failure_reason.is_some());
}

#[test]
fn test_transition_outgoing_succeeded_to_succeeded_via_invoice_paid() {
    let mut order = create_test_order(CchOrderStatus::OutgoingSucceeded);
    order.payment_preimage = Some(test_payment_hash(99));

    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Paid,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Succeeded);
}

// ============================================================================
// Tests for valid state transitions via payment events
// ============================================================================

#[test]
fn test_transition_incoming_accepted_to_outgoing_in_flight_via_payment_inflight() {
    let mut order = create_test_order(CchOrderStatus::IncomingAccepted);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Inflight,
        payment_preimage: None,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
}

#[test]
fn test_transition_incoming_accepted_to_outgoing_succeeded_via_payment_success() {
    let mut order = create_test_order(CchOrderStatus::IncomingAccepted);
    let preimage = test_payment_hash(42);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Success,
        payment_preimage: Some(preimage),
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::OutgoingSucceeded);
    assert_eq!(order.payment_preimage, Some(preimage));
}

#[test]
fn test_transition_outgoing_in_flight_to_outgoing_succeeded_via_payment_success() {
    let mut order = create_test_order(CchOrderStatus::OutgoingInFlight);
    let preimage = test_payment_hash(42);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Success,
        payment_preimage: Some(preimage),
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::OutgoingSucceeded);
    assert_eq!(order.payment_preimage, Some(preimage));
}

#[test]
fn test_transition_incoming_accepted_to_failed_via_payment_failed() {
    let mut order = create_test_order(CchOrderStatus::IncomingAccepted);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Failed,
        payment_preimage: None,
        failure_reason: Some("no route found".to_string()),
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert_eq!(order.failure_reason, Some("no route found".to_string()));
}

#[test]
fn test_transition_outgoing_in_flight_to_failed_via_payment_failed() {
    let mut order = create_test_order(CchOrderStatus::OutgoingInFlight);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Failed,
        payment_preimage: None,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
    assert!(order.failure_reason.is_some());
}

// ============================================================================
// Tests for staying in same status (no-op transitions)
// ============================================================================

#[test]
fn test_staying_in_pending_via_invoice_open() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Open,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_none());
    assert_eq!(order.status, CchOrderStatus::Pending);
}

#[test]
fn test_staying_in_outgoing_in_flight_via_payment_inflight() {
    let mut order = create_test_order(CchOrderStatus::OutgoingInFlight);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Inflight,
        payment_preimage: None,
        failure_reason: None,
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_none());
    assert_eq!(order.status, CchOrderStatus::OutgoingInFlight);
}

// ============================================================================
// Tests for invalid state transitions
// ============================================================================

#[test]
fn test_invalid_transition_pending_to_outgoing_in_flight() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Inflight,
        payment_preimage: None,
        failure_reason: None,
    };

    let result = CchOrderStateMachine::apply(&mut order, event);

    assert!(matches!(result, Err(CchError::InvalidTransition(_, _))));
}

#[test]
fn test_invalid_transition_pending_to_outgoing_succeeded() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let preimage = test_payment_hash(42);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Success,
        payment_preimage: Some(preimage),
        failure_reason: None,
    };

    let result = CchOrderStateMachine::apply(&mut order, event);

    assert!(matches!(result, Err(CchError::InvalidTransition(_, _))));
}

#[test]
fn test_invalid_transition_pending_to_succeeded() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Paid,
        failure_reason: None,
    };

    let result = CchOrderStateMachine::apply(&mut order, event);

    assert!(matches!(result, Err(CchError::InvalidTransition(_, _))));
}

#[test]
fn test_invalid_transition_succeeded_to_any_other() {
    let mut order = create_test_order(CchOrderStatus::Succeeded);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Open,
        failure_reason: None,
    };

    let result = CchOrderStateMachine::apply(&mut order, event);

    assert!(matches!(result, Err(CchError::InvalidTransition(_, _))));
}

#[test]
fn test_invalid_transition_failed_to_any_other() {
    let mut order = create_test_order(CchOrderStatus::Failed);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Open,
        failure_reason: None,
    };

    let result = CchOrderStateMachine::apply(&mut order, event);

    assert!(matches!(result, Err(CchError::InvalidTransition(_, _))));
}

// ============================================================================
// Tests for error conditions
// ============================================================================

#[test]
fn test_payment_success_without_preimage_returns_error() {
    let mut order = create_test_order(CchOrderStatus::IncomingAccepted);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Success,
        payment_preimage: None, // Missing preimage!
        failure_reason: None,
    };

    let result = CchOrderStateMachine::apply(&mut order, event);

    assert!(matches!(
        result,
        Err(CchError::SettledPaymentMissingPreimage)
    ));
}

// ============================================================================
// Tests for failure transitions from any state
// ============================================================================

#[test]
fn test_failure_from_pending() {
    let mut order = create_test_order(CchOrderStatus::Pending);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Failed,
        payment_preimage: None,
        failure_reason: Some("test failure".to_string()),
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
}

#[test]
fn test_failure_from_incoming_accepted() {
    let mut order = create_test_order(CchOrderStatus::IncomingAccepted);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Cancelled,
        failure_reason: Some("test failure".to_string()),
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
}

#[test]
fn test_failure_from_outgoing_in_flight() {
    let mut order = create_test_order(CchOrderStatus::OutgoingInFlight);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Failed,
        payment_preimage: None,
        failure_reason: Some("test failure".to_string()),
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
}

#[test]
fn test_failure_from_outgoing_succeeded() {
    let mut order = create_test_order(CchOrderStatus::OutgoingSucceeded);
    let event = CchOrderEvent::IncomingInvoiceChanged {
        status: CkbInvoiceStatus::Cancelled,
        failure_reason: Some("test failure".to_string()),
    };

    let transition = CchOrderStateMachine::apply(&mut order, event).unwrap();

    assert!(transition.is_some());
    assert_eq!(order.status, CchOrderStatus::Failed);
}

// ============================================================================
// Tests for preimage preservation
// ============================================================================

#[test]
fn test_preimage_not_overwritten_if_already_set() {
    let mut order = create_test_order(CchOrderStatus::OutgoingInFlight);
    let original_preimage = test_payment_hash(1);
    order.payment_preimage = Some(original_preimage);

    let new_preimage = test_payment_hash(2);
    let event = CchOrderEvent::OutgoingPaymentChanged {
        status: PaymentStatus::Success,
        payment_preimage: Some(new_preimage),
        failure_reason: None,
    };

    CchOrderStateMachine::apply(&mut order, event).unwrap();
    // Should keep the original preimage
    assert_eq!(order.payment_preimage, Some(original_preimage));
}

// ============================================================================
// Tests for is_final
// ============================================================================

#[test]
fn test_is_final_returns_true_for_succeeded() {
    let order = create_test_order(CchOrderStatus::Succeeded);
    assert!(order.is_final());
}

#[test]
fn test_is_final_returns_true_for_failed() {
    let order = create_test_order(CchOrderStatus::Failed);
    assert!(order.is_final());
}

#[test]
fn test_is_final_returns_false_for_pending() {
    let order = create_test_order(CchOrderStatus::Pending);
    assert!(!order.is_final());
}

#[test]
fn test_is_final_returns_false_for_in_progress_statuses() {
    for status in [
        CchOrderStatus::IncomingAccepted,
        CchOrderStatus::OutgoingInFlight,
        CchOrderStatus::OutgoingSucceeded,
    ] {
        let order = create_test_order(status);
        assert!(!order.is_final(), "Expected {:?} to not be final", status);
    }
}
