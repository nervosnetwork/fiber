use crate::errors::{Error, Result};
use crate::fiber::channel::ChannelActorStateStore;
use crate::store::open_store;
use fiber_store::StorageBackend;
use fiber_types::ChannelState;
use std::path::Path;
use tracing::info;

pub fn restore(restore_path: &Path, base_path: &Path) -> Result<()> {
    let store = open_store(base_path).map_err(Error::DBInternalError)?;
    #[cfg(not(target_arch = "wasm32"))]
    restore_node_keys(restore_path, base_path)?;
    store.restore(restore_path, base_path)?;

    info!("Scanning stale channels.");
    for mut channel in store.get_all_channel_states() {
        if channel.is_risk_of_penalty() {
            channel.update_state(ChannelState::Stale);
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn restore_node_keys(restore_path: &Path, base_dir: &Path) -> Result<()> {
    let keys = [("key", "key"), ("sk", "sk")];

    for (src_name, dest_name) in keys {
        let src = restore_path.join(src_name);
        if src.exists() {
            let dest = base_dir.join(dest_name);
            std::fs::copy(&src, &dest).map_err(|e| {
                Error::DBInternalError(format!("Failed to restore key {}: {}", src_name, e))
            })?;
            tracing::info!("Restored key file: {}", dest_name);
        }
    }
    Ok(())
}
