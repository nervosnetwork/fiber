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

use super::graph::UdtCfgInfos;

#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct NodeInfoResult {
    version: String,
    commit_hash: String,
    public_key: Pubkey,
    node_name: Option<String>,
    #[serde_as(as = "DisplayFromStr")]
    peer_id: PeerId,
    addresses: Vec<MultiAddr>,
    chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    open_channel_auto_accept_min_ckb_funding_amount: u64,
    #[serde_as(as = "U64Hex")]
    auto_accept_channel_ckb_funding_amount: u64,
    #[serde_as(as = "U64Hex")]
    tlc_locktime_expiry_delta: u64,
    #[serde_as(as = "U128Hex")]
    tlc_min_value: u128,
    #[serde_as(as = "U128Hex")]
    tlc_max_value: u128,
    #[serde_as(as = "U128Hex")]
    tlc_fee_proportional_millionths: u128,
    #[serde_as(as = "U32Hex")]
    channel_count: u32,
    #[serde_as(as = "U32Hex")]
    pending_channel_count: u32,
    #[serde_as(as = "U32Hex")]
    peers_count: u32,
    network_sync_status: String,
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

#[rpc(server)]
trait InfoRpc {
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
            tlc_locktime_expiry_delta: response.tlc_locktime_expiry_delta,
            tlc_min_value: response.tlc_min_value,
            tlc_max_value: response.tlc_max_value,
            tlc_fee_proportional_millionths: response.tlc_fee_proportional_millionths,
            channel_count: response.channel_count,
            pending_channel_count: response.pending_channel_count,
            peers_count: response.peers_count,
            network_sync_status: response.network_sync_status,
            udt_cfg_infos: response.udt_cfg_infos.into(),
        })
    }
}
