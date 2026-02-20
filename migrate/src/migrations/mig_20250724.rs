use fiber_v070::{
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use std::sync::Arc;

// Remember to update the version number here
const MIGRATION_DB_VERSION: &str = "20250724111111";

pub struct MigrationObj {
    version: String,
}

impl MigrationObj {
    pub fn new() -> Self {
        Self {
            version: MIGRATION_DB_VERSION.to_string(),
        }
    }
}

impl Migration for MigrationObj {
    fn migrate<'a>(
        &self,
        db: &'a Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a Store, Error> {
        eprintln!("MigrationObj::migrate .....{}....", MIGRATION_DB_VERSION);
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn is_break_change(&self) -> bool {
        // This migration is a breaking change for MPP and security updates
        true
    }
}
