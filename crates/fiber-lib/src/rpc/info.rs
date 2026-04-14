use crate::ckb::CkbConfig;
use crate::fiber::{FiberConfig, NetworkActorCommand, NetworkActorMessage};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::Script;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;

pub use fiber_json_types::NodeInfoResult;
#[cfg(not(target_arch = "wasm32"))]
use fiber_store::StorageBackend;
use ractor::{call, ActorRef};
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

pub struct InfoRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    default_funding_lock_script: Script,

    #[cfg(not(target_arch = "wasm32"))]
    store: S,
    #[cfg(not(target_arch = "wasm32"))]
    ckb_key_path: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    fiber_key_path: PathBuf,

    #[cfg(target_arch = "wasm32")]
    _marker: std::marker::PhantomData<S>,
}

impl<S: StoreInfo> InfoRpcServerImpl<S> {
    #[allow(unused_variables)]
    pub fn new(
        actor: ActorRef<NetworkActorMessage>,
        store: S,
        ckb_config: CkbConfig,
        fiber_config: Option<FiberConfig>,
    ) -> Self {
        #[cfg(not(test))]
        let default_funding_lock_script = ckb_config
            .get_default_funding_lock_script()
            .expect("get default funding lock script should be ok")
            .into();

        // `decrypt_from_file` is invoked in `get_default_funding_lock_script`,
        // which will cost more than 30 seconds, so we mock it in tests.
        #[cfg(test)]
        let default_funding_lock_script = Default::default();

        #[cfg(not(target_arch = "wasm32"))]
        let fiber_config = fiber_config.expect("fiber config should be set");

        InfoRpcServerImpl {
            actor,
            default_funding_lock_script,
            #[cfg(not(target_arch = "wasm32"))]
            store,
            #[cfg(not(target_arch = "wasm32"))]
            ckb_key_path: ckb_config.base_dir().join("key"),
            #[cfg(not(target_arch = "wasm32"))]
            fiber_key_path: fiber_config.base_dir().join("sk"),
            #[cfg(target_arch = "wasm32")]
            _marker: std::marker::PhantomData,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub trait StoreInfo: StorageBackend + Clone + Send + Sync + 'static {}
#[cfg(not(target_arch = "wasm32"))]
impl<T> StoreInfo for T where T: StorageBackend + Clone + Send + Sync + 'static {}
#[cfg(target_arch = "wasm32")]
pub trait StoreInfo: Clone + Send + Sync + 'static {}
#[cfg(target_arch = "wasm32")]
impl<T> StoreInfo for T where T: Clone + Send + Sync + 'static {}

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
impl<S: StoreInfo> InfoRpcServer for InfoRpcServerImpl<S> {
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        self.node_info().await
    }

    async fn backup_now(&self, target_path: &Path) -> Result<(), ErrorObjectOwned> {
        self.backup_now(target_path).await
    }
}
impl<S: StoreInfo> InfoRpcServerImpl<S> {
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
        perform_key_backup(target_path, &self.ckb_key_path, &self.fiber_key_path)
            .or_else(|e| log_and_error!(target_path, format!("Failed to backup keys: {e}")))?;

        self.store
            .backup(target_path)
            .or_else(|e| log_and_error!(target_path, format!("Failed to backup: {e}")))
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Backup the node key files to a specified path.
fn perform_key_backup(
    target_dir: &Path,
    ckb_key_path: &Path,
    fiber_key_path: &Path,
) -> Result<(), String> {
    let keys_to_copy = [(ckb_key_path, "key"), (fiber_key_path, "sk")];

    for (src_file, dest_name) in keys_to_copy {
        if src_file.exists() {
            let dest_file = target_dir.join(dest_name);
            if let Err(e) = std::fs::copy(src_file, &dest_file) {
                return Err(format!("Failed to copy key file {:?}: {}", src_file, e));
            }
            tracing::info!("Successfully backed up key: {}", dest_name);
        } else {
            tracing::warn!("Key file not found at {:?}, skipping", src_file);
        }
    }
    Ok(())
}
