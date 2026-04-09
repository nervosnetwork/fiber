use crate::errors::{Error, Result};
use crate::store::audit::create_restore_audit_map;
use crate::store::open_store;
use fiber_store::StorageBackend;
use fiber_types::RestoreAuditStore;
use std::path::Path;
use tracing::info;

pub fn restore(restore_path: &Path, base_path: &Path) -> Result<()> {
    let store = open_store(base_path).map_err(Error::DBInternalError)?;
    #[cfg(not(target_arch = "wasm32"))]
    restore_node_keys(restore_path, base_path)?;
    store.restore(restore_path, base_path)?;

    info!("Scanning for active channels to build audit map...");
    let audit_map = create_restore_audit_map(&store);
    let channel_count = audit_map.channels.len();

    store.insert_restore_audit_map(audit_map);

    info!(
        "Restore completed successfully. {} channels marked for consistency check on next startup.",
        channel_count
    );

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
