//! Cross-chain hub (CCH) types.

use crate::invoice::{CkbInvoice, CkbInvoiceStatus};
use crate::payment::PaymentStatus;
use crate::serde_utils::{U128Hex, U64Hex};
use crate::Hash256;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum CchOrderStatus {
    /// Order is created and waiting for the incoming invoice to collect enough TLCs.
    Pending = 0,
    /// The incoming invoice collected the required TLCs and is ready to send outgoing payment to obtain the preimage.
    IncomingAccepted = 1,
    /// The outgoing payment is in flight.
    OutgoingInFlight = 2,
    /// The outgoing payment is settled and preimage has been obtained.
    OutgoingSuccess = 3,
    /// Both payments are settled and the order succeeds.
    Success = 4,
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
            CkbInvoiceStatus::Paid => CchOrderStatus::Success,
        }
    }
}

impl From<PaymentStatus> for CchOrderStatus {
    fn from(status: PaymentStatus) -> Self {
        match status {
            PaymentStatus::Created => CchOrderStatus::IncomingAccepted,
            PaymentStatus::Inflight => CchOrderStatus::OutgoingInFlight,
            PaymentStatus::Success => CchOrderStatus::OutgoingSuccess,
            PaymentStatus::Failed => CchOrderStatus::Failed,
        }
    }
}

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

/// Durable intent for creating the Lightning incoming invoice of a `receive_btc` order.
///
/// The intent is written before calling LND and atomically removed when the final
/// [`CchOrder`] is persisted. Its payment hash is the idempotency key used to recover
/// an invoice creation whose RPC result was lost or interrupted by a node restart.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchReceiveBtcOrderCreation {
    /// Seconds since epoch when order creation was accepted.
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Relative expiry time copied from the CCH configuration at acceptance time.
    #[serde_as(as = "U64Hex")]
    pub order_expiry_delta_seconds: u64,
    /// Original signed Fiber invoice. Exact equality is used to reject conflicting retries.
    pub fiber_pay_req: String,
    /// Idempotency key shared by the Fiber invoice, LND invoice, and final CCH order.
    pub payment_hash: Hash256,
    /// Outgoing Fiber principal in satoshis.
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// CCH fee in satoshis.
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// Wrapped BTC type script validated when the operation was accepted.
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,
    /// LND final-hop CLTV delta validated when the operation was accepted.
    #[serde_as(as = "U64Hex")]
    pub btc_final_tlc_expiry_delta_blocks: u64,
    /// Percentage of the collected fee available to the outgoing Fiber payment.
    #[serde_as(as = "U64Hex")]
    pub max_outgoing_fee_percentage: u64,
}

impl CchOrder {
    pub fn is_final(&self) -> bool {
        self.status == CchOrderStatus::Success || self.status == CchOrderStatus::Failed
    }

    /// Check if the order is expired given the current time, and mark it as Failed if expired.
    ///
    /// Returns `true` if the order was expired (and has been marked as Failed).
    /// Updates `status` to `Failed` and sets `failure_reason` when expired.
    pub fn update_if_expired(&mut self, current_time: u64) -> bool {
        self.update_if_expired_with_reason(current_time, "Order expired on startup")
    }

    /// Check if the order is expired given the current time, and mark it as Failed with
    /// `expired_reason` if expired.
    ///
    /// Returns `true` if the order was expired (and has been marked as Failed).
    /// Updates `status` to `Failed` and sets `failure_reason` when expired.
    pub fn update_if_expired_with_reason(
        &mut self,
        current_time: u64,
        expired_reason: &str,
    ) -> bool {
        if self.status != CchOrderStatus::Pending {
            return false;
        }

        let Some(expiry_time) = self.created_at.checked_add(self.expiry_delta_seconds) else {
            self.status = CchOrderStatus::Failed;
            self.failure_reason = Some("Order expiry time overflows".to_string());
            return true;
        };
        if expiry_time <= current_time {
            self.status = CchOrderStatus::Failed;
            self.failure_reason = Some(expired_reason.to_string());
            true
        } else {
            false
        }
    }
}
