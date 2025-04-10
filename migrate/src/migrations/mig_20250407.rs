use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use rocksdb::ops::Delete;
use rocksdb::ops::Iterate;
use rocksdb::DB;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250407133930";

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
        info!(
            "MigrationObj::migrate to {} ...........",
            MIGRATION_DB_VERSION
        );

        const BROADCAST_MESSAGE_PREFIX: u8 = 96;
        let prefix = vec![BROADCAST_MESSAGE_PREFIX];

        for (k, _v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            // just delete the old broadcast message
            db.delete(k).expect("delete old broadcast message");
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
