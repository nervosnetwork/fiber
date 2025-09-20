use super::subscription_unit_tests::{
    mock_invoice, mock_payment_session, MockStore, StoreTestSubscriber,
};
use crate::{
    fiber::{graph::NetworkGraphStateStore, payment::PaymentStatus},
    gen_rand_sha256_hash,
    invoice::{CkbInvoiceStatus, InvoiceStore, PreimageStore},
    store::pub_sub::{
        InvoiceUpdatedPayload, PaymentUpdatedPayload, StoreUpdatedEvent, StoreWithPubSub,
    },
};
use ractor::{concurrency::Duration, Actor};

#[tokio::test]
async fn test_insert_invoice() {
    let invoice = mock_invoice(100);
    let invoice_hash = *invoice.payment_hash();
    let expected =
        StoreUpdatedEvent::new_invoice_updated_event(invoice_hash, InvoiceUpdatedPayload::Open);

    let store = StoreWithPubSub::new(MockStore::default());

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store.insert_invoice(invoice, None).unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_update_invoice_status_to_received() {
    let invoice = mock_invoice(100);
    let invoice_hash = *invoice.payment_hash();
    let expected = StoreUpdatedEvent::new_invoice_updated_event(
        invoice_hash,
        InvoiceUpdatedPayload::Received {
            amount: 100,
            is_finished: true,
        },
    );

    let store = StoreWithPubSub::new(MockStore {
        invoice_status: Some(CkbInvoiceStatus::Received),
        invoice: Some(mock_invoice(100)),
        ..Default::default()
    });

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store
        .update_invoice_status(&invoice_hash, CkbInvoiceStatus::Received)
        .unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_insert_payment_session() {
    let payment_hash = gen_rand_sha256_hash();
    let session = mock_payment_session(payment_hash, PaymentStatus::Created);
    let expected =
        StoreUpdatedEvent::new_payment_updated_event(payment_hash, PaymentUpdatedPayload::Created);

    let store = StoreWithPubSub::new(MockStore::default());

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store.insert_payment_session(session);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_insert_payment_session_with_success_status() {
    let payment_hash = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let session = mock_payment_session(payment_hash, PaymentStatus::Success);
    let expected = StoreUpdatedEvent::new_payment_updated_event(
        payment_hash,
        PaymentUpdatedPayload::Success { preimage },
    );

    let store = StoreWithPubSub::new(MockStore {
        payment_session: Some(session.clone()),
        preimage: Some(preimage),
        ..Default::default()
    });

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store.insert_payment_session(session);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_remove_preimage_with_inflight_status() {
    let payment_hash = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let session = mock_payment_session(payment_hash, PaymentStatus::Inflight);
    let expected = StoreUpdatedEvent::new_payment_updated_event(
        payment_hash,
        PaymentUpdatedPayload::Success { preimage },
    );

    let store = StoreWithPubSub::new(MockStore {
        payment_session: Some(session),
        preimage: Some(preimage),
        ..Default::default()
    });

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store.remove_preimage(&payment_hash);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}
