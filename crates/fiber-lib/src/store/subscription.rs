//! Publish-subscribe notifications for the store.

use std::marker::PhantomData;

use ractor::{
    async_trait, port::OutputPortSubscriber, Actor, ActorProcessingErr, ActorRef, OutputPort,
};
use tracing::warn;

use crate::{
    fiber::{
        graph::{NetworkGraphStateStore, PaymentSessionStatus},
        types::Hash256,
    },
    invoice::{CkbInvoiceStatus, InvoiceStore, PreimageStore},
};

// The state of an invoice. Basically the same as CkbInvoiceStatus,
// but with additional information for downstream services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RichCkbInvoiceStatus {
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

impl From<RichCkbInvoiceStatus> for CkbInvoiceStatus {
    fn from(state: RichCkbInvoiceStatus) -> Self {
        match state {
            RichCkbInvoiceStatus::Open => CkbInvoiceStatus::Open,
            RichCkbInvoiceStatus::Cancelled => CkbInvoiceStatus::Cancelled,
            RichCkbInvoiceStatus::Expired => CkbInvoiceStatus::Expired,
            RichCkbInvoiceStatus::Received { .. } => CkbInvoiceStatus::Received,
            RichCkbInvoiceStatus::Paid => CkbInvoiceStatus::Paid,
        }
    }
}

// Some CkbInvoiceStatus are not self-contained, we will need to enrich the status
// for downstream services. But some CkbInvoiceStatus are already good enough.
// We convert the good enough CkbInvoiceStatus to Some(RichCkbInvoiceStatus) and the others
// to None.
impl From<CkbInvoiceStatus> for Option<RichCkbInvoiceStatus> {
    fn from(status: CkbInvoiceStatus) -> Self {
        match status {
            CkbInvoiceStatus::Open => Some(RichCkbInvoiceStatus::Open),
            CkbInvoiceStatus::Cancelled => Some(RichCkbInvoiceStatus::Cancelled),
            CkbInvoiceStatus::Expired => Some(RichCkbInvoiceStatus::Expired),
            CkbInvoiceStatus::Received => None,
            CkbInvoiceStatus::Paid => Some(RichCkbInvoiceStatus::Paid),
        }
    }
}

// The state of a payment session. Basically the same as PaymentSessionStatus,
// but with additional information for downstream services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RichPaymentSessionStatus {
    /// initial status, payment session is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// related HTLC is successfully settled
    Success { preimage: Hash256 },
    /// related HTLC is failed
    Failed,
}

impl From<RichPaymentSessionStatus> for PaymentSessionStatus {
    fn from(state: RichPaymentSessionStatus) -> Self {
        match state {
            RichPaymentSessionStatus::Created => PaymentSessionStatus::Created,
            RichPaymentSessionStatus::Inflight => PaymentSessionStatus::Inflight,
            RichPaymentSessionStatus::Success { .. } => PaymentSessionStatus::Success,
            RichPaymentSessionStatus::Failed => PaymentSessionStatus::Failed,
        }
    }
}

// Some PaymentSessionStatus are not self-contained, we will need to enrich the status
// for downstream services. But some PaymentSessionStatus are already good enough.
// We convert the good enough PaymentSessionStatus to Some(RichPaymentSessionStatus) and the others
// to None.
impl From<PaymentSessionStatus> for Option<RichPaymentSessionStatus> {
    fn from(status: PaymentSessionStatus) -> Self {
        match status {
            PaymentSessionStatus::Created => Some(RichPaymentSessionStatus::Created),
            PaymentSessionStatus::Inflight => Some(RichPaymentSessionStatus::Inflight),
            PaymentSessionStatus::Success => None,
            PaymentSessionStatus::Failed => Some(RichPaymentSessionStatus::Failed),
        }
    }
}

/// Message sent from Store to publisher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorePublisherMessage {
    InvoiceUpdated {
        invoice_hash: Hash256,
        status: CkbInvoiceStatus,
    },
    PaymentUpdated {
        payment_hash: Hash256,
        status: PaymentSessionStatus,
    },
}

/// Message sent from publisher to subscribers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreSubscriberMessage {
    InvoiceUpdated {
        invoice_hash: Hash256,
        status: RichCkbInvoiceStatus,
    },
    PaymentUpdated {
        payment_hash: Hash256,
        status: RichPaymentSessionStatus,
    },
}

pub enum StorePubSubMessage {
    Publish(StorePublisherMessage),
    Subscribe(OutputPortSubscriber<StoreSubscriberMessage>),
}

/// This ractor receives the notification StoreUpdateMessage and sends out StoreRichUpdateMessage
pub struct StorePublisher<S>(PhantomData<S>);
pub struct StorePublisherState<S> {
    store: S,
    output_port: OutputPort<StoreSubscriberMessage>,
}

impl<S> StorePublisher<S> {
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<S> Default for StorePublisher<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<S> Actor for StorePublisher<S>
where
    S: NetworkGraphStateStore + InvoiceStore + PreimageStore + Send + Sync + 'static,
{
    type Msg = StorePubSubMessage;
    type State = StorePublisherState<S>;
    type Arguments = S;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State {
            store: args,
            output_port: OutputPort::default(),
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            StorePubSubMessage::Publish(input) => match state.convert(input.clone()) {
                Some(output) => state.output_port.send(output),
                None => {
                    warn!(input = ?input, "Failed to convert incoming store update message");
                }
            },
            StorePubSubMessage::Subscribe(subscriber) => {
                subscriber.subscribe_to_port(&state.output_port)
            }
        }
        Ok(())
    }
}

impl<S> StorePublisherState<S>
where
    S: NetworkGraphStateStore + InvoiceStore + PreimageStore + Send + Sync + 'static,
{
    pub fn get_rich_invoice_status(&self, invoice_hash: Hash256) -> Option<RichCkbInvoiceStatus> {
        self.store
            .get_invoice_status(&invoice_hash)
            .and_then(|status| match status {
                CkbInvoiceStatus::Received => {
                    // TODO: We should save received amount to the store. This is useful to
                    // determine if the invoice is fully paid.
                    // Currently we assume that the invoice is fully paid if the status is
                    // Received.
                    let amount = self
                        .store
                        .get_invoice(&invoice_hash)
                        .and_then(|invoice| invoice.amount);
                    amount.map(|amount| RichCkbInvoiceStatus::Received {
                        amount,
                        is_finished: true,
                    })
                }
                _ => status.into(),
            })
    }

    pub fn get_rich_payment_session_status(
        &self,
        payment_hash: Hash256,
    ) -> Option<RichPaymentSessionStatus> {
        self.store
            .get_payment_session(payment_hash)
            .and_then(|session| match session.status {
                PaymentSessionStatus::Success => {
                    let preimage = self.store.get_preimage(&payment_hash);
                    preimage.map(|preimage| RichPaymentSessionStatus::Success { preimage })
                }
                status => status.into(),
            })
    }

    fn convert(&self, input: StorePublisherMessage) -> Option<StoreSubscriberMessage> {
        match input {
            StorePublisherMessage::InvoiceUpdated {
                invoice_hash,
                status,
            } => {
                let status: Option<RichCkbInvoiceStatus> = status.into();
                // TODO: there is a race condition here. If the status passed is not self-contained,
                // we need to query the store to get the current state. E.g. a CkbInvoiceStatus::Received
                // status does not contain the amount, so we need to query the store to get the amount.
                // But when we query the store, the status might have changed. We need to handle this.
                status
                    .or_else(|| self.get_rich_invoice_status(invoice_hash))
                    .map(|status| StoreSubscriberMessage::InvoiceUpdated {
                        invoice_hash,
                        status,
                    })
            }
            StorePublisherMessage::PaymentUpdated {
                payment_hash,
                status,
            } => {
                let status: Option<RichPaymentSessionStatus> = status.into();
                // TODO: there is a race condition here. If the status passed is not self-contained,
                // we need to query the store to get the current state. E.g. a PaymentSessionStatus::Success
                // status does not contain the preimage, so we need to query the store to get the preimage.
                // But when we query the store, the status might have changed. We need to handle this.
                status
                    .or_else(|| self.get_rich_payment_session_status(payment_hash))
                    .map(|status| StoreSubscriberMessage::PaymentUpdated {
                        payment_hash,
                        status,
                    })
            }
        }
    }
}
