//! Publish-subscribe notifications for the store.

use std::sync::Arc;

use ractor::{port::OutputPortSubscriber, OutputPort};

use crate::{
    fiber::{payment::PaymentStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvoiceUpdatedPayload {
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvoiceUpdatedEvent {
    pub invoice_hash: Hash256,
    pub payload: InvoiceUpdatedPayload,
}

impl TryFrom<CkbInvoiceStatus> for InvoiceUpdatedPayload {
    type Error = String;

    fn try_from(value: CkbInvoiceStatus) -> Result<Self, Self::Error> {
        let payload = match value {
            CkbInvoiceStatus::Open => InvoiceUpdatedPayload::Open,
            CkbInvoiceStatus::Cancelled => InvoiceUpdatedPayload::Cancelled,
            CkbInvoiceStatus::Expired => InvoiceUpdatedPayload::Expired,
            CkbInvoiceStatus::Received => {
                return Err("InvoiceUpdatedPayload::Received requires extra data".to_string())
            }
            CkbInvoiceStatus::Paid => InvoiceUpdatedPayload::Paid,
        };
        Ok(payload)
    }
}

impl InvoiceUpdatedEvent {
    pub fn new(invoice_hash: Hash256, payload: InvoiceUpdatedPayload) -> Self {
        Self {
            invoice_hash,
            payload,
        }
    }
}

// but with additional information for downstream services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaymentUpdatedPayload {
    /// initial status, payment session is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// related HTLC is successfully settled
    Success { preimage: Hash256 },
    /// related HTLC is failed
    Failed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentUpdatedEvent {
    pub payment_hash: Hash256,
    pub payload: PaymentUpdatedPayload,
}

impl TryFrom<PaymentStatus> for PaymentUpdatedPayload {
    type Error = String;

    fn try_from(value: PaymentStatus) -> Result<Self, Self::Error> {
        let payload = match value {
            PaymentStatus::Created => PaymentUpdatedPayload::Created,
            PaymentStatus::Inflight => PaymentUpdatedPayload::Inflight,
            PaymentStatus::Success => {
                return Err("PaymentUpdatedPayload::Created requires extra data".to_string())
            }
            PaymentStatus::Failed => PaymentUpdatedPayload::Failed,
        };
        Ok(payload)
    }
}

impl PaymentUpdatedEvent {
    pub fn new(payment_hash: Hash256, payload: PaymentUpdatedPayload) -> Self {
        Self {
            payment_hash,
            payload,
        }
    }
}

/// Message sent from Store to publisher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreUpdatedEvent {
    InvoiceUpdated(InvoiceUpdatedEvent),
    PaymentUpdated(PaymentUpdatedEvent),
}

impl StoreUpdatedEvent {
    pub fn new_invoice_updated_event(
        invoice_hash: Hash256,
        payload: InvoiceUpdatedPayload,
    ) -> Self {
        StoreUpdatedEvent::InvoiceUpdated(InvoiceUpdatedEvent::new(invoice_hash, payload))
    }
    pub fn new_payment_updated_event(
        payment_hash: Hash256,
        payload: PaymentUpdatedPayload,
    ) -> Self {
        StoreUpdatedEvent::PaymentUpdated(PaymentUpdatedEvent::new(payment_hash, payload))
    }
}

/// This ractor receives the notification StoreUpdateMessage and sends out StoreRichUpdateMessage
#[derive(Default, Clone, Debug)]
pub struct StorePublisher(Arc<OutputPort<StoreUpdatedEvent>>);

impl StorePublisher {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn publish(&self, event: StoreUpdatedEvent) {
        self.0.send(event);
    }

    pub fn subscribe(&self, subscriber: OutputPortSubscriber<StoreUpdatedEvent>) {
        subscriber.subscribe_to_port(&self.0);
    }
}
