use std::collections::{hash_map::Entry, HashMap};

use ractor::{
    async_trait, call_t, Actor, ActorProcessingErr, ActorRef, MessagingErr, RactorErr, RpcReplyPort,
};
use thiserror::Error;
use tracing::warn;

use crate::{
    fiber::{graph::PaymentSessionStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

use super::Store;

const CALLING_ACTOR_TIMEOUT_MS: u64 = 1000;

pub(crate) struct SubscriptionActor {
    store: Store,
}

impl SubscriptionActor {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SubscriptionImpl {
    actor: ActorRef<SubscriptionActorMessage>,
}

pub async fn new_subscription(store: Store) -> SubscriptionImpl {
    let actor = SubscriptionActor::new(store);
    let actor_ref = Actor::spawn(Some("store subscription actor".to_string()), actor, ())
        .await
        .expect("start store subscription actor")
        .0;
    SubscriptionImpl { actor: actor_ref }
}

#[derive(Debug)]
pub enum SubscriptionActorMessage {
    InvoiceUpdated(Hash256, CkbInvoiceStatus),
    PaymentUpdated(Hash256, PaymentSessionStatus),
    SubscribeInvoiceUpdates(
        Hash256,
        ActorRef<InvoiceUpdate>,
        RpcReplyPort<SubscriptionId>,
    ),
    UnsubscribeInvoiceUpdates(SubscriptionId),
    SubscribePaymentUpdates(
        Hash256,
        ActorRef<PaymentUpdate>,
        RpcReplyPort<SubscriptionId>,
    ),
    UnsubscribePaymentUpdates(SubscriptionId),
}

type SubscriptionId = u64;

struct InvoiceSubscriber {
    id: SubscriptionId,
    receiver: ActorRef<InvoiceUpdate>,
}

impl InvoiceSubscriber {
    pub fn new(id: SubscriptionId, receiver: ActorRef<InvoiceUpdate>) -> Self {
        Self { id, receiver }
    }

    pub fn send_update(&self, update: InvoiceUpdate) -> bool {
        self.receiver.send_message(update).is_ok()
    }
}

struct PaymentSubscriber {
    id: SubscriptionId,
    receiver: ActorRef<PaymentUpdate>,
}

impl PaymentSubscriber {
    pub fn new(id: SubscriptionId, receiver: ActorRef<PaymentUpdate>) -> Self {
        Self { id, receiver }
    }

    pub fn send_update(&self, update: PaymentUpdate) -> bool {
        self.receiver.send_message(update).is_ok()
    }
}

#[derive(Default)]
pub struct SubscriptionActorState {
    next_subscriber_id: SubscriptionId,
    invoice_subscriptions: HashMap<Hash256, Vec<InvoiceSubscriber>>,
    payment_subscriptions: HashMap<Hash256, Vec<PaymentSubscriber>>,
}

impl SubscriptionActorState {
    pub fn send_invoice_update(&mut self, invoice_hash: Hash256, update: InvoiceUpdate) {
        if let Entry::Occupied(mut entry) = self.invoice_subscriptions.entry(invoice_hash) {
            entry
                .get_mut()
                .retain(|subscription| subscription.send_update(update.clone()));
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    pub fn send_payment_update(&mut self, payment_hash: Hash256, update: PaymentUpdate) {
        if let Entry::Occupied(mut entry) = self.payment_subscriptions.entry(payment_hash) {
            entry
                .get_mut()
                .retain(|subscription| subscription.send_update(update.clone()));
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    pub fn get_next_subscriber_id(&mut self) -> SubscriptionId {
        let id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        id
    }

    pub fn add_invoice_subscriber(
        &mut self,
        invoice_hash: Hash256,
        receiver: ActorRef<InvoiceUpdate>,
    ) -> SubscriptionId {
        let id = self.get_next_subscriber_id();
        self.invoice_subscriptions
            .entry(invoice_hash)
            .or_default()
            .push(InvoiceSubscriber::new(id, receiver));
        id
    }

    pub fn add_payment_subscriber(
        &mut self,
        payment_hash: Hash256,
        receiver: ActorRef<PaymentUpdate>,
    ) -> SubscriptionId {
        let id = self.get_next_subscriber_id();
        self.payment_subscriptions
            .entry(payment_hash)
            .or_default()
            .push(PaymentSubscriber::new(id, receiver));
        id
    }
}

#[async_trait]
impl Actor for SubscriptionActor {
    type Msg = SubscriptionActorMessage;
    type State = SubscriptionActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State::default())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SubscriptionActorMessage::InvoiceUpdated(hash, status) => {
                match self.create_invoice_state_from_status(hash, status) {
                    Some(s) => state.send_invoice_update(hash, InvoiceUpdate { hash, state: s }),
                    None => {
                        warn!(hash = ?hash, status = ?status, "Failed to create invoice state from status");
                    }
                }
            }
            SubscriptionActorMessage::PaymentUpdated(hash, status) => {
                match self.create_payment_state_from_status(hash, status) {
                    Some(s) => state.send_payment_update(hash, PaymentUpdate { hash, state: s }),
                    None => {
                        warn!(hash = ?hash, status = ?status, "Failed to create payment state from status");
                    }
                }
            }
            SubscriptionActorMessage::SubscribeInvoiceUpdates(hash, receiver, reply) => {
                let _ = reply.send(state.add_invoice_subscriber(hash, receiver));
            }
            SubscriptionActorMessage::UnsubscribeInvoiceUpdates(subscription) => {
                for subscribers in state.invoice_subscriptions.values_mut() {
                    // TODO: maybe remember which hash the subscription is for to avoid iterating over all
                    // the subscriptions
                    let old_num_subscribers = subscribers.len();
                    subscribers.retain(|s| s.id != subscription);
                    if subscribers.len() != old_num_subscribers {
                        break;
                    }
                }
            }
            SubscriptionActorMessage::SubscribePaymentUpdates(hash, receiver, reply) => {
                let _ = reply.send(state.add_payment_subscriber(hash, receiver));
            }
            SubscriptionActorMessage::UnsubscribePaymentUpdates(subscription) => {
                // TODO: maybe remember which hash the subscription is for to avoid iterating over all
                // the subscriptions
                for subscribers in state.payment_subscriptions.values_mut() {
                    let old_num_subscribers = subscribers.len();
                    subscribers.retain(|s| s.id != subscription);
                    if subscribers.len() != old_num_subscribers {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

impl SubscriptionActor {
    pub fn get_current_invoice_state(&self, invoice_hash: Hash256) -> Option<InvoiceState> {
        unimplemented!()
    }

    pub fn get_current_payment_state(&self, payment_hash: Hash256) -> Option<PaymentState> {
        unimplemented!()
    }

    pub fn create_invoice_state_from_status(
        &self,
        invoice_hash: Hash256,
        status: CkbInvoiceStatus,
    ) -> Option<InvoiceState> {
        let state: Option<InvoiceState> = status.into();
        state.or_else(|| self.get_current_invoice_state(invoice_hash))
    }

    pub fn create_payment_state_from_status(
        &self,
        payment_hash: Hash256,
        status: PaymentSessionStatus,
    ) -> Option<PaymentState> {
        let state: Option<PaymentState> = status.into();
        state.or_else(|| self.get_current_payment_state(payment_hash))
    }
}

#[derive(Error, Debug)]
pub enum SubscriptionError {
    #[error("Error while sending actor message: {0:?}")]
    MessagingErr(#[from] MessagingErr<SubscriptionActorMessage>),
    #[error("Error while processing actor message: {0:?}")]
    RactorErr(#[from] RactorErr<SubscriptionActorMessage>),
}

// The state of an invoice. Basically the same as CkbInvoiceStatus,
// but with additional information for downstream services.
#[derive(Clone)]
pub enum InvoiceState {
    /// The invoice is open and can be paid.
    Open,
    /// The invoice is cancelled.
    Cancelled,
    /// The invoice is expired.
    Expired,
    /// The invoice is received, but not settled yet.
    Received {
        /// The amount of the invoice.
        amount: u128,
        /// Depending on whether AMP is supported, the invoice may have multiple parts,
        /// this field indicates if we received all parts.
        is_finished: bool,
    },
    /// The invoice is paid.
    Paid,
}

impl From<InvoiceState> for CkbInvoiceStatus {
    fn from(state: InvoiceState) -> Self {
        match state {
            InvoiceState::Open => CkbInvoiceStatus::Open,
            InvoiceState::Cancelled => CkbInvoiceStatus::Cancelled,
            InvoiceState::Expired => CkbInvoiceStatus::Expired,
            InvoiceState::Received { .. } => CkbInvoiceStatus::Received,
            InvoiceState::Paid => CkbInvoiceStatus::Paid,
        }
    }
}

// Some CkbInvoiceStatus are not self-contained, we will need to enrich the status
// for downstream services. But some CkbInvoiceStatus are already good enough.
// We convert the good enough CkbInvoiceStatus to Some(InvoiceState) and the others
// to None.
impl From<CkbInvoiceStatus> for Option<InvoiceState> {
    fn from(status: CkbInvoiceStatus) -> Self {
        match status {
            CkbInvoiceStatus::Open => Some(InvoiceState::Open),
            CkbInvoiceStatus::Cancelled => Some(InvoiceState::Cancelled),
            CkbInvoiceStatus::Expired => Some(InvoiceState::Expired),
            CkbInvoiceStatus::Received => None,
            CkbInvoiceStatus::Paid => Some(InvoiceState::Paid),
        }
    }
}

#[derive(Clone)]
pub struct InvoiceUpdate {
    hash: Hash256,
    state: InvoiceState,
}

// The state of a payment session. Basically the same as PaymentSessionStatus,
// but with additional information for downstream services.
#[derive(Clone)]
pub enum PaymentState {
    /// initial status, payment session is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// related HTLC is successfully settled
    Success { preimage: Hash256 },
    /// related HTLC is failed
    Failed,
}

impl From<PaymentState> for PaymentSessionStatus {
    fn from(state: PaymentState) -> Self {
        match state {
            PaymentState::Created => PaymentSessionStatus::Created,
            PaymentState::Inflight => PaymentSessionStatus::Inflight,
            PaymentState::Success { .. } => PaymentSessionStatus::Success,
            PaymentState::Failed => PaymentSessionStatus::Failed,
        }
    }
}

// Some PaymentSessionStatus are not self-contained, we will need to enrich the status
// for downstream services. But some PaymentSessionStatus are already good enough.
// We convert the good enough PaymentSessionStatus to Some(PaymentState) and the others
// to None.
impl From<PaymentSessionStatus> for Option<PaymentState> {
    fn from(status: PaymentSessionStatus) -> Self {
        match status {
            PaymentSessionStatus::Created => Some(PaymentState::Created),
            PaymentSessionStatus::Inflight => Some(PaymentState::Inflight),
            PaymentSessionStatus::Success => None,
            PaymentSessionStatus::Failed => Some(PaymentState::Failed),
        }
    }
}

#[derive(Clone)]
pub struct PaymentUpdate {
    hash: Hash256,
    state: PaymentState,
}

trait StoreUpdateSubscription: InvoiceSubscription + PaymentSubscription {}

#[async_trait]
pub trait InvoiceSubscription: Send + Clone {
    type Subscription;
    type Error: std::error::Error;

    async fn subscribe_invoice(
        &self,
        invoice_hash: Hash256,
        receiver: ActorRef<InvoiceUpdate>,
    ) -> Result<Self::Subscription, Self::Error>;

    async fn unsubscribe_invoice(
        &self,
        subscription: Self::Subscription,
    ) -> Result<(), Self::Error>;
}

#[async_trait]
pub trait PaymentSubscription: Send + Clone {
    type Subscription;
    type Error: std::error::Error;

    async fn subscribe_payment(
        &self,
        payment_hash: Hash256,
        receiver: ActorRef<PaymentUpdate>,
    ) -> Result<Self::Subscription, Self::Error>;

    async fn unsubscribe_payment(
        &self,
        subscription: Self::Subscription,
    ) -> Result<(), Self::Error>;
}

#[async_trait]
impl InvoiceSubscription for SubscriptionImpl {
    type Subscription = SubscriptionId;
    type Error = SubscriptionError;

    async fn subscribe_invoice(
        &self,
        invoice_hash: Hash256,
        receiver: ActorRef<InvoiceUpdate>,
    ) -> Result<Self::Subscription, Self::Error> {
        let id = call_t!(
            self.actor,
            |reply| SubscriptionActorMessage::SubscribeInvoiceUpdates(
                invoice_hash,
                receiver,
                reply
            ),
            CALLING_ACTOR_TIMEOUT_MS
        )?;
        Ok(id)
    }

    async fn unsubscribe_invoice(
        &self,
        subscription: Self::Subscription,
    ) -> Result<(), Self::Error> {
        self.actor
            .send_message(SubscriptionActorMessage::UnsubscribeInvoiceUpdates(
                subscription,
            ))?;
        Ok(())
    }
}

#[async_trait]
impl PaymentSubscription for SubscriptionImpl {
    type Subscription = SubscriptionId;
    type Error = SubscriptionError;

    async fn subscribe_payment(
        &self,
        payment_hash: Hash256,
        receiver: ActorRef<PaymentUpdate>,
    ) -> Result<Self::Subscription, Self::Error> {
        let id = call_t!(
            self.actor,
            |reply| SubscriptionActorMessage::SubscribePaymentUpdates(
                payment_hash,
                receiver,
                reply
            ),
            CALLING_ACTOR_TIMEOUT_MS
        )?;
        Ok(id)
    }

    async fn unsubscribe_payment(
        &self,
        subscription: Self::Subscription,
    ) -> Result<(), Self::Error> {
        self.actor
            .send_message(SubscriptionActorMessage::UnsubscribePaymentUpdates(
                subscription,
            ))?;
        Ok(())
    }
}

impl StoreUpdateSubscription for SubscriptionImpl {}

pub trait OnInvoiceUpdated: Send + Clone {
    fn on_invoice_updated(&self, invoice_hash: Hash256, status: CkbInvoiceStatus);
}

pub trait OnPaymentUpdated: Send + Clone {
    fn on_payment_updated(&self, payment_hash: Hash256, status: PaymentSessionStatus);
}

impl OnInvoiceUpdated for SubscriptionImpl {
    fn on_invoice_updated(&self, invoice_hash: Hash256, status: CkbInvoiceStatus) {
        let _ = self
            .actor
            .send_message(SubscriptionActorMessage::InvoiceUpdated(
                invoice_hash,
                status,
            ));
    }
}

impl OnPaymentUpdated for SubscriptionImpl {
    fn on_payment_updated(&self, payment_hash: Hash256, status: PaymentSessionStatus) {
        let _ = self
            .actor
            .send_message(SubscriptionActorMessage::PaymentUpdated(
                payment_hash,
                status,
            ));
    }
}

pub type NoopStoreUpdateHook = ();

impl OnInvoiceUpdated for NoopStoreUpdateHook {
    #[inline]
    fn on_invoice_updated(&self, _invoice_hash: Hash256, _status: CkbInvoiceStatus) {}
}

impl OnPaymentUpdated for NoopStoreUpdateHook {
    #[inline]
    fn on_payment_updated(&self, _payment_hash: Hash256, _status: PaymentSessionStatus) {}
}
