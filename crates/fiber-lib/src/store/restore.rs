use crate::errors::{Error, Result};
use crate::store::audit::{RestoreAuditMap, RestoreAuditStore};
use crate::store::Store;
use fiber_store::restore::perform_physical_copy;
use std::path::PathBuf;
use tracing::info;

pub fn run_restore(checkpoint_path: &PathBuf, db_path: &PathBuf) -> Result<()> {
    perform_physical_copy(checkpoint_path, db_path)?;

    let store = Store::new(db_path.to_str().unwrap()).map_err(Error::DBInternalError)?;

    info!("Scanning for active channels to build audit map...");
    let audit_map = RestoreAuditMap::build_from_store(&store);
    let channel_count = audit_map.channels.len();

    store.insert_restore_audit_map(audit_map);

    info!(
        "Restore completed successfully. {} channels marked for consistency check on next startup.",
        channel_count
    );

    Ok(())
}
