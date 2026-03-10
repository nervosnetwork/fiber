//! Payment types for the Fiber Network JSON-RPC API.

use crate::serde_utils::{EntityHex, Hash256, Pubkey, SliceHex, U128Hex, U32Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use ckb_types::packed::OutPoint;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;

/// The status of a payment, will update as the payment progresses.
/// The transfer path for payment status is `Created -> Inflight -> Success | Failed`.
///
/// **MPP Behavior**: A single session may involve multiple attempts (HTLCs) to fulfill the total amount.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    /// Initial status. A payment session is created, but no HTLC has been dispatched.
    Created,
    /// The first hop AddTlc is sent successfully and waiting for the response.
    Inflight,
    /// The payment is finished. All related HTLCs are successfully settled.
    Success,
    /// The payment session has terminated.
    Failed,
}

/// Parameters for getting a payment.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct GetPaymentCommandParams {
    /// The payment hash of the payment to retrieve
    pub payment_hash: Hash256,
}

/// The node and channel information in a payment route hop.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// The public key of the node
    pub pubkey: Pubkey,
    /// The amount for this hop
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The channel outpoint for this hop
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

/// The router is a list of nodes that the payment will go through.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// The nodes in the route
    pub nodes: Vec<SessionRouteNode>,
}

/// The result of a get_payment command, which includes the payment hash, status, timestamps,
/// error message if failed, fee paid, and custom records.
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
    /// We store in the payment session and then will use it to track the payment history.
    /// If the payment adapted MPP (multi-part payment), the routers will be a list of nodes.
    /// For example:
    ///    `A(amount, channel) -> B -> C -> D`
    /// means A will send `amount` with `channel` to B.
    pub routers: Vec<SessionRoute>,
}

/// Parameters for listing payments.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct ListPaymentsParams {
    /// Filter payments by status. If not set, all payments are returned.
    pub status: Option<PaymentStatus>,
    /// The maximum number of payments to return. Default is 15.
    #[serde_as(as = "Option<U64Hex>")]
    pub limit: Option<u64>,
    /// The payment hash to start returning payments after (exclusive cursor for pagination).
    pub after: Option<Hash256>,
}

/// Result of listing payments.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ListPaymentsResult {
    /// The list of payments.
    pub payments: Vec<GetPaymentCommandResult>,
    /// The last cursor for pagination. Use this as `after` in the next request to get more results.
    pub last_cursor: Option<Hash256>,
}

/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, it's range limited in 0 ~ 65535, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
/// For example:
/// ```json
/// "custom_records": {
///    "0x1": "0x01020304",
///    "0x2": "0x05060708",
///    "0x3": "0x090a0b0c",
///    "0x4": "0x0d0e0f10010d090a0b0c"
///  }
/// ```
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    #[serde(flatten)]
    #[serde_as(as = "HashMap<U32Hex, SliceHex>")]
    pub data: HashMap<u32, Vec<u8>>,
}

/// Parameters for sending a payment.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentCommandParams {
    /// The public key (`Pubkey`) of the payment target node, serialized as a hex string.
    /// You can obtain a node's pubkey via the `node_info` or `graph_nodes` RPC.
    pub target_pubkey: Option<Pubkey>,

    /// the amount of the payment, the unit is Shannons for non UDT payment
    /// If not set and there is a invoice, the amount will be set to the invoice amount
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,

    /// the hash to use within the payment's HTLC.
    /// If not set and `keysend` is set to true, a random hash will be generated.
    /// If not set and there is a `payment_hash` in the invoice, it will be used.
    /// Otherwise, `payment_hash` need to be set.
    pub payment_hash: Option<Hash256>,

    /// the TLC expiry delta should be used to set the timelock for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,

    /// the TLC expiry limit for the whole payment, in milliseconds, each hop is with a default tlc delta of 1 day
    /// suppose the payment router is with N hops, the total tlc expiry limit is at least (N-1) days
    /// this is also the default value for the payment if this parameter is not provided
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_limit: Option<u64>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// the payment timeout in seconds, if the payment is not completed within this time, it will be cancelled
    #[serde_as(as = "Option<U64Hex>")]
    pub timeout: Option<u64>,

    /// the maximum fee amounts in shannons that the sender is willing to pay.
    /// Note: In trampoline routing mode, the sender will use the max_fee_amount as the total fee as much as possible.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_fee_amount: Option<u128>,

    /// the maximum fee rate per thousand, default is 5 (0.5%)
    #[serde_as(as = "Option<U64Hex>")]
    pub max_fee_rate: Option<u64>,

    /// max parts for the payment, only used for multi-part payments
    #[serde_as(as = "Option<U64Hex>")]
    pub max_parts: Option<u64>,

    /// Optional explicit trampoline hops.
    ///
    /// When set to a non-empty list `[t1, t2, ...]`, routing will only find a path from the
    /// payer to `t1`, and the inner trampoline onion will encode `t1 -> t2 -> ... -> final`.
    pub trampoline_hops: Option<Vec<Pubkey>>,

    /// keysend payment
    pub keysend: Option<bool>,

    /// udt type script for the payment
    pub udt_type_script: Option<Script>,

    /// Allow paying yourself through a circular route, default is false.
    /// This is useful for **channel rebalancing**: the payment flows out of one channel and
    /// back through another, shifting liquidity between your channels without changing your
    /// total balance (only routing fees are deducted).
    /// Set `target_pubkey` to your own node pubkey and `keysend` to `true` to perform a rebalance.
    /// Note: `allow_self_payment` is not compatible with trampoline routing.
    pub allow_self_payment: Option<bool>,

    /// Some custom records for the payment which contains a map of u32 to Vec<u8>
    /// The key is the record type, and the value is the serialized data
    /// For example:
    /// ```json
    /// "custom_records": {
    ///    "0x1": "0x01020304",
    ///    "0x2": "0x05060708",
    ///    "0x3": "0x090a0b0c",
    ///    "0x4": "0x0d0e0f10010d090a0b0c"
    ///  }
    /// ```
    pub custom_records: Option<PaymentCustomRecords>,

    /// Optional route hints to reach the destination through private channels.
    /// Note:
    ///    1. this is only used for the private channels with the last hop.
    ///    2. `hop_hints` is only a `hint` for routing algorithm,
    ///       it is not a guarantee that the payment will be routed through the specified channels,
    ///       it is up to the routing algorithm to decide whether to use the hints or not.
    ///
    /// For example `(pubkey, channel_outpoint, fee_rate, tlc_expiry_delta)` suggest path router
    /// to use the channel of `channel_outpoint` at hop with `pubkey` to forward the payment
    /// and the fee rate is `fee_rate` and tlc_expiry_delta is `tlc_expiry_delta`.
    pub hop_hints: Option<Vec<HopHint>>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
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

/// Parameters for building a payment router.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRouterParams {
    /// the amount of the payment, the unit is Shannons for non UDT payment
    /// If not set, the minimum routable amount `1` is used
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,

    /// udt type script for the payment router
    pub udt_type_script: Option<Script>,

    /// A list of hops that defines the route. This does not include the source hop pubkey.
    /// A hop info is a tuple of pubkey and the channel(specified by channel funding tx) will be used.
    /// This is a strong restriction given on payment router, which means these specified hops and channels
    /// must be adapted in the router. This is different from hop hints, which maybe ignored by find path.
    /// If channel is not specified, find path algorithm will pick a channel within these two peers.
    ///
    /// An error will be returned if there is no router could be build from given hops and channels
    pub hops_info: Vec<HopRequire>,

    /// the TLC expiry delta should be used to set the timelock for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,
}

/// A hop requirement to meet when building a router. Does not include the source node;
/// the last hop is the target node.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopRequire {
    /// The public key of the node
    pub pubkey: Pubkey,
    /// The outpoint for the channel, which means use channel with `channel_outpoint` to reach this node
    #[serde_as(as = "Option<EntityHex>")]
    pub channel_outpoint: Option<OutPoint>,
}

/// A router hop information for a payment, a paymenter router is an array of RouterHop,
/// a router hop generally implies hop `target` will receive `amount_received` with `channel_outpoint` of channel.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouterHop {
    /// The node that is sending the TLC to the next node.
    pub target: Pubkey,
    /// The channel of this hop used to receive TLC
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The amount that the source node will transfer to the target node.
    #[serde_as(as = "U128Hex")]
    pub amount_received: u128,
    /// The expiry for the TLC that the source node sends to the target node.
    #[serde_as(as = "U64Hex")]
    pub incoming_tlc_expiry: u64,
}

/// The router returned by build_router.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BuildPaymentRouterResult {
    /// The hops information for router
    pub router_hops: Vec<RouterHop>,
}

/// Parameters for sending a payment with a specified router.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct SendPaymentWithRouterParams {
    /// the hash to use within the payment's HTLC.
    /// If not set and `keysend` is set to true, a random hash will be generated.
    /// If not set and there is a `payment_hash` in the invoice, it will be used.
    /// Otherwise, `payment_hash` need to be set.
    pub payment_hash: Option<Hash256>,

    /// The router to use for the payment
    pub router: Vec<RouterHop>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// Some custom records for the payment which contains a map of u32 to Vec<u8>
    /// The key is the record type, and the value is the serialized data.
    /// Limits: the sum size of values can not exceed 2048 bytes.
    ///
    /// For example:
    /// ```json
    /// "custom_records": {
    ///    "0x1": "0x01020304",
    ///    "0x2": "0x05060708",
    ///    "0x3": "0x090a0b0c",
    ///    "0x4": "0x0d0e0f10010d090a0b0c"
    ///  }
    /// ```
    pub custom_records: Option<PaymentCustomRecords>,

    /// keysend payment
    pub keysend: Option<bool>,

    /// udt type script for the payment
    pub udt_type_script: Option<Script>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: Option<bool>,
}
