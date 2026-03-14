//! Fee and forwarding event types for the Fiber Network JSON-RPC API.

use crate::schema_helpers::*;
use crate::serde_utils::{Hash256, U128Hex, U64Hex};
use ckb_jsonrpc_types::Script;
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

/// Parameters for the `forwarding_history` RPC method.
///
/// Queries individual forwarding events with time range and pagination.
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
    /// Number of events to skip (for pagination). Default is 0.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub offset: Option<u64>,
    /// Filter by UDT type script. If set, only events for this specific UDT are returned.
    /// Use `null` or omit to return events for all asset types.
    /// Use an explicit JSON `null` value with `ckb_only: true` to get only CKB events.
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
}
