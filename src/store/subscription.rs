use ractor::{async_trait, ActorRef};
use thiserror::Error;

use crate::{
    fiber::{graph::PaymentSessionStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

use super::Store;

pub(crate) struct SubscriptionImpl {
    store: Store,
}

impl SubscriptionImpl {
    pub fn new(store: Store) -> Self {
        SubscriptionImpl { store }
    }
}

impl SubscriptionImpl {
    pub async fn subscribe_invoice(&self, invoice_hash: Hash256) {
        unimplemented!()
    }

    pub async fn unsubscribe_invoice(&self, invoice_hash: Hash256) {
        unimplemented!()
    }

    pub async fn subscribe_payment(&self, payment_hash: Hash256) {
        unimplemented!()
    }

    pub async fn unsubscribe_payment(&self, payment_hash: Hash256) {
        unimplemented!()
    }
}

#[derive(Error, Debug)]
pub enum SubscriptionError {}

// The state of an invoice. Basically the same as CkbInvoiceStatus,
// but with additional
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

pub struct InvoiceUpdate {
    hash: Hash256,
    state: InvoiceState,
}

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

pub struct PaymentUpdate {
    hash: Hash256,
    state: PaymentState,
}

pub trait FiberSubscription: InvoiceSubscription + PaymentSubscription {}

#[async_trait]
pub trait InvoiceSubscription {
    type Error: std::error::Error;

    async fn subscribe_invoice(
        &self,
        invoice_hash: Hash256,
        receiver: ActorRef<InvoiceUpdate>,
    ) -> Result<(), Self::Error>;

    async fn unsubscribe_invoice(&self, invoice_hash: Hash256) -> Result<(), Self::Error>;
}

#[async_trait]
pub trait PaymentSubscription {
    type Error: std::error::Error;

    async fn subscribe_payment(
        &self,
        payment_hash: Hash256,
        receiver: ActorRef<PaymentUpdate>,
    ) -> Result<(), Self::Error>;

    async fn unsubscribe_payment(&self, payment_hash: Hash256) -> Result<(), Self::Error>;
}

#[async_trait]
impl InvoiceSubscription for SubscriptionImpl {
    type Error = SubscriptionError;

    async fn subscribe_invoice(
        &self,
        invoice_hash: Hash256,
        receiver: ActorRef<InvoiceUpdate>,
    ) -> Result<(), Self::Error> {
        unimplemented!()
    }

    async fn unsubscribe_invoice(&self, invoice_hash: Hash256) -> Result<(), Self::Error> {
        unimplemented!()
    }
}

#[async_trait]
impl PaymentSubscription for SubscriptionImpl {
    type Error = SubscriptionError;

    async fn subscribe_payment(
        &self,
        payment_hash: Hash256,
        receiver: ActorRef<PaymentUpdate>,
    ) -> Result<(), Self::Error> {
        unimplemented!()
    }

    async fn unsubscribe_payment(&self, payment_hash: Hash256) -> Result<(), Self::Error> {
        unimplemented!()
    }
}

pub(crate) trait OnInvoiceUpdated {
    fn on_invoice_updated(&self, invoice_hash: Hash256, status: CkbInvoiceStatus);
}

pub(crate) trait OnPaymentUpdated {
    fn on_payment_updated(&self, payment_hash: Hash256, status: PaymentSessionStatus);
}

impl OnInvoiceUpdated for SubscriptionImpl {
    fn on_invoice_updated(&self, invoice_hash: Hash256, status: CkbInvoiceStatus) {
        unimplemented!()
    }
}

impl OnPaymentUpdated for SubscriptionImpl {
    fn on_payment_updated(&self, payment_hash: Hash256, status: PaymentSessionStatus) {
        unimplemented!()
    }
}
