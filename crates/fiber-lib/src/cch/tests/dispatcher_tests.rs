//! Unit tests for backend dispatchers

use crate::cch::actions::backend_dispatchers::{
    dispatch_invoice_handler, dispatch_payment_handler, InvoiceHandlerType, PaymentHandlerType,
};
use crate::cch::actions::send_outgoing_payment::SendOutgoingPaymentDispatcher;
use crate::cch::actions::settle_incoming_invoice::SettleIncomingInvoiceDispatcher;
use crate::cch::actions::track_incoming_invoice::TrackIncomingInvoiceDispatcher;
use crate::cch::order::{CchInvoice, CchOrder, CchOrderStatus};
use crate::fiber::types::Hash256;

/// Helper to create a test payment hash
fn test_payment_hash(value: u8) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    Hash256::from(bytes)
}

/// Create a test Lightning invoice by parsing a valid invoice string
fn create_test_lightning_invoice() -> lightning_invoice::Bolt11Invoice {
    // A valid mainnet Lightning invoice for testing
    let invoice_str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap9us6v52vjjsrvywa6rt52cm9r9zqt8r2t7mlcwspyetp5h2tztugp9lfyql";
    invoice_str.parse().unwrap()
}

/// Create a test order with a Lightning incoming invoice
fn create_order_with_lightning_invoice(status: CchOrderStatus) -> CchOrder {
    let invoice = create_test_lightning_invoice();

    CchOrder {
        created_at: 1000,
        expires_after: 3600,
        ckb_final_tlc_expiry_delta: 86400000,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script {
            code_hash: Default::default(),
            hash_type: ckb_jsonrpc_types::ScriptHashType::Data,
            args: Default::default(),
        },
        outgoing_pay_req: "fibb1280...".to_string(),
        incoming_invoice: CchInvoice::Lightning(invoice),
        payment_hash: test_payment_hash(1),
        payment_preimage: None,
        amount_sats: 100000,
        fee_sats: 100,
        status,
        failure_reason: None,
    }
}

/// Create a test order with a Fiber incoming invoice.
/// Since we can't easily create a real CkbInvoice without signing,
/// we use a different approach by parsing from a string representation.
fn create_order_with_fiber_invoice(status: CchOrderStatus) -> CchOrder {
    use crate::invoice::{Attribute, CkbInvoice, Currency, InvoiceData};
    use crate::time::{Duration, SystemTime, UNIX_EPOCH};
    use secp256k1::{Secp256k1, SecretKey};

    // Create a deterministic keypair for tests
    let private_key = SecretKey::from_slice(&[42u8; 32]).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), &private_key);

    let payment_hash = test_payment_hash(1);

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

    CchOrder {
        created_at: 1000,
        expires_after: 3600,
        ckb_final_tlc_expiry_delta: 86400000,
        wrapped_btc_type_script: ckb_jsonrpc_types::Script {
            code_hash: Default::default(),
            hash_type: ckb_jsonrpc_types::ScriptHashType::Data,
            args: Default::default(),
        },
        outgoing_pay_req: "lnbc1...".to_string(),
        incoming_invoice: CchInvoice::Fiber(invoice),
        payment_hash,
        payment_preimage: None,
        amount_sats: 100000,
        fee_sats: 100,
        status,
        failure_reason: None,
    }
}

// =============================================================================
// dispatch_invoice_handler tests
// =============================================================================

#[test]
fn test_dispatch_invoice_handler_returns_fiber_for_fiber_invoice() {
    let order = create_order_with_fiber_invoice(CchOrderStatus::Pending);
    let handler = dispatch_invoice_handler(&order);
    assert_eq!(handler, InvoiceHandlerType::Fiber);
}

#[test]
fn test_dispatch_invoice_handler_returns_lightning_for_lightning_invoice() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Pending);
    let handler = dispatch_invoice_handler(&order);
    assert_eq!(handler, InvoiceHandlerType::Lightning);
}

// =============================================================================
// dispatch_payment_handler tests
// =============================================================================

#[test]
fn test_dispatch_payment_handler_returns_lightning_for_fiber_invoice() {
    // When incoming is Fiber, outgoing payment goes to Lightning
    let order = create_order_with_fiber_invoice(CchOrderStatus::Pending);
    let handler = dispatch_payment_handler(&order);
    assert_eq!(handler, PaymentHandlerType::Lightning);
}

#[test]
fn test_dispatch_payment_handler_returns_fiber_for_lightning_invoice() {
    // When incoming is Lightning, outgoing payment goes to Fiber
    let order = create_order_with_lightning_invoice(CchOrderStatus::Pending);
    let handler = dispatch_payment_handler(&order);
    assert_eq!(handler, PaymentHandlerType::Fiber);
}

// =============================================================================
// SendOutgoingPaymentDispatcher::should_dispatch tests
// =============================================================================

#[test]
fn test_send_outgoing_payment_should_dispatch_when_incoming_accepted() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::IncomingAccepted);
    assert!(SendOutgoingPaymentDispatcher::should_dispatch(&order));
}

#[test]
fn test_send_outgoing_payment_should_not_dispatch_when_pending() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Pending);
    assert!(!SendOutgoingPaymentDispatcher::should_dispatch(&order));
}

#[test]
fn test_send_outgoing_payment_should_not_dispatch_when_outgoing_in_flight() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::OutgoingInFlight);
    assert!(!SendOutgoingPaymentDispatcher::should_dispatch(&order));
}

#[test]
fn test_send_outgoing_payment_should_not_dispatch_when_succeeded() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Succeeded);
    assert!(!SendOutgoingPaymentDispatcher::should_dispatch(&order));
}

#[test]
fn test_send_outgoing_payment_should_not_dispatch_when_failed() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Failed);
    assert!(!SendOutgoingPaymentDispatcher::should_dispatch(&order));
}

// =============================================================================
// SettleIncomingInvoiceDispatcher::should_dispatch tests
// =============================================================================

#[test]
fn test_settle_incoming_invoice_should_dispatch_when_outgoing_succeeded() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::OutgoingSucceeded);
    assert!(SettleIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_settle_incoming_invoice_should_not_dispatch_when_pending() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Pending);
    assert!(!SettleIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_settle_incoming_invoice_should_not_dispatch_when_outgoing_in_flight() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::OutgoingInFlight);
    assert!(!SettleIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_settle_incoming_invoice_should_not_dispatch_when_succeeded() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Succeeded);
    assert!(!SettleIncomingInvoiceDispatcher::should_dispatch(&order));
}

// =============================================================================
// TrackIncomingInvoiceDispatcher::should_dispatch tests
// =============================================================================

#[test]
fn test_track_incoming_invoice_should_dispatch_when_pending() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Pending);
    assert!(TrackIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_track_incoming_invoice_should_dispatch_when_incoming_accepted() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::IncomingAccepted);
    assert!(TrackIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_track_incoming_invoice_should_dispatch_when_outgoing_in_flight() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::OutgoingInFlight);
    assert!(TrackIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_track_incoming_invoice_should_dispatch_when_outgoing_succeeded() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::OutgoingSucceeded);
    assert!(TrackIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_track_incoming_invoice_should_not_dispatch_when_succeeded() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Succeeded);
    assert!(!TrackIncomingInvoiceDispatcher::should_dispatch(&order));
}

#[test]
fn test_track_incoming_invoice_should_not_dispatch_when_failed() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Failed);
    assert!(!TrackIncomingInvoiceDispatcher::should_dispatch(&order));
}

// =============================================================================
// Handler type consistency tests
// =============================================================================

#[test]
fn test_invoice_and_payment_handlers_are_inverse_for_fiber() {
    let order = create_order_with_fiber_invoice(CchOrderStatus::Pending);
    let invoice_handler = dispatch_invoice_handler(&order);
    let payment_handler = dispatch_payment_handler(&order);

    assert_eq!(invoice_handler, InvoiceHandlerType::Fiber);
    assert_eq!(payment_handler, PaymentHandlerType::Lightning);
}

#[test]
fn test_invoice_and_payment_handlers_are_inverse_for_lightning() {
    let order = create_order_with_lightning_invoice(CchOrderStatus::Pending);
    let invoice_handler = dispatch_invoice_handler(&order);
    let payment_handler = dispatch_payment_handler(&order);

    assert_eq!(invoice_handler, InvoiceHandlerType::Lightning);
    assert_eq!(payment_handler, PaymentHandlerType::Fiber);
}
