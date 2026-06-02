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
///
/// The order tracks one BTC leg (always denominated in millisatoshi to avoid
/// rounding when computing fees and exchange rates) and one Fiber-side leg
/// (denominated in the smallest unit of the target Fiber asset — shannon for
/// native CKB or the UDT's smallest denomination for an allowlisted UDT).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,

    /// Identity of the Fiber-side asset for this order.
    /// `None` denotes native CKB (no UDT type script); `Some(script)` denotes
    /// a UDT identified by its full type script.
    pub fiber_type_script: Option<ckb_jsonrpc_types::Script>,

    pub outgoing_pay_req: String,
    /// Counterparty invoice the hub mints for the incoming payment. The order
    /// is only created once the counterparty leg has been priced and its
    /// invoice minted, so this is always present.
    pub incoming_invoice: CchInvoice,
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,

    /// Amount of the Lightning (BTC) leg invoice, in **millisatoshi** — the
    /// same unit a Bolt11 invoice carries (1 satoshi = 1000 millisatoshi). This
    /// **always equals that leg's invoice amount** (whether the leg is the
    /// submitted invoice or the counterparty invoice the hub mints — see
    /// `fiber_invoice_amount` for the Fiber-leg counterpart), and what that
    /// amount represents depends on direction:
    /// - **SendBTC**: the Lightning leg is the outgoing Bolt11 the hub pays, so
    ///   this is the submitted Bolt11 amount and is fee-**exclusive** (the hub
    ///   collects `btc_fee_msat` on the Fiber/incoming leg instead).
    /// - **ReceiveBTC**: the Lightning leg is the hold invoice the hub mints, so
    ///   this is the minted invoice amount and is fee-**inclusive** (the payer
    ///   pays principal + `btc_fee_msat`).
    #[serde_as(as = "U128Hex")]
    pub lightning_invoice_amount: u128,
    /// The hub's fee for this order, in millisatoshi.
    ///
    /// The fee is always BTC-denominated and computed against the
    /// fee-**exclusive** Lightning amount as
    /// `exclusive * fee_rate_per_million_sats / 1_000_000 + base_fee_sats * 1000`.
    /// Because `lightning_invoice_amount` is fee-inclusive only for `ReceiveBTC`,
    /// the two directions relate the two fields differently:
    /// - **SendBTC**: `lightning_invoice_amount` is already the fee-exclusive
    ///   amount, so the Fiber (incoming) leg is priced to cover
    ///   `lightning_invoice_amount + btc_fee_msat`.
    /// - **ReceiveBTC**: `lightning_invoice_amount` is fee-inclusive, so the fee
    ///   is recovered from it by subtracting `base_fee_sats * 1000` first, then
    ///   dividing by `1 + fee_rate_per_million_sats / 1_000_000`; the
    ///   fee-exclusive principal is `lightning_invoice_amount - btc_fee_msat`.
    #[serde_as(as = "U128Hex")]
    pub btc_fee_msat: u128,
    /// Amount of the Fiber leg invoice, in the **smallest unit** of
    /// `fiber_type_script` — the same unit a Fiber invoice carries (shannon for
    /// native CKB, the UDT's smallest unit otherwise). This **always equals that
    /// leg's invoice amount** (whether the leg is the submitted invoice —
    /// `ReceiveBTC` — or the counterparty invoice the hub mints — `SendBTC`).
    #[serde_as(as = "U128Hex")]
    pub fiber_invoice_amount: u128,

    pub status: CchOrderStatus,

    pub failure_reason: Option<String>,
}

/// Direction of a swap proposal sent to the operator acceptor.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum SwapDirection {
    /// Swap client submitted a Bolt11 invoice; hub will pay Lightning,
    /// client pays the Fiber leg.
    SendBTC,
    /// Swap client submitted a Fiber invoice; hub will pay the Fiber leg,
    /// client pays Lightning.
    ReceiveBTC,
}

/// Notification pushed to operator clients subscribed via
/// `subscribe_swap_proposals` when a swap whose Fiber leg is allowlisted
/// but **not** in the fixed-rate list arrives at the hub. The operator
/// must answer with a [`SwapProposalResponse`] (via
/// `submit_swap_proposal_response`) before the configured timeout, or the
/// proposal is rejected automatically.
///
/// This is also the record the hub **persists** while the proposal awaits an
/// operator decision. It is stored in its own table (keyed by `payment_hash`)
/// separately from [`CchOrder`], because no order exists yet: the order is
/// materialised (as [`CchOrderStatus::Pending`]) only once the operator accepts
/// the proposal, at which point this record is deleted. On rejection or timeout
/// the record is removed and no order is ever created. On restart it is
/// re-broadcast and its timeout re-armed from `expires_at`. Everything needed
/// to build the resulting order on accept is read from these fields; the
/// order's own relative expiry is taken from the hub config at accept time
/// rather than persisted here.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapProposal {
    /// Opaque id; the operator MUST echo this in their `SwapProposalResponse`.
    pub proposal_id: Hash256,
    /// Hub-internal id of the underlying CCH order. For now this equals
    /// `payment_hash`; kept as a separate field so the spec's contract is
    /// honoured even if the two diverge later.
    pub order_id: Hash256,
    /// Whether the Fiber leg is incoming (`SendBTC`) or outgoing
    /// (`ReceiveBTC`) from the hub's perspective.
    pub direction: SwapDirection,
    /// Payment hash that links both legs.
    pub payment_hash: Hash256,
    /// UDT type script when the Fiber leg is a UDT; absent for native CKB.
    pub fiber_asset: Option<ckb_jsonrpc_types::Script>,
    /// Fiber-leg amount in the asset's smallest unit when known up-front
    /// (parsed from the submitted Fiber invoice on `ReceiveBTC`); absent on
    /// `SendBTC` because the operator supplies it in their response. Same unit
    /// as the Fiber invoice (shannon for native CKB, UDT smallest unit
    /// otherwise).
    #[serde_as(as = "Option<U128Hex>")]
    pub fiber_invoice_amount: Option<u128>,
    /// Lightning amount in millisatoshi when known up-front (parsed from
    /// the submitted Bolt11 on `SendBTC`, the fee-EXCLUSIVE Bolt11 amount the
    /// hub will pay on Lightning); absent on `ReceiveBTC` because the operator
    /// supplies it in their response. The fee is carried separately in
    /// `fee_on_btc_side_msat`. Same unit as the Bolt11 invoice (millisatoshi).
    #[serde_as(as = "Option<U128Hex>")]
    pub lightning_invoice_amount: Option<u128>,
    /// Hub-configured proportional fee in effect for this swap.
    #[serde_as(as = "U64Hex")]
    pub configured_fee_rate_per_million_sats: u64,
    /// Hub-configured flat base fee in effect for this swap, in satoshis.
    #[serde_as(as = "U64Hex")]
    pub configured_base_fee_sats: u64,
    /// Fee attributed to the BTC leg derived from the configured rate,
    /// in millisatoshi. `Some` on `SendBTC` (computed from the submitted
    /// Bolt11 amount); `None` on `ReceiveBTC`, where it cannot be computed
    /// up-front (it depends on the operator-set BTC-leg amount) — the operator
    /// MUST account for the configured rate/base when choosing the
    /// counterparty amount.
    #[serde_as(as = "Option<U128Hex>")]
    pub fee_on_btc_side_msat: Option<u128>,
    /// Encoded pay request the swap client supplied, for operator review.
    pub submitted_invoice: String,
    /// Wall-clock seconds since UNIX epoch when this proposal will be
    /// auto-rejected if no response has been received.
    #[serde_as(as = "U64Hex")]
    pub expires_at: u64,
    /// Wall-clock seconds since UNIX epoch when this proposal was built.
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
}

/// Result of a `send_btc` / `receive_btc` request.
///
/// A fixed-rate (fast-path) swap yields a persisted [`CchOrder`] immediately.
/// A non-fixed-rate swap instead enters the operator-proposal flow: no order
/// exists yet, so the hub returns the [`SwapProposal`] it is broadcasting to
/// operators. The order is created (and observable via `get_cch_order`) only
/// once an operator accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NewOrderResult {
    /// Fast path: the order was created and is already `Pending`.
    Order(CchOrder),
    /// Proposal path: no order exists yet; the swap awaits an operator
    /// decision on this proposal.
    PendingProposal(SwapProposal),
}

/// Operator's decision for a [`SwapProposal`]. Submitted via the
/// `submit_swap_proposal_response` RPC method (separate from the
/// subscription channel because jsonrpsee subscriptions are unidirectional).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapProposalResponse {
    /// Must match a pending proposal previously notified to this client.
    pub proposal_id: Hash256,
    /// `true` to accept the swap, `false` to reject.
    pub accept: bool,
    /// REQUIRED when `accept` is `true`. Smallest-unit integer in the
    /// counterparty leg's asset:
    ///
    /// * On `SendBTC`, this is the **Fiber-leg** amount in smallest units.
    /// * On `ReceiveBTC`, this is the **BTC-leg** amount in millisatoshi.
    #[serde_as(as = "Option<U128Hex>")]
    pub counterparty_leg_amount: Option<u128>,
    /// Optional human-readable reason; logged by the hub and returned to
    /// the swap client when `accept` is `false`.
    pub reject_reason: Option<String>,
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
        let Some(expiry_time) = self.created_at.checked_add(self.expiry_delta_seconds) else {
            self.status = CchOrderStatus::Failed;
            self.failure_reason = Some("Order expiry time overflows".to_string());
            return true;
        };
        if expiry_time < current_time {
            self.status = CchOrderStatus::Failed;
            self.failure_reason = Some("Order expired on startup".to_string());
            true
        } else {
            false
        }
    }
}
