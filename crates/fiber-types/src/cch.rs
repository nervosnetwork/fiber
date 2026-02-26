//! Cross-chain hub (CCH) types.

use crate::invoice::{CkbInvoice, CkbInvoiceStatus};
use crate::payment::PaymentStatus;
use crate::serde_utils::{U128Hex, U64Hex};
use crate::Hash256;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

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

// ============================================================
// CchInvoice
// ============================================================

/// The generated proxy invoice for the incoming payment.
///
/// The JSON representation:
///
/// ```text
/// { "Fiber": String } | { "Lightning": String }
/// ```
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CchInvoice {
    /// Fiber invoice that once paid, the hub will send the outgoing payment to Lightning
    Fiber(#[serde_as(as = "DisplayFromStr")] CkbInvoice),
    /// Lightning invoice that once paid, the hub will send the outgoing payment to Fiber
    Lightning(#[serde_as(as = "DisplayFromStr")] Bolt11Invoice),
}

// ============================================================
// CchOrder
// ============================================================

/// A cross-chain hub order.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,

    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub outgoing_pay_req: String,
    pub incoming_invoice: CchInvoice,
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,

    /// Amount required to pay in Satoshis via BTC or wrapped BTC, including the fee for the cross-chain hub
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,

    pub failure_reason: Option<String>,
}

impl CchOrder {
    pub fn is_final(&self) -> bool {
        self.status == CchOrderStatus::Succeeded || self.status == CchOrderStatus::Failed
    }

    /// Check if the order is expired given the current time, and mark it as Failed if expired.
    ///
    /// Returns `true` if the order was expired (and has been marked as Failed).
    /// Updates `status` to `Failed` and sets `failure_reason` when expired.
    pub fn update_if_expired(&mut self, current_time: u64) -> bool {
        let expiry_time = self.created_at + self.expiry_delta_seconds;
        if expiry_time < current_time {
            self.status = CchOrderStatus::Failed;
            self.failure_reason = Some("Order expired on startup".to_string());
            true
        } else {
            false
        }
    }
}
