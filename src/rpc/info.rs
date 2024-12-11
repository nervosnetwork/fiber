use super::graph::UdtCfgInfos;
use crate::fiber::serde_utils::U32Hex;
use crate::fiber::{
    channel::ChannelActorStateStore,
    serde_utils::{U128Hex, U64Hex},
    types::{Hash256, Pubkey},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::{handle_actor_call, log_and_error};
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::{multiaddr::MultiAddr, secio::PeerId};

#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct NodeInfoResult {
    /// The version of the node software.
    version: String,

    /// The commit hash of the node software.
    commit_hash: String,

    /// The public key of the node.
    public_key: Pubkey,

    /// The optional name of the node.
    node_name: Option<String>,

    /// The peer ID of the node, serialized as a string.
    #[serde_as(as = "DisplayFromStr")]
    peer_id: PeerId,

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

    /// The locktime expiry delta for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    tlc_expiry_delta: u64,

    /// The minimum value for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    tlc_min_value: u128,

    /// The maximum value for Time-Locked Contracts (TLC), serialized as a hexadecimal string, `0` means no maximum value limit.
    #[serde_as(as = "U128Hex")]
    tlc_max_value: u128,

    /// The fee proportional to the value of Time-Locked Contracts (TLC), expressed in millionths and serialized as a hexadecimal string.
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

pub(crate) struct InfoRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    _store: S,
}

impl<S> InfoRpcServerImpl<S> {
    pub(crate) fn new(actor: ActorRef<NetworkActorMessage>, _store: S) -> Self {
        InfoRpcServerImpl { actor, _store }
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
impl<S> InfoRpcServer for InfoRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let commit_hash = crate::get_git_versin().to_string();

        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::NodeInfo((), rpc_reply));

        handle_actor_call!(self.actor, message, ()).map(|response| NodeInfoResult {
            version,
            commit_hash,
            public_key: response.public_key,
            node_name: response.node_name.map(|name| name.to_string()),
            peer_id: response.peer_id,
            addresses: response.addresses,
            chain_hash: response.chain_hash,
            open_channel_auto_accept_min_ckb_funding_amount: response
                .open_channel_auto_accept_min_ckb_funding_amount,
            auto_accept_channel_ckb_funding_amount: response.auto_accept_channel_ckb_funding_amount,
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
