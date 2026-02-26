//! Cross-chain hub (CCH) order status.

use crate::invoice::CkbInvoiceStatus;
use crate::payment::PaymentStatus;
use serde::{Deserialize, Serialize};

/// The status of a cross-chain hub order.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and waiting for the incoming invoice to collect enough TLCs.
    Pending = 0,
    /// The incoming invoice collected the required TLCs.
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
