//! Network graph types for the Fiber Network JSON-RPC API.

use crate::schema_helpers::*;
use crate::serde_utils::{EntityHex, Hash256, Pubkey, U128Hex, U64Hex};
use ckb_jsonrpc_types::{DepType, JsonBytes, OutPoint as OutPointWrapper, Script, ScriptHashType};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Parameters for querying graph nodes.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct GraphNodesParams {
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    /// The maximum number of nodes to return.
    pub limit: Option<u64>,
    /// The cursor to start returning nodes from.
    pub after: Option<JsonBytes>,
}

/// The UDT script which is used to identify the UDT configuration for a Fiber Node.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct UdtScript {
    /// The code hash of the script.
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub code_hash: H256,
    /// The hash type of the script.
    pub hash_type: ScriptHashType,
    /// The arguments of the script.
    pub args: String,
}

/// Udt script on-chain dependencies.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct UdtDep {
    /// cell dep described by out_point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_dep: Option<UdtCellDep>,
    /// cell dep described by type ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<Script>,
}

/// The UDT cell dep which is used to identify the UDT configuration for a Fiber Node.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct UdtCellDep {
    /// The out point of the cell dep.
    pub out_point: OutPointWrapper,
    /// The type of the cell dep.
    pub dep_type: DepType,
}

/// The UDT argument info which is used to identify the UDT configuration.
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct UdtArgInfo {
    /// The name of the UDT.
    pub name: String,
    /// The script of the UDT.
    pub script: UdtScript,
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    /// The minimum amount of the UDT that can be automatically accepted.
    pub auto_accept_amount: Option<u128>,
    /// The cell deps of the UDT.
    pub cell_deps: Vec<UdtDep>,
}

/// A list of UDT configuration infos.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct UdtCfgInfos(
    /// The list of UDT configuration infos.
    pub Vec<UdtArgInfo>,
);

/// The Node information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct NodeInfo {
    /// The name of the node.
    pub node_name: String,
    /// The version of the node.
    pub version: String,
    /// The addresses of the node (serialized as strings).
    #[schemars(schema_with = "schema_as_string_array")]
    pub addresses: Vec<String>,
    /// The node features supported by the node.
    pub features: Vec<String>,
    /// The identity public key of the node (secp256k1 compressed, hex string), same as `pubkey` in `list_peers`.
    pub pubkey: Pubkey,
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    /// The latest timestamp set by the owner for the node announcement.
    /// When a Node is online this timestamp will be updated to the latest value.
    pub timestamp: u64,
    /// The chain hash of the node.
    pub chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    /// The minimum CKB funding amount for automatically accepting open channel requests.
    pub auto_accept_min_ckb_funding_amount: u64,
    /// The UDT configuration infos of the node.
    pub udt_cfg_infos: UdtCfgInfos,
}

/// Result of querying graph nodes.
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct GraphNodesResult {
    /// The list of nodes.
    pub nodes: Vec<NodeInfo>,
    /// The last cursor.
    pub last_cursor: JsonBytes,
}

/// Parameters for querying graph channels.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct GraphChannelsParams {
    /// The maximum number of channels to return.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub limit: Option<u64>,
    /// The cursor to start returning channels from.
    pub after: Option<JsonBytes>,
}

/// The channel update info with a single direction of channel.
#[serde_as]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelUpdateInfo {
    /// The timestamp is the time when the channel update was received by the node.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub timestamp: u64,
    /// Whether the channel can be currently used for payments (in this one direction).
    pub enabled: bool,
    /// The exact amount of balance that we can send to the other party via the channel.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub outbound_liquidity: Option<u128>,
    /// The difference in htlc expiry values that you must have when routing through this channel (in milliseconds).
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_expiry_delta: u64,
    /// The minimum value, which must be relayed to the next hop via the channel
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_minimum_value: u128,
    /// The forwarding fee rate for the channel.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub fee_rate: u64,
}

/// The Channel information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct ChannelInfo {
    /// The outpoint of the channel.
    #[serde_as(as = "EntityHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub channel_outpoint: OutPoint,
    /// The identity public key of the first node (secp256k1 compressed, hex string).
    pub node1: Pubkey,
    /// The identity public key of the second node (secp256k1 compressed, hex string).
    pub node2: Pubkey,
    /// The created timestamp of the channel, which is the block header timestamp of the block
    /// that contains the channel funding transaction.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_timestamp: u64,

    /// The update info from node1 to node2, e.g. timestamp, fee_rate, tlc_expiry_delta, tlc_minimum_value
    pub update_info_of_node1: Option<ChannelUpdateInfo>,

    /// The update info from node2 to node1, e.g. timestamp, fee_rate, tlc_expiry_delta, tlc_minimum_value
    pub update_info_of_node2: Option<ChannelUpdateInfo>,

    /// The capacity of the channel.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub capacity: u128,
    /// The chain hash of the channel.
    pub chain_hash: Hash256,
    /// The UDT type script of the channel.
    pub udt_type_script: Option<Script>,
}

/// Result of querying graph channels.
#[derive(Serialize, Deserialize, Clone, JsonSchema)]
pub struct GraphChannelsResult {
    /// A list of channels.
    pub channels: Vec<ChannelInfo>,
    /// The last cursor for pagination.
    pub last_cursor: JsonBytes,
}
