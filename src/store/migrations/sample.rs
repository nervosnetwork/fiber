use crate::{migration::migration::Migration, Error};
use indicatif::ProgressBar;
use rocksdb::DB;
use std::sync::Arc;

const INIT_DB_VERSION: &str = "20351116135521";

pub struct SampleMigration {
    version: String,
}

impl SampleMigration {
    pub fn new() -> Self {
        Self {
            version: INIT_DB_VERSION.to_string(),
        }
    }
}

impl Migration for SampleMigration {
    fn migrate(
        &self,
        db: Arc<DB>,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<Arc<DB>, Error> {
        eprintln!("SampleMigration::migrate ...........");
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn expensive(&self) -> bool {
        false
    }
}
