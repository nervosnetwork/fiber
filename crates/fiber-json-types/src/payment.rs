//! Payment-related types for the Fiber Network Node RPC API.

use crate::graph::RouterHop;
use crate::serde_utils::{EntityHex, SliceHex, U128Hex, U32Hex, U64Hex};
use crate::{Hash256, Pubkey};

use ckb_jsonrpc_types::Script;
use ckb_types::packed::OutPoint;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;

pub use fiber_types::payment::PaymentStatus;

// ============================================================
// RPC-specific payment types (different serde from fiber-types)
// ============================================================

/// A node in a payment session route.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// the public key of the node
    pub pubkey: Pubkey,
    /// the amount for this hop
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// the channel outpoint for this hop
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

/// A payment session route.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// the nodes in the route
    pub nodes: Vec<SessionRouteNode>,
}

/// A hop requirement for building a route.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopRequire {
    pub pubkey: Pubkey,
    #[serde_as(as = "Option<EntityHex>")]
    pub channel_outpoint: Option<OutPoint>,
}

/// A payment router containing a list of router hops.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentRouter {
    pub router_hops: Vec<RouterHop>,
}

// ============================================================
// RPC param/result types
// ============================================================

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct GetPaymentCommandParams {
    /// The payment hash of the payment to retrieve
    pub payment_hash: Hash256,
}

/// The result of a get_payment command.
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GetPaymentCommandResult {
    /// The payment hash of the payment
    pub payment_hash: Hash256,
    /// The status of the payment
    pub status: PaymentStatus,
    #[serde_as(as = "U64Hex")]
    /// The time the payment was created at, in milliseconds from UNIX epoch
    pub created_at: u64,
    #[serde_as(as = "U64Hex")]
    /// The time the payment was last updated at, in milliseconds from UNIX epoch
    pub last_updated_at: u64,
    /// The error message if the payment failed
    pub failed_error: Option<String>,
    /// fee paid for the payment
    #[serde_as(as = "U128Hex")]
    pub fee: u128,
    /// The custom records to be included in the payment.
    pub custom_records: Option<PaymentCustomRecords>,
    #[cfg(debug_assertions)]
    /// The router is a list of nodes that the payment will go through.
    pub routers: Vec<SessionRoute>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct ListPaymentsParams {
    /// Filter payments by status.
    pub status: Option<PaymentStatus>,
    /// The maximum number of payments to return. Default is 15.
    #[serde_as(as = "Option<U64Hex>")]
    pub limit: Option<u64>,
    /// The payment hash to start returning payments after (exclusive cursor for pagination).
    pub after: Option<Hash256>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ListPaymentsResult {
    /// The list of payments.
    pub payments: Vec<GetPaymentCommandResult>,
    /// The last cursor for pagination.
    pub last_cursor: Option<Hash256>,
}

/// The custom records to be included in the payment (RPC version with hex encoding).
///
/// Note: This has different serde than `fiber_types::payment::PaymentCustomRecords`
/// which uses plain serde (bincode-compatible). This version uses `#[serde(flatten)]`
/// and hex-encoded keys/values for JSON compatibility.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    #[serde(flatten)]
    #[serde_as(as = "HashMap<U32Hex, SliceHex>")]
    pub data: HashMap<u32, Vec<u8>>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentCommandParams {
    /// the identifier of the payment target
    pub target_pubkey: Option<Pubkey>,
    /// the amount of the payment
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,
    /// the hash to use within the payment's HTLC
    pub payment_hash: Option<Hash256>,
    /// the TLC expiry delta for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,
    /// the TLC expiry limit for the whole payment, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_limit: Option<u64>,
    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,
    /// the payment timeout in seconds
    #[serde_as(as = "Option<U64Hex>")]
    pub timeout: Option<u64>,
    /// the maximum fee amounts in shannons
    #[serde_as(as = "Option<U128Hex>")]
    pub max_fee_amount: Option<u128>,
    /// the maximum fee rate per thousand
    #[serde_as(as = "Option<U64Hex>")]
    pub max_fee_rate: Option<u64>,
    /// max parts for the payment
    #[serde_as(as = "Option<U64Hex>")]
    pub max_parts: Option<u64>,
    /// Optional explicit trampoline hops.
    pub trampoline_hops: Option<Vec<Pubkey>>,
    /// keysend payment
    pub keysend: Option<bool>,
    /// udt type script for the payment
    pub udt_type_script: Option<Script>,
    /// allow self payment
    pub allow_self_payment: Option<bool>,
    /// Some custom records for the payment
    pub custom_records: Option<PaymentCustomRecords>,
    /// Optional route hints to reach the destination through private channels.
    pub hop_hints: Option<Vec<HopHint>>,
    /// dry_run for payment
    pub dry_run: Option<bool>,
}

/// A hop hint is a hint for a node to use a specific channel.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopHint {
    /// The public key of the node
    pub pubkey: Pubkey,
    /// The outpoint of the channel
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The fee rate to use this hop to forward the payment.
    #[serde_as(as = "U64Hex")]
    pub fee_rate: u64,
    /// The TLC expiry delta to use this hop to forward the payment.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRouterParams {
    /// the amount of the payment
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,
    /// udt type script for the payment router
    pub udt_type_script: Option<Script>,
    /// A list of hops that defines the route.
    pub hops_info: Vec<HopRequire>,
    /// the TLC expiry delta for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,
}

/// The router returned by build_router
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BuildPaymentRouterResult {
    /// The hops information for router
    pub router_hops: Vec<RouterHop>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct SendPaymentWithRouterParams {
    /// the hash to use within the payment's HTLC
    pub payment_hash: Option<Hash256>,
    /// The router to use for the payment
    pub router: Vec<RouterHop>,
    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,
    /// Some custom records for the payment
    pub custom_records: Option<PaymentCustomRecords>,
    /// keysend payment
    pub keysend: Option<bool>,
    /// udt type script for the payment
    pub udt_type_script: Option<Script>,
    /// dry_run for payment
    pub dry_run: Option<bool>,
}
