use crate::ckb::CkbConfig;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::Script;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;

pub use fiber_json_types::NodeInfoResult;
use ractor::{call, ActorRef};

use crate::store::actor::StoreActorMessage;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

pub struct InfoRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
    #[allow(unused)]
    store_actor: Option<ActorRef<StoreActorMessage>>,
    default_funding_lock_script: Script,
}

impl InfoRpcServerImpl {
    #[allow(unused_variables)]
    pub fn new(
        actor: ActorRef<NetworkActorMessage>,
        store_actor: Option<ActorRef<StoreActorMessage>>,
        config: CkbConfig,
    ) -> Self {
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
            store_actor,
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

    /// Backup the node information.
    #[method(name = "backup_now")]
    async fn backup_now(&self, target_path: &Path) -> Result<(), ErrorObjectOwned>;
}

#[async_trait::async_trait]
#[cfg(not(target_arch = "wasm32"))]
impl InfoRpcServer for InfoRpcServerImpl {
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        self.node_info().await
    }

    async fn backup_now(&self, target_path: &Path) -> Result<(), ErrorObjectOwned> {
        self.backup_now(target_path).await
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
            pubkey: response.node_id.into(),
            node_name: response.node_name.map(|name| name.to_string()),
            addresses: response.addresses.iter().map(|a| a.to_string()).collect(),
            chain_hash: response.chain_hash.into(),
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

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn backup_now(&self, target_path: &Path) -> Result<(), ErrorObjectOwned> {
        if let Some(ref store_actor) = self.store_actor {
            handle_actor_call!(store_actor, StoreActorMessage::ForceBackup, target_path)
        } else {
            log_and_error!(target_path, format!("Backup service is not initialized"))
        }
    }
}
