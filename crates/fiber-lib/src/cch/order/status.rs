use serde::{Deserialize, Serialize};

use crate::{fiber::payment::PaymentStatus, invoice::CkbInvoiceStatus};

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and waiting for the incoming invoice to collect enough TLCs.
    Pending = 0,
    /// The incoming invoice collected the required TLCs and is ready to send outgoing payment to obtain the preimage.
    IncomingAccepted = 1,
    /// The outgoing payment is in flight.
    OutgoingInFlight = 2,
    /// The outgoing payment is settled and preimage has been obtained.
    OutgoingSucceeded = 3,
    /// Both payments are settled and the order succeeds.
    Succeeded = 4,
    /// Order is failed.
    Failed = 5,
}

/// Cch uses invoice to trace the incoming half of the swap.
impl From<CkbInvoiceStatus> for CchOrderStatus {
    fn from(status: CkbInvoiceStatus) -> Self {
        match status {
            CkbInvoiceStatus::Open => CchOrderStatus::Pending,
            CkbInvoiceStatus::Cancelled => CchOrderStatus::Failed,
            CkbInvoiceStatus::Expired => CchOrderStatus::Failed,
            CkbInvoiceStatus::Received => CchOrderStatus::IncomingAccepted,
            CkbInvoiceStatus::Paid => CchOrderStatus::Succeeded,
        }
    }
}

/// Cch uses payment to trace the outgoing half of the swap.
impl From<PaymentStatus> for CchOrderStatus {
    fn from(status: PaymentStatus) -> Self {
        match status {
            PaymentStatus::Created => CchOrderStatus::IncomingAccepted,
            PaymentStatus::Inflight => CchOrderStatus::OutgoingInFlight,
            PaymentStatus::Success => CchOrderStatus::OutgoingSucceeded,
            PaymentStatus::Failed => CchOrderStatus::Failed,
        }
    }
}
