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

impl CchOrder {
    /// Return the amount required by the incoming invoice, in satoshis.
    ///
    /// The persisted `amount_sats` field is used as a compatibility fallback for invoices that do
    /// not encode an amount. Older ReceiveBTC orders stored the outgoing Fiber principal in that
    /// field, so deriving the value from the incoming invoice also repairs their public amount
    /// semantics without changing the serialized order format.
    pub fn required_incoming_amount_sats(&self) -> u128 {
        match &self.incoming_invoice {
            CchInvoice::Fiber(invoice) => invoice.amount(),
            CchInvoice::Lightning(invoice) => invoice
                .amount_milli_satoshis()
                .map(u128::from)
                .map(|amount_msat| amount_msat.div_ceil(1_000)),
        }
        .unwrap_or(self.amount_sats)
    }

    /// Refresh `amount_sats` from the amount encoded in the incoming invoice when available.
    pub fn normalize_amount_sats(&mut self) {
        self.amount_sats = self.required_incoming_amount_sats();
    }

    pub fn is_final(&self) -> bool {
        self.status == CchOrderStatus::Success || self.status == CchOrderStatus::Failed
    }

    /// Check if the order is expired before the outgoing leg starts, and mark it as Failed.
    ///
    /// `expiry_delta_seconds` bounds how long a CCH order can wait for the incoming payment.
    /// Once the incoming payment is accepted, the order is governed by payment tracking and the
    /// incoming HTLC/TLC expiry budget; timing it out here could cancel funds after the outgoing
    /// leg has already obtained a preimage.
    pub fn update_if_expired(&mut self, current_time: u64) -> bool {
        self.update_if_expired_with_reason(current_time, "Order expired on startup")
    }

    /// Check if the order is expired before the outgoing leg starts, and mark it as Failed with
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
