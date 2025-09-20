use crate::{
    fiber::{
        graph::NetworkGraphStateStore,
        history::{Direction, TimedResult},
        network::SendPaymentData,
        payment::{Attempt, AttemptStatus, PaymentSession, PaymentStatus},
        types::Hash256,
    },
    gen_rand_fiber_public_key, gen_rand_sha256_hash,
    invoice::{
        CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore, PreimageStore,
    },
    store::pub_sub::{
        InvoiceUpdatedPayload, PaymentUpdatedPayload, StoreUpdatedEvent, StoreWithPubSub,
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

    fn get_payment_sessions_with_status(&self, _status: PaymentStatus) -> Vec<PaymentSession> {
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

    fn remove_channel_history(&mut self, _channel_outpoint: &packed::OutPoint) {
        unimplemented!()
    }

    fn get_attempt(&self, _payment_hash: Hash256, _attempt_id: u64) -> Option<Attempt> {
        unimplemented!()
    }

    fn insert_attempt(&self, _attempt: Attempt) {
        unimplemented!()
    }

    fn get_attempts(&self, _payment_hash: Hash256) -> Vec<Attempt> {
        unimplemented!()
    }

    fn delete_attempts(&self, _payment_hash: Hash256) {
        unimplemented!()
    }

    fn get_attempts_with_statuses(
        &self,
        _status: &[AttemptStatus],
    ) -> Vec<crate::fiber::payment::Attempt> {
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
        // skip
    }

    fn remove_preimage(&self, _payment_hash: &Hash256) {
        // skip
    }

    fn get_preimage(&self, _payment_hash: &Hash256) -> Option<Hash256> {
        self.preimage
    }
}

pub struct StoreTestSubscriber;

#[async_trait]
impl Actor for StoreTestSubscriber {
    type Msg = StoreUpdatedEvent;
    type Arguments = StoreUpdatedEvent;
    type State = StoreUpdatedEvent;

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
        .payment_hash(gen_rand_sha256_hash())
        .build()
        .expect("mock invoice")
}

pub fn mock_payment_session(payment_hash: Hash256, status: PaymentStatus) -> PaymentSession {
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
        allow_mpp: false,
        channel_stats: Default::default(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Duration since unix epoch")
        .as_millis() as u64;
    let mut session = PaymentSession {
        request: payment_data,
        last_error: None,
        try_limit: 10,
        status,
        created_at: now,
        last_updated_at: now,
        cached_attempts: Default::default(),
    };
    session.status = status;
    session
}

#[tokio::test]
async fn test_invoice_open() {
    let invoice_hash = gen_rand_sha256_hash();
    let input =
        StoreUpdatedEvent::new_invoice_updated_event(invoice_hash, InvoiceUpdatedPayload::Open);

    let store = StoreWithPubSub::new(MockStore::default());
    let (subscriber_ref, subscriber_handle) =
        Actor::spawn(None, StoreTestSubscriber, input.clone())
            .await
            .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store.publish(input);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}

#[tokio::test]
async fn test_payment_success() {
    let payment_hash = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let input = StoreUpdatedEvent::new_payment_updated_event(
        payment_hash,
        PaymentUpdatedPayload::Success { preimage },
    );

    let store = StoreWithPubSub::new(MockStore {
        payment_session: Some(mock_payment_session(payment_hash, PaymentStatus::Success)),
        preimage: Some(preimage),
        ..Default::default()
    });
    let (subscriber_ref, subscriber_handle) =
        Actor::spawn(None, StoreTestSubscriber, input.clone())
            .await
            .unwrap();

    store.subscribe(Box::new(subscriber_ref));
    store.publish(input);

    ractor::concurrency::timeout(Duration::from_millis(100), subscriber_handle)
        .await
        .expect("Test actor failed in exit")
        .unwrap();
}
