use super::graph::UdtCfgInfos;
use crate::ckb::CkbConfig;
use crate::fiber::serde_utils::U32Hex;
use crate::fiber::{
    serde_utils::{U128Hex, U64Hex},
    types::{Hash256, Pubkey},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::Script;
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tentacle::multiaddr::MultiAddr;

#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct NodeInfoResult {
    /// The version of the node software.
    pub version: String,

    /// The commit hash of the node software.
    pub commit_hash: String,

    /// The identity public key of the node.
    pub node_id: Pubkey,

    /// The optional name of the node.
    pub node_name: Option<String>,

    /// A list of multi-addresses associated with the node.
    pub addresses: Vec<MultiAddr>,

    /// The hash of the blockchain that the node is connected to.
    pub chain_hash: Hash256,

    /// The minimum CKB funding amount for automatically accepting open channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,

    /// The CKB funding amount for automatically accepting channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    pub auto_accept_channel_ckb_funding_amount: u64,

    /// The default funding lock script for the node.
    pub default_funding_lock_script: Script,

    /// The locktime expiry delta for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,

    /// The minimum value for Time-Locked Contracts (TLC) we can send, serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    pub tlc_min_value: u128,

    /// The maximum value for Time-Locked Contracts (TLC) we can send, serialized as a hexadecimal string, `0` means no maximum value limit.
    #[serde_as(as = "U128Hex")]
    pub tlc_max_value: u128,

    /// The fee (to forward payments) proportional to the value of Time-Locked Contracts (TLC), expressed in millionths and serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    pub tlc_fee_proportional_millionths: u128,

    /// The number of channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pub channel_count: u32,

    /// The number of pending channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pub pending_channel_count: u32,

    /// The number of peers connected to the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pub peers_count: u32,

    /// Configuration information for User-Defined Tokens (UDT) associated with the node.
    pub udt_cfg_infos: UdtCfgInfos,
}
