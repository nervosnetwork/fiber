use super::subscription_unit_tests::{
    mock_invoice, mock_payment_session, MockStore, StoreTestSubscriber,
};
use crate::{
    fiber::graph::{NetworkGraphStateStore, PaymentSessionStatus},
    gen_rand_sha256_hash,
    invoice::{CkbInvoiceStatus, InvoiceStore},
    store::{
        store_impl::StoreWithPubSub,
        subscription::{
            RichCkbInvoiceStatus, RichPaymentSessionStatus, StorePublisher, StoreSubscriberMessage,
        },
    },
};
use ractor::{concurrency::Duration, Actor};

#[tokio::test]
async fn test_insert_invoice() {
    let invoice = mock_invoice(100);
    let invoice_hash = *invoice.payment_hash();
    let expected = StoreSubscriberMessage::InvoiceUpdated {
        invoice_hash,
        status: RichCkbInvoiceStatus::Open,
    };

    let store = MockStore::default();
    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store.clone())
            .await
            .unwrap();

    let store_with_pub_sub = StoreWithPubSub::new(store, publisher_ref);

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store_with_pub_sub
        .subscribe(Box::new(subscriber_ref))
        .unwrap();
    store_with_pub_sub.insert_invoice(invoice, None).unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_update_invoice_status_to_received() {
    let invoice = mock_invoice(100);
    let invoice_hash = *invoice.payment_hash();
    let expected = StoreSubscriberMessage::InvoiceUpdated {
        invoice_hash,
        status: RichCkbInvoiceStatus::Received {
            amount: 100,
            is_finished: true,
        },
    };

    let store = MockStore {
        invoice_status: Some(CkbInvoiceStatus::Received),
        invoice: Some(mock_invoice(100)),
        ..Default::default()
    };

    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store.clone())
            .await
            .unwrap();

    let store_with_pub_sub = StoreWithPubSub::new(store, publisher_ref);

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store_with_pub_sub
        .subscribe(Box::new(subscriber_ref))
        .unwrap();
    store_with_pub_sub
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
    let session = mock_payment_session(payment_hash, PaymentSessionStatus::Created);
    let expected = StoreSubscriberMessage::PaymentUpdated {
        payment_hash,
        status: RichPaymentSessionStatus::Created,
    };

    let store = MockStore::default();
    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store.clone())
            .await
            .unwrap();

    let store_with_pub_sub = StoreWithPubSub::new(store, publisher_ref);

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store_with_pub_sub
        .subscribe(Box::new(subscriber_ref))
        .unwrap();
    store_with_pub_sub.insert_payment_session(session);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_insert_payment_session_with_success_status() {
    let payment_hash = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let session = mock_payment_session(payment_hash, PaymentSessionStatus::Success);
    let expected = StoreSubscriberMessage::PaymentUpdated {
        payment_hash,
        status: RichPaymentSessionStatus::Success { preimage },
    };

    let store = MockStore {
        payment_session: Some(session.clone()),
        preimage: Some(preimage),
        ..Default::default()
    };
    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store.clone())
            .await
            .unwrap();

    let store_with_pub_sub = StoreWithPubSub::new(store, publisher_ref);

    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    store_with_pub_sub
        .subscribe(Box::new(subscriber_ref))
        .unwrap();
    store_with_pub_sub.insert_payment_session(session);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}
