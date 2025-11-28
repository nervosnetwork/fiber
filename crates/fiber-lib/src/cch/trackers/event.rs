use crate::{
    fiber::{payment::PaymentStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

#[derive(Debug, Clone)]
pub enum CchTrackingEvent {
    InvoiceChanged {
        /// The payment hash of the invoice.
        payment_hash: Hash256,
        status: CkbInvoiceStatus,
    },

    PaymentChanged {
        /// The payment hash of the invoice.
        payment_hash: Hash256,
        /// The preimage of the invoice.
        payment_preimage: Option<Hash256>,
        status: PaymentStatus,
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
