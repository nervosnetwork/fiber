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

/// The RPC module for node information.
#[rpc(server)]
trait InfoRpc {
    /// Get the node information.
    #[method(name = "node_info")]
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned>;
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
