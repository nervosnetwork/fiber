use crate::invoice::CkbInvoiceStatus;
use fiber_types::{Hash256, PaymentStatus};
use std::fmt;

#[derive(Debug, Clone)]
pub enum CchTrackingEvent {
    InvoiceChanged {
        /// The payment hash of the invoice.
        payment_hash: Hash256,
        status: CkbInvoiceStatus,
        failure_reason: Option<String>,
    },

    PaymentChanged {
        /// The payment hash of the invoice.
        payment_hash: Hash256,
        /// The preimage of the invoice.
        payment_preimage: Option<Hash256>,
        status: PaymentStatus,
        failure_reason: Option<String>,
    },
}

impl CchTrackingEvent {
    pub fn payment_hash(&self) -> &Hash256 {
        match self {
            CchTrackingEvent::InvoiceChanged { payment_hash, .. } => payment_hash,
            CchTrackingEvent::PaymentChanged { payment_hash, .. } => payment_hash,
        }
    }
}

pub struct RedactedCchTrackingEvent<'a>(pub &'a CchTrackingEvent);

impl fmt::Debug for RedactedCchTrackingEvent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            CchTrackingEvent::InvoiceChanged {
                payment_hash,
                status,
                failure_reason,
            } => f
                .debug_struct("InvoiceChanged")
                .field("payment_hash", payment_hash)
                .field("status", status)
                .field("failure_reason", failure_reason)
                .finish(),
            CchTrackingEvent::PaymentChanged {
                payment_hash,
                payment_preimage,
                status,
                failure_reason,
            } => f
                .debug_struct("PaymentChanged")
                .field("payment_hash", payment_hash)
                .field("status", status)
                .field("failure_reason", failure_reason)
                .field("has_payment_preimage", &payment_preimage.is_some())
                .finish(),
        }
    }
}
