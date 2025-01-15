use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use rocksdb::{prelude::*, DB};
use std::sync::Arc;

// Remember to update the version number here
const MIGRATION_DB_VERSION: &str = "20311116135521";

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
    fn migrate(
        &self,
        db: Arc<DB>,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<Arc<DB>, Error> {
        eprintln!("MigrationObj::migrate ...........");
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
