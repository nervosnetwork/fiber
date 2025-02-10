//! Subscription interfaces for store updates. Sometimes we want to subscribe to store updates to run our own logic.
//! This mod contains the traits definition of the subscription interfaces. The mod subscription_impl contains an
//! implementation that sends out payment/invoice update events to subscribers.

use ractor::{async_trait, DerivedActorRef};

use crate::{
    fiber::{graph::PaymentSessionStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

// The state of an invoice. Basically the same as CkbInvoiceStatus,
// but with additional information for downstream services.
#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct InvoiceUpdate {
    pub hash: Hash256,
    pub state: InvoiceState,
}

// The state of a payment session. Basically the same as PaymentSessionStatus,
// but with additional information for downstream services.
#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct PaymentUpdate {
    pub hash: Hash256,
    pub state: PaymentState,
}

pub trait StoreUpdateSubscription: InvoiceSubscription + PaymentSubscription {}

#[async_trait]
pub trait InvoiceSubscription: Send + Clone {
    type Subscription;
    type Error: std::error::Error;

    async fn subscribe_invoice(
        &self,
        invoice_hash: Hash256,
        receiver: DerivedActorRef<InvoiceUpdate>,
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
        receiver: DerivedActorRef<PaymentUpdate>,
    ) -> Result<Self::Subscription, Self::Error>;

    async fn unsubscribe_payment(
        &self,
        subscription: Self::Subscription,
    ) -> Result<(), Self::Error>;
}
