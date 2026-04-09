//! Node info types for the Fiber Network JSON-RPC API.

use crate::graph::UdtCfgInfos;
use crate::schema_helpers::*;
use crate::serde_utils::{Hash256, Pubkey, U128Hex, U32Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Node information result.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct NodeInfoResult {
    /// The version of the node software.
    pub version: String,

    /// The commit hash of the node software.
    pub commit_hash: String,

    /// The identity public key of this node (secp256k1 compressed, hex without 0x prefix).
    pub pubkey: Pubkey,

    /// The features supported by the node.
    pub features: Vec<String>,

    /// The optional name of the node.
    pub node_name: Option<String>,

    /// A list of multi-addresses associated with the node (as strings).
    #[schemars(schema_with = "schema_as_string_array")]
    pub addresses: Vec<String>,

    /// The hash of the blockchain that the node is connected to.
    pub chain_hash: Hash256,

    /// The minimum CKB funding amount for automatically accepting open channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,

    /// The CKB funding amount for automatically accepting channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub auto_accept_channel_ckb_funding_amount: u64,

    /// The default funding lock script for the node.
    pub default_funding_lock_script: Script,

    /// The locktime expiry delta for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_expiry_delta: u64,

    /// The minimum value for Time-Locked Contracts (TLC) we can send, serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_min_value: u128,

    /// The fee (to forward payments) proportional to the value of Time-Locked Contracts (TLC),
    /// expressed in millionths and serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_fee_proportional_millionths: u128,

    /// The number of channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub channel_count: u32,

    /// The number of pending channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub pending_channel_count: u32,

    /// The number of peers connected to the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub peers_count: u32,

    /// Configuration information for User-Defined Tokens (UDT) associated with the node.
    pub udt_cfg_infos: UdtCfgInfos,
}
