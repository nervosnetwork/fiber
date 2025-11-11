use lnd_grpc_tonic_client::lnrpc;

use crate::{cch::CchOrderStatus, fiber::types::Hash256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CchIncomingPaymentStatus {
    // The incoming payment is in-flight
    InFlight = 0,
    // Incoming payment TLCs have been accepted
    Accepted = 1,
    Settled = 2,
    Failed = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CchOutgoingPaymentStatus {
    // The outgoing payment is in-flight
    InFlight = 0,
    Settled = 2,
    Failed = 3,
}

impl From<CchIncomingPaymentStatus> for CchOrderStatus {
    fn from(status: CchIncomingPaymentStatus) -> Self {
        match status {
            CchIncomingPaymentStatus::InFlight => CchOrderStatus::Pending,
            CchIncomingPaymentStatus::Accepted => CchOrderStatus::IncomingAccepted,
            CchIncomingPaymentStatus::Settled => CchOrderStatus::Succeeded,
            CchIncomingPaymentStatus::Failed => CchOrderStatus::Failed,
        }
    }
}

impl From<CchOutgoingPaymentStatus> for CchOrderStatus {
    fn from(status: CchOutgoingPaymentStatus) -> Self {
        match status {
            CchOutgoingPaymentStatus::InFlight => CchOrderStatus::OutgoingInFlight,
            CchOutgoingPaymentStatus::Settled => CchOrderStatus::OutgoingSettled,
            CchOutgoingPaymentStatus::Failed => CchOrderStatus::Failed,
        }
    }
}

/// Lnd invoice is the incoming part of a CCHOrder to receive BTC from Lightning to Fiber
impl From<lnrpc::invoice::InvoiceState> for CchIncomingPaymentStatus {
    fn from(state: lnrpc::invoice::InvoiceState) -> Self {
        use lnrpc::invoice::InvoiceState;
        match state {
            InvoiceState::Open => CchIncomingPaymentStatus::InFlight,
            InvoiceState::Settled => CchIncomingPaymentStatus::Settled,
            InvoiceState::Canceled => CchIncomingPaymentStatus::Failed,
            InvoiceState::Accepted => CchIncomingPaymentStatus::Accepted,
        }
    }
}

/// Lnd payment is the outgoing part of a CCHOrder to send BTC from Fiber to Lightning
impl From<lnrpc::payment::PaymentStatus> for CchOutgoingPaymentStatus {
    fn from(status: lnrpc::payment::PaymentStatus) -> Self {
        use lnrpc::payment::PaymentStatus;
        match status {
            PaymentStatus::Unknown => CchOutgoingPaymentStatus::InFlight,
            PaymentStatus::InFlight => CchOutgoingPaymentStatus::InFlight,
            PaymentStatus::Succeeded => CchOutgoingPaymentStatus::Settled,
            PaymentStatus::Failed => CchOutgoingPaymentStatus::Failed,
            PaymentStatus::Initiated => CchOutgoingPaymentStatus::InFlight,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CchIncomingEvent {
    InvoiceChanged {
        /// The payment hash of the invoice.
        payment_hash: Hash256,
        /// The preimage of the invoice.
        payment_preimage: Option<Hash256>,
        status: CchIncomingPaymentStatus,
    },

    PaymentChanged {
        /// The payment hash of the invoice.
        payment_hash: Hash256,
        /// The preimage of the invoice.
        payment_preimage: Option<Hash256>,
        status: CchOutgoingPaymentStatus,
    },
}

impl CchIncomingEvent {
    pub fn payment_hash(&self) -> &Hash256 {
        match self {
            CchIncomingEvent::InvoiceChanged { payment_hash, .. } => payment_hash,
            CchIncomingEvent::PaymentChanged { payment_hash, .. } => payment_hash,
        }
    }
}
