use super::*;

#[test]
fn test_lnd_permanent_payment_error_returns_failed_event() {
    let payment_hash = Hash256::from([42; 32]);
    let error = tonic::Status::unknown("self-payments not allowed");

    let event = SendLightningOutgoingPaymentExecutor::event_for_payment_error(payment_hash, &error)
        .expect("permanent errors should be converted into tracking events");

    match event {
        CchTrackingEvent::PaymentChanged {
            payment_hash: event_payment_hash,
            payment_preimage,
            status,
            failure_reason,
        } => {
            assert_eq!(event_payment_hash, payment_hash);
            assert_eq!(payment_preimage, None);
            assert_eq!(status, PaymentStatus::Failed);
            assert!(failure_reason
                .expect("failed payment should include a reason")
                .contains("self-payments not allowed"));
        }
        CchTrackingEvent::InvoiceChanged { .. } => {
            panic!("payment errors should not produce invoice events")
        }
    }
}

#[test]
fn test_lnd_already_exists_payment_error_returns_inflight_event() {
    let payment_hash = Hash256::from([43; 32]);
    let error = tonic::Status::already_exists("payment is in flight");

    let event = SendLightningOutgoingPaymentExecutor::event_for_payment_error(payment_hash, &error)
        .expect("already-existing payments should be tracked rather than retried");

    match event {
        CchTrackingEvent::PaymentChanged {
            payment_hash: event_payment_hash,
            payment_preimage,
            status,
            failure_reason,
        } => {
            assert_eq!(event_payment_hash, payment_hash);
            assert_eq!(payment_preimage, None);
            assert_eq!(status, PaymentStatus::Inflight);
            assert_eq!(failure_reason, None);
        }
        CchTrackingEvent::InvoiceChanged { .. } => {
            panic!("payment errors should not produce invoice events")
        }
    }
}

#[test]
fn test_lnd_transient_payment_error_remains_retryable() {
    let payment_hash = Hash256::from([44; 32]);
    let error = tonic::Status::unavailable("lnd is unavailable");

    let result =
        SendLightningOutgoingPaymentExecutor::event_for_payment_error(payment_hash, &error);

    let error = result.expect_err("transient errors should be returned for actor retry");
    assert!(error.to_string().contains("lnd is unavailable"));
}
