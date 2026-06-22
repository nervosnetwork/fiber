//! Cross-chain hub types for the Fiber Network JSON-RPC API.

use crate::invoice::Currency;
use crate::schema_helpers::*;
use crate::serde_utils::{Hash256, U128Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub enum CchOrderStatus {
    /// Order is created and waiting for the incoming invoice to collect enough TLCs.
    Pending,
    /// The incoming invoice collected the required TLCs and is ready to send outgoing payment to obtain the preimage.
    IncomingAccepted,
    /// The outgoing payment is in flight.
    OutgoingInFlight,
    /// The outgoing payment is settled and preimage has been obtained.
    OutgoingSuccess,
    /// Both payments are settled and the order succeeds.
    Success,
    /// Order is failed.
    Failed,
}

/// The generated proxy invoice for the incoming payment.
///
/// The JSON representation:
///
/// ```text
/// { "Fiber": String } | { "Lightning": String }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum CchInvoice {
    /// Fiber invoice string
    Fiber(String),
    /// Lightning invoice string
    Lightning(String),
}

/// Parameters for sending BTC via cross-chain hub.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SendBTCParams {
    /// Payment request string for the BTC Lightning payee.
    pub btc_pay_req: String,
    /// Request currency
    pub currency: Currency,
    /// Identity of the Fiber-side asset to use for this swap. `null` denotes
    /// native CKB; otherwise the full UDT type script identifies the asset.
    /// Must appear in the hub's `fiber_asset_allowlist`.
    #[serde(default)]
    pub fiber_type_script: Option<Script>,
}

/// Cross-chain hub order response.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CchOrderResponse {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub timestamp: u64,
    /// Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expiry_delta_seconds: u64,

    /// Identity of the Fiber-side asset for this order. `null` denotes native
    /// CKB; otherwise the full UDT type script identifies the asset.
    pub fiber_type_script: Option<Script>,

    /// Generated invoice for the incoming payment.
    pub incoming_invoice: CchInvoice,
    /// The final payee to accept the payment. It has the different network with incoming invoice.
    pub outgoing_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
    /// Amount of the Lightning (BTC) leg invoice, in **millisatoshi** (the same
    /// unit a Bolt11 invoice carries; 1 satoshi = 1000 millisatoshi). Always
    /// equals that leg's invoice amount: fee-exclusive on `SendBTC` (the
    /// submitted Bolt11), fee-inclusive on `ReceiveBTC` (the minted hold
    /// invoice).
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub lightning_invoice_amount: u128,
    /// Hub fee for this order, in millisatoshi.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub btc_fee_msat: u128,
    /// Amount of the Fiber leg invoice, in the **smallest unit** of
    /// `fiber_type_script` (shannon for native CKB, the UDT's smallest unit
    /// otherwise; the same unit a Fiber invoice carries). Always equals that
    /// leg's invoice amount.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub fiber_invoice_amount: u128,
    /// Order status
    pub status: CchOrderStatus,
}

/// Result of `send_btc` / `receive_btc`.
///
/// A fixed-rate (fast-path) swap returns the created order directly. A
/// non-fixed-rate swap instead enters the operator-proposal flow: no order
/// exists yet, so the hub returns the swap proposal it is broadcasting to
/// operators. The order is created (and observable via `get_cch_order`) only
/// once an operator accepts.
///
/// The JSON representation is an externally-tagged enum:
///
/// ```text
/// { "Order": { ... } } | { "PendingProposal": { ... } }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum CchNewOrderResponse {
    /// Fast path: the order was created and is already `Pending`.
    Order(CchOrderResponse),
    /// Proposal path: no order exists yet; the swap awaits an operator
    /// decision on this proposal.
    PendingProposal(SwapProposal),
}

/// Parameters for receiving BTC via cross-chain hub.
#[serde_as]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReceiveBTCParams {
    /// Payment request string for the CKB Fiber payee.
    pub fiber_pay_req: String,
}

/// Parameters for getting a CCH order.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
}

/// Direction of a swap proposal sent to the operator acceptor.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
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
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    pub fiber_asset: Option<Script>,
    /// Fiber-leg amount in the asset's smallest unit when known up-front
    /// (parsed from the submitted Fiber invoice on `ReceiveBTC`); absent on
    /// `SendBTC` because the operator supplies it in their response. Same unit
    /// as the Fiber invoice (shannon for native CKB, UDT smallest unit
    /// otherwise).
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub fiber_invoice_amount: Option<u128>,
    /// Lightning amount in millisatoshi when known up-front (parsed from
    /// the submitted Bolt11 on `SendBTC`, fee-exclusive); absent on `ReceiveBTC`
    /// because the operator supplies it in their response. Same unit as the
    /// Bolt11 invoice (millisatoshi).
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub lightning_invoice_amount: Option<u128>,
    /// Hub-configured proportional fee in effect for this swap.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub configured_fee_rate_per_million_sats: u64,
    /// Hub-configured flat base fee in effect for this swap, in satoshis.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub configured_base_fee_sats: u64,
    /// Fee attributed to the BTC leg derived from the configured rate,
    /// in millisatoshi. `Some` on `SendBTC` (computed from the submitted
    /// Bolt11 amount); `null` on `ReceiveBTC`, where it cannot be computed
    /// up-front (it depends on the operator-set BTC-leg amount) — the operator
    /// MUST account for the configured rate/base when choosing the
    /// counterparty amount.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub fee_on_btc_side_msat: Option<u128>,
    /// Encoded pay request the swap client supplied, for operator review.
    pub submitted_invoice: String,
    /// Wall-clock seconds since UNIX epoch when this proposal will be
    /// auto-rejected if no response has been received.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_at: u64,
    /// Wall-clock seconds since UNIX epoch when this proposal was built.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
}

/// Parameters for [`submit_swap_proposal_response`]: the operator's
/// decision for a [`SwapProposal`] previously delivered via the
/// `subscribe_swap_proposals` subscription. The method is separate from
/// the subscription channel because jsonrpsee subscriptions are
/// unidirectional.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubmitSwapProposalResponseParams {
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
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub counterparty_leg_amount: Option<u128>,
    /// Optional human-readable reason; logged by the hub and returned to
    /// the swap client when `accept` is `false`.
    pub reject_reason: Option<String>,
}
