//! Node info types for the Fiber Network Node RPC API.

use crate::graph::UdtCfgInfos;
use crate::serde_utils::{U128Hex, U32Hex, U64Hex};
use crate::{Hash256, Pubkey};

use ckb_jsonrpc_types::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tentacle::multiaddr::MultiAddr;

#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct NodeInfoResult {
    /// The version of the node software.
    pub version: String,
    /// The commit hash of the node software.
    pub commit_hash: String,
    /// The identity public key of the node.
    pub node_id: Pubkey,
    /// The features supported by the node.
    pub features: Vec<String>,
    /// The optional name of the node.
    pub node_name: Option<String>,
    /// A list of multi-addresses associated with the node.
    pub addresses: Vec<MultiAddr>,
    /// The hash of the blockchain that the node is connected to.
    pub chain_hash: Hash256,
    /// The minimum CKB funding amount for automatically accepting open channel requests.
    #[serde_as(as = "U64Hex")]
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,
    /// The CKB funding amount for automatically accepting channel requests.
    #[serde_as(as = "U64Hex")]
    pub auto_accept_channel_ckb_funding_amount: u64,
    /// The default funding lock script for the node.
    pub default_funding_lock_script: Script,
    /// The locktime expiry delta for TLC.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
    /// The minimum value for TLC we can send.
    #[serde_as(as = "U128Hex")]
    pub tlc_min_value: u128,
    /// The fee proportional to the value of TLC, expressed in millionths.
    #[serde_as(as = "U128Hex")]
    pub tlc_fee_proportional_millionths: u128,
    /// The number of channels associated with the node.
    #[serde_as(as = "U32Hex")]
    pub channel_count: u32,
    /// The number of pending channels.
    #[serde_as(as = "U32Hex")]
    pub pending_channel_count: u32,
    /// The number of peers connected to the node.
    #[serde_as(as = "U32Hex")]
    pub peers_count: u32,
    /// Configuration information for UDT.
    pub udt_cfg_infos: UdtCfgInfos,
}
