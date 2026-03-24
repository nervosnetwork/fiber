//! Fee and forwarding event types for the Fiber Network JSON-RPC API.

use crate::schema_helpers::*;
use crate::serde_utils::{Hash256, U128Hex, U64Hex};
use ckb_jsonrpc_types::{JsonBytes, Script};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Fee summary for a single asset type (CKB or a specific UDT).
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct AssetFeeReport {
    /// The UDT type script. `None` means native CKB.
    pub udt_type_script: Option<Script>,
    /// Total fees earned in the last 24 hours.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub daily_fee_sum: u128,
    /// Total fees earned in the last 7 days.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub weekly_fee_sum: u128,
    /// Total fees earned in the last 30 days.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub monthly_fee_sum: u128,
    /// Total number of forwarding events in the last 24 hours.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub daily_event_count: u64,
    /// Total number of forwarding events in the last 7 days.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub weekly_event_count: u64,
    /// Total number of forwarding events in the last 30 days.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub monthly_event_count: u64,
}

/// Result of the `fee_report` RPC method.
///
/// Returns aggregated forwarding fee sums over different time windows,
/// grouped by asset type (CKB and each UDT).
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct FeeReportResult {
    /// Fee reports grouped by asset type.
    /// Each entry corresponds to a different token (CKB or a UDT).
    pub asset_reports: Vec<AssetFeeReport>,
}

/// Parameters for the `fee_report` RPC method.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default, JsonSchema)]
pub struct FeeReportParams {
    /// Number of days to include in the report (starting from now).
    /// Default is 30. This is used when start_time is not specified.
    /// Mutually exclusive with start_time/end_time.
    pub days: Option<u64>,
    /// Start time in milliseconds since UNIX epoch (inclusive).
    /// Mutually exclusive with days.
    /// Default is (end_time - days * 24h) or 30 days ago if end_time is also not specified.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub start_time: Option<u64>,
    /// End time in milliseconds since UNIX epoch (inclusive).
    /// Default is current time.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub end_time: Option<u64>,
}

/// Parameters for the `forwarding_history` RPC method.
///
/// Queries individual forwarding events with time range and pagination.
/// Uses cursor-based pagination: pass `last_cursor` from a previous response
/// as `after` to fetch the next page.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default, JsonSchema)]
pub struct ForwardingHistoryParams {
    /// Start time in milliseconds since UNIX epoch (inclusive). Default is 0 (the beginning of time).
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub start_time: Option<u64>,
    /// End time in milliseconds since UNIX epoch (inclusive). Default is the current time.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub end_time: Option<u64>,
    /// Maximum number of events to return. Default is 100.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub limit: Option<u64>,
    /// Opaque cursor for pagination. Pass the `last_cursor` value from a previous
    /// response to retrieve the next page of results. Omit or set to `null` to
    /// start from the beginning.
    pub after: Option<JsonBytes>,
    /// Filter by UDT type script. If set, only events for this specific UDT are returned.
    /// Use `null` or omit to return events for all asset types.
    pub udt_type_script: Option<Script>,
}

/// A single forwarding event as returned by the `forwarding_history` RPC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct ForwardingEventInfo {
    /// Timestamp when this event was recorded, in milliseconds since UNIX epoch.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub timestamp: u64,
    /// The channel ID through which the inbound TLC arrived.
    pub incoming_channel_id: Hash256,
    /// The channel ID through which the outbound TLC was sent.
    pub outgoing_channel_id: Hash256,
    /// The amount received on the incoming channel.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub incoming_amount: u128,
    /// The amount forwarded on the outgoing channel.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub outgoing_amount: u128,
    /// The fee earned for this forwarding event.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub fee: u128,
    /// The payment hash associated with this forwarded TLC.
    pub payment_hash: Hash256,
    /// The UDT type script. `None` means native CKB.
    pub udt_type_script: Option<Script>,
}

/// Result of the `forwarding_history` RPC method.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct ForwardingHistoryResult {
    /// The list of forwarding events.
    pub events: Vec<ForwardingEventInfo>,
    /// The total number of forwarding events returned in this result.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub total_count: u64,
    /// Opaque cursor pointing past the last event in this page.
    /// Pass this value as `after` in the next request to fetch the next page.
    /// `null` means there are no more results.
    pub last_cursor: Option<JsonBytes>,
}

/// Payment amount summary for a single asset type (CKB or a specific UDT).
///
/// Used by both `sent_payment_report` and `received_payment_report` RPCs.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct AssetPaymentReport {
    /// The UDT type script. `None` means native CKB.
    pub udt_type_script: Option<Script>,
    /// Total amount in the last 24 hours.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub daily_amount_sum: u128,
    /// Total amount in the last 7 days.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub weekly_amount_sum: u128,
    /// Total amount in the last 30 days.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub monthly_amount_sum: u128,
    /// Total number of payment events in the last 24 hours.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub daily_event_count: u64,
    /// Total number of payment events in the last 7 days.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub weekly_event_count: u64,
    /// Total number of payment events in the last 30 days.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub monthly_event_count: u64,
}

/// Result of the `sent_report` RPC method.
///
/// Returns aggregated sent payment amounts over different time windows,
/// grouped by asset type (CKB and each UDT).
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SentPaymentReportResult {
    /// Payment reports grouped by asset type.
    pub asset_reports: Vec<AssetPaymentReport>,
}

/// Result of the `received_report` RPC method.
///
/// Returns aggregated received payment amounts over different time windows,
/// grouped by asset type (CKB and each UDT).
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct ReceivedPaymentReportResult {
    /// Payment reports grouped by asset type.
    pub asset_reports: Vec<AssetPaymentReport>,
}

/// Parameters for the `payment_history` RPC method.
///
/// Queries individual send/receive payment events with time range and pagination.
/// Uses cursor-based pagination: pass `last_cursor` from a previous response
/// as `after` to fetch the next page.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default, JsonSchema)]
pub struct PaymentHistoryParams {
    /// Start time in milliseconds since UNIX epoch (inclusive). Default is 0.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub start_time: Option<u64>,
    /// End time in milliseconds since UNIX epoch (inclusive). Default is the current time.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub end_time: Option<u64>,
    /// Maximum number of events to return. Default is 100.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub limit: Option<u64>,
    /// Opaque cursor for pagination. Pass the `last_cursor` value from a previous
    /// response to retrieve the next page of results. Omit or set to `null` to
    /// start from the beginning.
    pub after: Option<JsonBytes>,
    /// Filter by UDT type script.
    pub udt_type_script: Option<Script>,
}

/// A single payment event as returned by the `payment_history` RPC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct PaymentEventInfo {
    /// The type of this event: "Send" or "Receive".
    pub event_type: String,
    /// Timestamp when this event was recorded, in milliseconds since UNIX epoch.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub timestamp: u64,
    /// The channel ID through which the TLC was sent or received.
    pub channel_id: Hash256,
    /// The payment amount.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// The routing fee (only meaningful for Send events; 0 for Receive).
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub fee: u128,
    /// The payment hash associated with this TLC.
    pub payment_hash: Hash256,
    /// The UDT type script. `None` means native CKB.
    pub udt_type_script: Option<Script>,
}

/// Result of the `payment_history` RPC method.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct PaymentHistoryResult {
    /// The list of payment events.
    pub events: Vec<PaymentEventInfo>,
    /// The total number of events returned in this result.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub total_count: u64,
    /// Opaque cursor pointing past the last event in this page.
    /// Pass this value as `after` in the next request to fetch the next page.
    /// `null` means there are no more results.
    pub last_cursor: Option<JsonBytes>,
}
