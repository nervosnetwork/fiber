use fiber_v070::{
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260301103357";
const PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX: u8 = 16;

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
        info!(
            "MigrationObj::migrate to {} - clearing legacy network actor state entries ...",
            MIGRATION_DB_VERSION
        );

        let prefix = vec![PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX];
        let mut batch = db.batch();
        let mut deleted_count = 0;
        for (key, _) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(prefix.as_slice()))
        {
            batch.delete(key.to_vec());
            deleted_count += 1;
        }
        batch.commit();

        info!(
            "MigrationObj::migrate to {} - removed {} legacy network actor state entries",
            MIGRATION_DB_VERSION, deleted_count
        );
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
