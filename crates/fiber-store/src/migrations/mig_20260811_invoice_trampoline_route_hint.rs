use crate::migration::{Migration, MigrationStore};
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260811120000";

/// Records the backward-compatible addition of `TrampolineRouteHint` to the end
/// of the persisted invoice `Attribute` enum. Existing variant discriminants do
/// not change, so stored invoices require no data rewrite.
pub struct MigrationObj {
    version: String,
}

impl Default for MigrationObj {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrationObj {
    pub fn new() -> Self {
        Self {
            version: MIGRATION_DB_VERSION.to_string(),
        }
    }
}

impl Migration for MigrationObj {
    fn migrate(&self, _store: &dyn MigrationStore) -> Result<(), String> {
        info!(
            "Migrating to {}: enabling invoice trampoline route hints ...",
            MIGRATION_DB_VERSION
        );
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}
