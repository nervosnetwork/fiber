#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use super::graph::UdtCfgInfos;
use crate::ckb::CkbConfig;
use crate::fiber::serde_utils::U32Hex;
use crate::fiber::{
    serde_utils::{U128Hex, U64Hex},
    types::{Hash256, Pubkey},
    FiberConfig, NetworkActorCommand, NetworkActorMessage,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::now_timestamp_as_millis_u64;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::server::RpcServerStore;
#[cfg(not(target_arch = "wasm32"))]
use crate::store::store_impl::KVStore;
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::Script;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;

use ractor::{call, ActorRef};
#[cfg(not(target_arch = "wasm32"))]
use rocksdb::checkpoint::Checkpoint;
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

/// The result of a backup operation.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BackupResult {
    /// The path of backup file
    path: String,
    /// The timestamp of backup
    #[serde_as(as = "U64Hex")]
    timestamp: u64,
}

pub struct InfoRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,

    default_funding_lock_script: Script,

    #[cfg(not(target_arch = "wasm32"))]
    store: S,
    #[cfg(not(target_arch = "wasm32"))]
    fiber_key_path: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    ckb_key_path: PathBuf,

    #[cfg(target_arch = "wasm32")]
    _marker: std::marker::PhantomData<S>,
}

#[cfg(not(target_arch = "wasm32"))]
pub trait StoreInfo: RpcServerStore + KVStore + Clone + Send + Sync + 'static {}
#[cfg(not(target_arch = "wasm32"))]
impl<T> StoreInfo for T where T: RpcServerStore + KVStore + Clone + Send + Sync + 'static {}
#[cfg(target_arch = "wasm32")]
pub trait StoreInfo: Clone + Send + Sync + 'static {}
#[cfg(target_arch = "wasm32")]
impl<T> StoreInfo for T where T: Clone + Send + Sync + 'static {}

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

/// The RPC module for node information.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait InfoRpc {
    /// Get the node information.
    #[method(name = "node_info")]
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned>;

    /// Backup the node database and key files to a specified path.
    #[method(name = "backup_now")]
    async fn backup_now(&self, path: String) -> Result<BackupResult, ErrorObjectOwned>;
}

#[async_trait::async_trait]
#[cfg(not(target_arch = "wasm32"))]
impl<S: StoreInfo> InfoRpcServer for InfoRpcServerImpl<S> {
    async fn node_info(&self) -> Result<NodeInfoResult, ErrorObjectOwned> {
        self.node_info().await
    }

    async fn backup_now(&self, path: String) -> Result<BackupResult, ErrorObjectOwned> {
        self.backup_now(path).await
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
            tlc_fee_proportional_millionths: response.tlc_fee_proportional_millionths,
            channel_count: response.channel_count,
            pending_channel_count: response.pending_channel_count,
            peers_count: response.peers_count,
            udt_cfg_infos: response.udt_cfg_infos.into(),
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn backup_now(&self, path: String) -> Result<BackupResult, ErrorObjectOwned> {
        let target_dir = PathBuf::from(&path);

        // Prevent overwriting existing data
        if target_dir.exists() {
            return log_and_error!(path, "Backup directory already exists".to_string());
        }

        if let Err(e) = std::fs::create_dir_all(&target_dir) {
            return log_and_error!(path, format!("Failed to create backup directory: {}", e));
        }
        tracing::info!("Starting node backup to: {:?}", target_dir);

        let db_backup_path = target_dir.join("db");
        let checkpoint = match Checkpoint::new(self.store.inner_db()) {
            Ok(c) => c,
            Err(e) => return log_and_error!(path, format!("RocksDB checkpoint init error: {}", e)),
        };

        if let Err(e) = checkpoint.create_checkpoint(&db_backup_path) {
            return log_and_error!(path, format!("Failed to create DB checkpoint: {}", e));
        }

        self.perform_key_backup(&target_dir)?;

        let now = now_timestamp_as_millis_u64();

        tracing::info!("Backup completed successfully at block height (synced): [Approach A]");

        Ok(BackupResult {
            path,
            timestamp: now,
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn perform_key_backup(&self, target_dir: &Path) -> Result<(), ErrorObjectOwned> {
        let keys_to_copy = [(&self.ckb_key_path, "key"), (&self.fiber_key_path, "sk")];

        for (src_file, dest_name) in keys_to_copy {
            if src_file.exists() {
                let dest_file = target_dir.join(dest_name);
                if let Err(e) = std::fs::copy(src_file, &dest_file) {
                    return log_and_error!(
                        target_dir,
                        format!("Failed to copy key file {:?}: {}", src_file, e)
                    );
                }
                tracing::info!("Successfully backed up key: {}", dest_name);
            } else {
                tracing::warn!("Key file not found at {:?}, skipping", src_file);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::test_utils::{generate_store, get_fiber_config, NetworkNode, TempDir};
    use std::fs;

    async fn setup_test_impl() -> (InfoRpcServerImpl<crate::store::Store>, TempDir) {
        let (store, tempdir) = generate_store();
        let fiber_config = get_fiber_config(tempdir.as_ref(), Some("backup_test"));

        let ckb_config = CkbConfig {
            base_dir: Some(PathBuf::from(tempdir.as_ref())),
            rpc_url: "http://127.0.0.1:8114".to_string(),
            udt_whitelist: None,
            tx_tracing_polling_interval_ms: 4000,
            #[cfg(not(target_arch = "wasm32"))]
            funding_tx_shell_builder: None,
        };

        let ckb_key_dir = ckb_config.base_dir.as_ref().unwrap();
        let fiber_key_dir = fiber_config.base_dir().to_path_buf();

        fs::create_dir_all(ckb_key_dir).unwrap();
        fs::create_dir_all(&fiber_key_dir).unwrap();
        fs::write(ckb_key_dir.join("key"), "mock_ckb_key").unwrap();
        fs::write(fiber_key_dir.join("sk"), "mock_fiber_key").unwrap();

        let node = NetworkNode::new_with_node_name_opt(Some("backup_test".to_string()));
        let actor = node.await.get_actor();

        let server = InfoRpcServerImpl::new(actor, store, ckb_config, Some(fiber_config));

        (server, tempdir)
    }

    #[tokio::test]
    async fn test_rpc_backup_now_success() {
        let (server, root) = setup_test_impl().await;

        // Construct path
        let backup_path = root.as_ref().join("backup_v1");
        let path_str = backup_path.to_str().unwrap().to_string();

        let result = server
            .backup_now(path_str)
            .await
            .expect("Backup should succeed");

        // Assert path correction
        assert!(result.path.contains("backup_v1"));

        // Assert path exists
        assert!(backup_path.exists());
        assert!(backup_path.join("db").exists());

        // Check copied file
        let ckb_key = backup_path.join("key");
        let fiber_key = backup_path.join("sk");

        assert!(
            ckb_key.exists() || fiber_key.exists(),
            "At least one key should be backed up"
        );
    }

    #[tokio::test]
    async fn test_rpc_backup_now_already_exists() {
        let (server, root) = setup_test_impl().await;

        // Using an existing path
        let path_str = root.as_ref().to_str().unwrap().to_string();

        let result = server.backup_now(path_str).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should be -32000 (Invalid Params)
        assert_eq!(err.code(), -32000);
    }
}
