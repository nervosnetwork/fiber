use jsonrpsee::{
    core::StringError, PendingSubscriptionSink, RpcModule, SubscriptionMessage, SubscriptionSink,
    TrySendError,
};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use serde::Serialize;
use tracing::debug;

use crate::{
    fiber::types::Hash256,
    store::{
        subscription::{InvoiceSubscription, InvoiceUpdate, PaymentSubscription, PaymentUpdate},
        subscription_impl::SubscriptionImpl,
        SubscriptionId,
    },
};

pub(crate) async fn start_pubsub_server<C: Send + Sync + 'static>(
    module: &mut RpcModule<C>,
    subscription_impl: &SubscriptionImpl,
    supervisor: &ActorCell,
) {
    // Don't know why some many clones are needed here.
    // I just follow rust compiler's suggestion.
    let subscription_impl_1: SubscriptionImpl = subscription_impl.clone();
    let subscription_impl_2: SubscriptionImpl = subscription_impl.clone();
    let supervisor_1 = supervisor.clone();
    let supervisor_2 = supervisor.clone();
    module
        .register_subscription(
            "subscribe_invoice_update",
            "subscribe_invoice_update",
            "unsubscribe_invoice_update",
            move |params, pending_sink, _context| {
                let subscription_impl: SubscriptionImpl = subscription_impl_1.clone();
                let supervisor = supervisor_1.clone();
                async move {
                    PubsubActor::new_invoice(params.one::<Hash256>()?)
                        .start(subscription_impl, pending_sink, supervisor)
                        .await
                }
            },
        )
        .expect("register subscribe_invoice_update");
    module
        .register_subscription(
            "subscribe_payment_update",
            "subscribe_payment_update",
            "unsubscribe_payment_update",
            move |params, pending_sink, _context| {
                let subscription_impl: SubscriptionImpl = subscription_impl_2.clone();
                let supervisor = supervisor_2.clone();
                async move {
                    PubsubActor::new_payment(params.one::<Hash256>()?)
                        .start(subscription_impl, pending_sink, supervisor)
                        .await
                }
            },
        )
        .expect("register subscribe_payment_update");
}

#[derive(Debug)]
pub enum PubsubKey {
    Invoice(Hash256),
    Payment(Hash256),
}

pub struct PubsubActor {
    pubsub_key: PubsubKey,
}

impl PubsubActor {
    pub fn new_invoice(hash: Hash256) -> Self {
        Self {
            pubsub_key: PubsubKey::Invoice(hash),
        }
    }

    pub fn new_payment(hash: Hash256) -> Self {
        Self {
            pubsub_key: PubsubKey::Payment(hash),
        }
    }

    pub async fn start(
        self,
        subscription_impl: SubscriptionImpl,
        pending_sink: PendingSubscriptionSink,
        supervisor: ActorCell,
    ) -> Result<(), StringError> {
        Actor::spawn_linked(None, self, (subscription_impl, pending_sink), supervisor).await?;
        Ok(())
    }
}

pub enum PubsubActorMessage {
    InvoiceUpdate(InvoiceUpdate),
    PaymentUpdate(PaymentUpdate),
}

impl From<InvoiceUpdate> for PubsubActorMessage {
    fn from(update: InvoiceUpdate) -> Self {
        Self::InvoiceUpdate(update)
    }
}

impl TryFrom<PubsubActorMessage> for InvoiceUpdate {
    type Error = anyhow::Error;

    fn try_from(message: PubsubActorMessage) -> Result<Self, Self::Error> {
        match message {
            PubsubActorMessage::InvoiceUpdate(update) => Ok(update),
            _ => Err(anyhow::anyhow!(
                "Invalid message type, expected InvoiceUpdate"
            )),
        }
    }
}

impl From<PaymentUpdate> for PubsubActorMessage {
    fn from(update: PaymentUpdate) -> Self {
        Self::PaymentUpdate(update)
    }
}

impl TryFrom<PubsubActorMessage> for PaymentUpdate {
    type Error = anyhow::Error;

    fn try_from(message: PubsubActorMessage) -> Result<Self, Self::Error> {
        match message {
            PubsubActorMessage::PaymentUpdate(update) => Ok(update),
            _ => Err(anyhow::anyhow!(
                "Invalid message type, expected PaymentUpdate"
            )),
        }
    }
}

pub struct PubsubActorState {
    subscription_impl: SubscriptionImpl,
    subscription_id: SubscriptionId,
    sink: SubscriptionSink,
}

impl PubsubActorState {
    pub fn new(
        subscription_impl: SubscriptionImpl,
        subscription_id: SubscriptionId,
        sink: SubscriptionSink,
    ) -> Self {
        Self {
            subscription_impl,
            subscription_id,
            sink,
        }
    }
}

#[ractor::async_trait]
impl Actor for PubsubActor {
    type Msg = PubsubActorMessage;
    type State = PubsubActorState;
    type Arguments = (SubscriptionImpl, PendingSubscriptionSink);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (subscription_impl, sink): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let subscription_id = match self.pubsub_key {
            PubsubKey::Invoice(hash) => {
                subscription_impl
                    .subscribe_invoice(hash, myself.get_derived())
                    .await?
            }
            PubsubKey::Payment(hash) => {
                subscription_impl
                    .subscribe_payment(hash, myself.get_derived())
                    .await?
            }
        };
        let sink = sink.accept().await?;
        debug!(subscription_id = subscription_id, key = ?self.pubsub_key, "Subscribed to updates");
        Ok(Self::State::new(subscription_impl, subscription_id, sink))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let result = match message {
            PubsubActorMessage::InvoiceUpdate(update) => {
                send_message_to_sink(&mut state.sink, update).await
            }
            PubsubActorMessage::PaymentUpdate(update) => {
                send_message_to_sink(&mut state.sink, update).await
            }
        };
        if let Err(error) = result {
            myself.stop(Some(format!("Failed to send message to sink: {:?}", error)));
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match self.pubsub_key {
            PubsubKey::Invoice(hash) => {
                debug!(
                    subscription_id = state.subscription_id,
                    hash = ?hash,
                    "Unsubscribing from invoice updates"
                );
                state
                    .subscription_impl
                    .unsubscribe_invoice(state.subscription_id)
                    .await?
            }
            PubsubKey::Payment(hash) => {
                debug!(
                    subscription_id = state.subscription_id,
                    hash = ?hash,
                    "Unsubscribing from payment updates"
                );
                state
                    .subscription_impl
                    .unsubscribe_payment(state.subscription_id)
                    .await?
            }
        }
        Ok(())
    }
}

pub async fn send_message_to_sink<T: Serialize>(
    sink: &mut SubscriptionSink,
    message: T,
) -> Result<(), anyhow::Error> {
    let msg = SubscriptionMessage::from_json(&message).expect("serialize message");
    match sink.try_send(msg) {
        Ok(_) => Ok(()),
        Err(TrySendError::Closed(_)) => Err(anyhow::anyhow!("Subscription was closed")),
        // channel is full, let's be naive an just drop the message.
        Err(TrySendError::Full(_)) => Ok(()),
    }
}
