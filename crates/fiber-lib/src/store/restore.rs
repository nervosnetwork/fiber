use crate::errors::{Error, Result};
use crate::fiber::channel::ChannelActorStateStore;
use crate::store::open_store;
use fiber_store::StorageBackend;
use fiber_types::ChannelState;
use std::path::Path;
use tracing::info;

#[allow(unused_variables)]
pub fn restore(
    restore_path: &Path,
    base_path: &Path,
    fiber_key_path: &Path,
    ckb_key_path: &Path,
) -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    restore_node_keys(restore_path, fiber_key_path, ckb_key_path)?;
    {
        let store = open_store(base_path).map_err(Error::DBInternalError)?;
        store.restore(restore_path, base_path)?;
    }
    let store = open_store(base_path).map_err(Error::DBInternalError)?;

    info!("Scanning stale channels.");
    for mut channel in store.get_all_channel_states() {
        if channel.is_risk_of_penalty() {
            channel.update_state(ChannelState::Stale);
            store.insert_channel_actor_state(channel);
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn restore_node_keys(
    restore_path: &Path,
    fiber_key_path: &Path,
    ckb_key_path: &Path,
) -> Result<()> {
    let backup_fiber_key = restore_path.join("sk");
    let backup_ckb_key = restore_path.join("key");
    if backup_fiber_key.exists() && backup_ckb_key.exists() {
        std::fs::copy(&backup_fiber_key, fiber_key_path)
            .map_err(|e| Error::DBInternalError(format!("Failed to restore fiber key: {e}")))?;
        std::fs::copy(&backup_ckb_key, ckb_key_path)
            .map_err(|e| Error::DBInternalError(format!("Failed to restore CKB key: {e}")))?;
        info!("Successfully restored node keys from backup.");
    } else {
        info!("No node keys found in backup, skipping key restoration.");
    }
    Ok(())
}
