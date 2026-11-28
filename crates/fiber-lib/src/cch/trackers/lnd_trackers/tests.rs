use super::*;

const PAYMENT_HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const ZERO_PREIMAGE: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn lnd_payment(status: lnrpc::payment::PaymentStatus, payment_preimage: &str) -> lnrpc::Payment {
    lnrpc::Payment {
        payment_hash: PAYMENT_HASH.to_string(),
        payment_preimage: payment_preimage.to_string(),
        status: status as i32,
        ..Default::default()
    }
}

#[test]
fn test_lnd_payment_mapper_accepts_successful_zero_preimage() {
    let event = map_lnd_payment_changed_event(lnd_payment(
        lnrpc::payment::PaymentStatus::Succeeded,
        ZERO_PREIMAGE,
    ))
    .expect("payment event should map");

    match event {
        CchTrackingEvent::PaymentChanged {
            payment_preimage,
            status,
            ..
        } => {
            assert_eq!(status, FiberPaymentStatus::Success);
            assert_eq!(
                payment_preimage,
                Some(Hash256::from_str(ZERO_PREIMAGE).expect("zero preimage should parse"))
            );
        }
        CchTrackingEvent::InvoiceChanged { .. } => panic!("expected payment event"),
    }
}

#[test]
fn test_lnd_payment_mapper_keeps_zero_placeholder_empty_before_success() {
    let event = map_lnd_payment_changed_event(lnd_payment(
        lnrpc::payment::PaymentStatus::InFlight,
        ZERO_PREIMAGE,
    ))
    .expect("payment event should map");

    match event {
        CchTrackingEvent::PaymentChanged {
            payment_preimage,
            status,
            ..
        } => {
            assert_eq!(status, FiberPaymentStatus::Inflight);
            assert_eq!(payment_preimage, None);
        }
        CchTrackingEvent::InvoiceChanged { .. } => panic!("expected payment event"),
    }
}

#[test]
fn test_track_payment_request_targets_one_hash_and_streams_inflight_updates() {
    let payment_hash = Hash256::from_str(PAYMENT_HASH).expect("payment hash should parse");
    let request = track_payment_request(payment_hash);

    assert_eq!(request.payment_hash, payment_hash.as_ref());
    assert!(!request.no_inflight_updates);
}

#[tokio::test]
async fn test_payment_tracker_stops_on_current_terminal_state() {
    let tracker = PaymentTracker {
        port: Arc::new(OutputPort::default()),
        payment_hash: Hash256::from_str(PAYMENT_HASH).expect("payment hash should parse"),
        lnd_connection: LndConnectionInfo {
            uri: "https://localhost:10009".parse().unwrap(),
            cert: None,
            macaroon: None,
        },
        token: CancellationToken::new(),
    };

    assert!(tracker
        .on_payment(lnd_payment(
            lnrpc::payment::PaymentStatus::Succeeded,
            ZERO_PREIMAGE,
        ))
        .await
        .expect("successful payment should map"));
    assert!(tracker
        .on_payment(lnd_payment(
            lnrpc::payment::PaymentStatus::Failed,
            ZERO_PREIMAGE,
        ))
        .await
        .expect("failed payment should map"));
    assert!(!tracker
        .on_payment(lnd_payment(
            lnrpc::payment::PaymentStatus::InFlight,
            ZERO_PREIMAGE,
        ))
        .await
        .expect("in-flight payment should map"));
}
