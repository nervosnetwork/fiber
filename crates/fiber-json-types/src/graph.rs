//! Graph-related types for the Fiber Network Node RPC API.

use crate::serde_utils::{EntityHex, U128Hex, U64Hex};
use crate::{Hash256, Pubkey};

use ckb_jsonrpc_types::{DepType, JsonBytes, OutPoint as OutPointWrapper, Script, ScriptHashType};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tentacle::multiaddr::MultiAddr;

// ============================================================
// Internal graph types exposed via RPC
// ============================================================

/// Channel update info from one direction.
#[serde_as]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdateInfo {
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    pub enabled: bool,
    #[serde_as(as = "Option<U128Hex>")]
    pub outbound_liquidity: Option<u128>,
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
    #[serde_as(as = "U128Hex")]
    pub tlc_minimum_value: u128,
    #[serde_as(as = "U64Hex")]
    pub fee_rate: u64,
}

/// A single hop in a payment route.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouterHop {
    pub target: Pubkey,
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    #[serde_as(as = "U128Hex")]
    pub amount_received: u128,
    #[serde_as(as = "U64Hex")]
    pub incoming_tlc_expiry: u64,
}

// ============================================================
// RPC param/result types
// ============================================================

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesParams {
    #[serde_as(as = "Option<U64Hex>")]
    /// The maximum number of nodes to return.
    pub limit: Option<u64>,
    /// The cursor to start returning nodes from.
    pub after: Option<JsonBytes>,
}

/// The UDT script which is used to identify the UDT configuration for a Fiber Node
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtScript {
    /// The code hash of the script.
    pub code_hash: H256,
    /// The hash type of the script.
    pub hash_type: ScriptHashType,
    /// The arguments of the script.
    pub args: String,
}

/// UDT script on-chain dependencies.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtDep {
    /// cell dep described by out_point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_dep: Option<UdtCellDep>,
    /// cell dep described by type ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<Script>,
}

/// The UDT cell dep which is used to identify the UDT configuration for a Fiber Node
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCellDep {
    /// The out point of the cell dep.
    pub out_point: OutPointWrapper,
    /// The type of the cell dep.
    pub dep_type: DepType,
}

/// The UDT argument info which is used to identify the UDT configuration
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtArgInfo {
    /// The name of the UDT.
    pub name: String,
    /// The script of the UDT.
    pub script: UdtScript,
    #[serde_as(as = "Option<U128Hex>")]
    /// The minimum amount of the UDT that can be automatically accepted.
    pub auto_accept_amount: Option<u128>,
    /// The cell deps of the UDT.
    pub cell_deps: Vec<UdtDep>,
}

/// A list of UDT configuration infos.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCfgInfos(
    /// The list of UDT configuration infos.
    pub Vec<UdtArgInfo>,
);

/// The Node information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    /// The name of the node.
    pub node_name: String,
    /// The version of the node.
    pub version: String,
    /// The addresses of the node.
    pub addresses: Vec<MultiAddr>,
    /// The node features supported by the node.
    pub features: Vec<String>,
    /// The identity public key of the node.
    pub node_id: Pubkey,
    #[serde_as(as = "U64Hex")]
    /// The latest timestamp set by the owner for the node announcement.
    pub timestamp: u64,
    /// The chain hash of the node.
    pub chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The minimum CKB funding amount for automatically accepting open channel requests.
    pub auto_accept_min_ckb_funding_amount: u64,
    /// The UDT configuration infos of the node.
    pub udt_cfg_infos: UdtCfgInfos,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesResult {
    /// The list of nodes.
    pub nodes: Vec<NodeInfo>,
    /// The last cursor.
    pub last_cursor: JsonBytes,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphChannelsParams {
    /// The maximum number of channels to return.
    #[serde_as(as = "Option<U64Hex>")]
    pub limit: Option<u64>,
    /// The cursor to start returning channels from.
    pub after: Option<JsonBytes>,
}

/// The Channel information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelInfo {
    /// The outpoint of the channel.
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The identity public key of the first node.
    pub node1: Pubkey,
    /// The identity public key of the second node.
    pub node2: Pubkey,
    /// The created timestamp of the channel.
    #[serde_as(as = "U64Hex")]
    pub created_timestamp: u64,
    /// The update info from node1 to node2.
    pub update_info_of_node1: Option<ChannelUpdateInfo>,
    /// The update info from node2 to node1.
    pub update_info_of_node2: Option<ChannelUpdateInfo>,
    /// The capacity of the channel.
    #[serde_as(as = "U128Hex")]
    pub capacity: u128,
    /// The chain hash of the channel.
    pub chain_hash: Hash256,
    /// The UDT type script of the channel.
    pub udt_type_script: Option<Script>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GraphChannelsResult {
    /// A list of channels.
    pub channels: Vec<ChannelInfo>,
    /// The last cursor for pagination.
    pub last_cursor: JsonBytes,
}
