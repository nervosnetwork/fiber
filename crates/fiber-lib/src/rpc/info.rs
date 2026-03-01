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
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;

use ractor::{call, ActorRef};
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

    /// The identity public key of this node (secp256k1 compressed, hex string).
    /// This is the same value referred to as `pubkey` in `list_peers` responses.
    /// Note: this is different from `peer_id`, which is a base58 hash derived from this key.
    pub pubkey: Pubkey,

    /// The features supported by the node.
    pub features: Vec<String>,

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

pub struct InfoRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
    default_funding_lock_script: Script,
}

impl InfoRpcServerImpl {
    #[allow(unused_variables)]
    pub fn new(actor: ActorRef<NetworkActorMessage>, config: CkbConfig) -> Self {
        #[cfg(not(test))]
        let default_funding_lock_script = config
            .get_default_funding_lock_script()
            .expect("get default funding lock script should be ok")
            .into();

        // `decrypt_from_file` is invoked in `get_default_funding_lock_script`,
        // which will cost more than 30 seconds, so we mock it in tests.
        #[cfg(test)]
        let default_funding_lock_script = Default::default();

        InfoRpcServerImpl {
            actor,
            default_funding_lock_script,
        }
    }
}

/// The RPC module for node information.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait InfoRpc {
    /// Get the node information.
    #[method(name = "node_info")]
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned>;
}

#[async_trait::async_trait]
#[cfg(not(target_arch = "wasm32"))]
impl InfoRpcServer for InfoRpcServerImpl {
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        self.node_info().await
    }
}
impl InfoRpcServerImpl {
    pub async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let commit_hash = crate::get_git_commit_info();

        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::NodeInfo((), rpc_reply));

        handle_actor_call!(self.actor, message, ()).map(|response| NodeInfoResult {
            version,
            commit_hash,
            features: response.features.enabled_features_names(),
            pubkey: response.node_id,
            node_name: response.node_name.map(|name| name.to_string()),
            addresses: response.addresses,
            chain_hash: response.chain_hash,
            open_channel_auto_accept_min_ckb_funding_amount: response
                .open_channel_auto_accept_min_ckb_funding_amount,
            auto_accept_channel_ckb_funding_amount: response.auto_accept_channel_ckb_funding_amount,
            default_funding_lock_script: self.default_funding_lock_script.clone(),
            tlc_expiry_delta: response.tlc_expiry_delta,
            tlc_min_value: response.tlc_min_value,
            tlc_fee_proportional_millionths: response.tlc_fee_proportional_millionths,
            channel_count: response.channel_count,
            pending_channel_count: response.pending_channel_count,
            peers_count: response.peers_count,
            udt_cfg_infos: response.udt_cfg_infos.into(),
        })
    }
}
