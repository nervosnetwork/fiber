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
pub(crate) struct NodeInfoResult {
    /// The version of the node software.
    version: String,

    /// The commit hash of the node software.
    commit_hash: String,

    /// The identity public key of the node.
    node_id: Pubkey,

    /// The optional name of the node.
    node_name: Option<String>,

    /// A list of multi-addresses associated with the node.
    addresses: Vec<MultiAddr>,

    /// The hash of the blockchain that the node is connected to.
    chain_hash: Hash256,

    /// The minimum CKB funding amount for automatically accepting open channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    open_channel_auto_accept_min_ckb_funding_amount: u64,

    /// The CKB funding amount for automatically accepting channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    auto_accept_channel_ckb_funding_amount: u64,

    /// The default funding lock script for the node.
    default_funding_lock_script: Script,

    /// The locktime expiry delta for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    tlc_expiry_delta: u64,

    /// The minimum value for Time-Locked Contracts (TLC) we can send, serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    tlc_min_value: u128,

    /// The maximum value for Time-Locked Contracts (TLC) we can send, serialized as a hexadecimal string, `0` means no maximum value limit.
    #[serde_as(as = "U128Hex")]
    tlc_max_value: u128,

    /// The fee (to forward payments) proportional to the value of Time-Locked Contracts (TLC), expressed in millionths and serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    tlc_fee_proportional_millionths: u128,

    /// The number of channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    channel_count: u32,

    /// The number of pending channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pending_channel_count: u32,

    /// The number of peers connected to the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    peers_count: u32,

    /// Configuration information for User-Defined Tokens (UDT) associated with the node.
    udt_cfg_infos: UdtCfgInfos,
}

pub(crate) struct InfoRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
    default_funding_lock_script: Script,
}

impl InfoRpcServerImpl {
    pub(crate) fn new(actor: ActorRef<NetworkActorMessage>, config: CkbConfig) -> Self {
        let default_funding_lock_script = config
            .get_default_funding_lock_script()
            .expect("get default funding lock script should be ok")
            .into();
        InfoRpcServerImpl {
            actor,
            default_funding_lock_script,
        }
    }
}

/// The RPC module for node information.
#[rpc(server)]
trait InfoRpc {
    /// Get the node information.
    #[method(name = "node_info")]
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned>;
}

#[async_trait]
impl InfoRpcServer for InfoRpcServerImpl {
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let commit_hash = crate::get_git_version().to_string();

        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::NodeInfo((), rpc_reply));

        handle_actor_call!(self.actor, message, ()).map(|response| NodeInfoResult {
            version,
            commit_hash,
            node_id: response.node_id,
            node_name: response.node_name.map(|name| name.to_string()),
            addresses: response.addresses,
            chain_hash: response.chain_hash,
            open_channel_auto_accept_min_ckb_funding_amount: response
                .open_channel_auto_accept_min_ckb_funding_amount,
            auto_accept_channel_ckb_funding_amount: response.auto_accept_channel_ckb_funding_amount,
            default_funding_lock_script: self.default_funding_lock_script.clone(),
            tlc_expiry_delta: response.tlc_expiry_delta,
            tlc_min_value: response.tlc_min_value,
            tlc_max_value: response.tlc_max_value,
            tlc_fee_proportional_millionths: response.tlc_fee_proportional_millionths,
            channel_count: response.channel_count,
            pending_channel_count: response.pending_channel_count,
            peers_count: response.peers_count,
            udt_cfg_infos: response.udt_cfg_infos.into(),
        })
    }
}
