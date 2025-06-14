use crate::{
    fiber::{
        graph::{NetworkGraphStateStore, PaymentSession, PaymentSessionStatus},
        history::{Direction, TimedResult},
        network::SendPaymentData,
        types::Hash256,
    },
    gen_rand_fiber_public_key, gen_rand_sha256_hash,
    invoice::{
        CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore, PreimageStore,
    },
    store::subscription::{
        RichCkbInvoiceStatus, RichPaymentSessionStatus, StorePubSubMessage, StorePublisher,
        StorePublisherMessage, StoreSubscriberMessage,
    },
};
use ckb_types::packed;
use ractor::{async_trait, concurrency::Duration, Actor, ActorProcessingErr, ActorRef};

#[derive(Default, Clone)]
pub struct MockStore {
    pub invoice: Option<CkbInvoice>,
    pub invoice_status: Option<CkbInvoiceStatus>,
    pub payment_session: Option<PaymentSession>,
    pub preimage: Option<Hash256>,
}

impl NetworkGraphStateStore for MockStore {
    fn get_payment_session(&self, _payment_hash: Hash256) -> Option<PaymentSession> {
        self.payment_session.clone()
    }

    fn get_payment_sessions_with_status(
        &self,
        _status: PaymentSessionStatus,
    ) -> Vec<PaymentSession> {
        unimplemented!()
    }

    fn insert_payment_session(&self, _session: PaymentSession) {
        // skip
    }

    fn insert_payment_history_result(
        &mut self,
        _channel_outpoint: packed::OutPoint,
        _direction: Direction,
        _result: TimedResult,
    ) {
        unimplemented!()
    }

    fn get_payment_history_results(&self) -> Vec<(packed::OutPoint, Direction, TimedResult)> {
        unimplemented!()
    }
}

impl InvoiceStore for MockStore {
    fn get_invoice(&self, _id: &Hash256) -> Option<CkbInvoice> {
        self.invoice.clone()
    }

    fn insert_invoice(
        &self,
        _invoice: CkbInvoice,
        _preimage: Option<Hash256>,
    ) -> Result<(), crate::invoice::InvoiceError> {
        // skip
        Ok(())
    }

    fn update_invoice_status(
        &self,
        _id: &Hash256,
        _status: CkbInvoiceStatus,
    ) -> Result<(), crate::invoice::InvoiceError> {
        // skip
        Ok(())
    }

    fn get_invoice_status(&self, _id: &Hash256) -> Option<CkbInvoiceStatus> {
        self.invoice_status
    }

    fn get_invoice_channel_info(
        &self,
        _payment_hash: &Hash256,
    ) -> Vec<crate::invoice::InvoiceChannelInfo> {
        unimplemented!()
    }

    fn add_invoice_channel_info(
        &self,
        _payment_hash: &Hash256,
        _invoice_channel_info: crate::invoice::InvoiceChannelInfo,
    ) -> Result<Vec<crate::invoice::InvoiceChannelInfo>, crate::invoice::InvoiceError> {
        unimplemented!()
    }
}

impl PreimageStore for MockStore {
    fn insert_preimage(&self, _payment_hash: Hash256, _preimage: Hash256) {
        unimplemented!()
    }

    fn remove_preimage(&self, _payment_hash: &Hash256) {
        unimplemented!()
    }

    fn get_preimage(&self, _payment_hash: &Hash256) -> Option<Hash256> {
        self.preimage
    }

    fn search_preimage(&self, _payment_hash_prefix: &[u8]) -> Option<Hash256> {
        unimplemented!()
    }
}

pub struct StoreTestSubscriber;

#[async_trait]
impl Actor for StoreTestSubscriber {
    type Msg = StoreSubscriberMessage;
    type Arguments = StoreSubscriberMessage;
    type State = StoreSubscriberMessage;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::State,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // Exit once received the expected message
        if message == *state {
            myself.stop(None);
        } else {
            eprintln!("expected {:?}, got {:?}", state, message);
        }
        Ok(())
    }
}

pub fn mock_invoice(amount: u128) -> CkbInvoice {
    InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(amount))
        .build()
        .expect("mock invoice")
}

pub fn mock_payment_session(payment_hash: Hash256, status: PaymentSessionStatus) -> PaymentSession {
    let payment_data = SendPaymentData {
        target_pubkey: gen_rand_fiber_public_key(),
        amount: 100,
        payment_hash,
        invoice: None,
        final_tlc_expiry_delta: 0,
        tlc_expiry_limit: 0,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        hop_hints: vec![],
        dry_run: false,
        custom_records: None,
        router: vec![],
    };
    let mut session = PaymentSession::new(payment_data.clone(), 10);
    session.status = status;
    session
}

#[tokio::test]
async fn test_invoice_open() {
    let invoice_hash = gen_rand_sha256_hash();
    let input = StorePublisherMessage::InvoiceUpdated {
        invoice_hash,
        status: CkbInvoiceStatus::Open,
    };
    let expected = StoreSubscriberMessage::InvoiceUpdated {
        invoice_hash,
        status: RichCkbInvoiceStatus::Open,
    };

    let store = MockStore::default();
    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store)
            .await
            .unwrap();
    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    publisher_ref
        .send_message(StorePubSubMessage::Subscribe(Box::new(subscriber_ref)))
        .unwrap();
    publisher_ref
        .send_message(StorePubSubMessage::Publish(input))
        .unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_invoice_received() {
    let invoice_hash = gen_rand_sha256_hash();
    let input = StorePublisherMessage::InvoiceUpdated {
        invoice_hash,
        status: CkbInvoiceStatus::Received,
    };
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
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store)
            .await
            .unwrap();
    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    publisher_ref
        .send_message(StorePubSubMessage::Subscribe(Box::new(subscriber_ref)))
        .unwrap();
    publisher_ref
        .send_message(StorePubSubMessage::Publish(input))
        .unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_payment_created() {
    let payment_hash = gen_rand_sha256_hash();
    let input = StorePublisherMessage::PaymentUpdated {
        payment_hash,
        status: PaymentSessionStatus::Created,
    };
    let expected = StoreSubscriberMessage::PaymentUpdated {
        payment_hash,
        status: RichPaymentSessionStatus::Created,
    };

    let store = MockStore::default();
    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store)
            .await
            .unwrap();
    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    publisher_ref
        .send_message(StorePubSubMessage::Subscribe(Box::new(subscriber_ref)))
        .unwrap();
    publisher_ref
        .send_message(StorePubSubMessage::Publish(input))
        .unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_payment_success() {
    let payment_hash = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let input = StorePublisherMessage::PaymentUpdated {
        payment_hash,
        status: PaymentSessionStatus::Success,
    };
    let expected = StoreSubscriberMessage::PaymentUpdated {
        payment_hash,
        status: RichPaymentSessionStatus::Success { preimage },
    };

    let store = MockStore {
        payment_session: Some(mock_payment_session(
            payment_hash,
            PaymentSessionStatus::Success,
        )),
        preimage: Some(preimage),
        ..Default::default()
    };
    let (publisher_ref, _publisher_handle) =
        Actor::spawn(None, StorePublisher::<MockStore>::new(), store)
            .await
            .unwrap();
    let (subscriber_ref, subscriber_handle) = Actor::spawn(None, StoreTestSubscriber, expected)
        .await
        .unwrap();

    publisher_ref
        .send_message(StorePubSubMessage::Subscribe(Box::new(subscriber_ref)))
        .unwrap();
    publisher_ref
        .send_message(StorePubSubMessage::Publish(input))
        .unwrap();

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}
