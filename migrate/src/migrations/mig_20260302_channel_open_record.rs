use fiber_store::{migration::Migration, BatchWriter, StorageBackend, Store, StoreError};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

/// Delete all `ChannelOpenRecord` entries (prefix 201).
///
/// These are ephemeral records that track in-progress channel openings.
/// The v0.8.0-rc1 schema changed the `peer_id: PeerId` field to
/// `pubkey: Pubkey`, making the old binary format incompatible.
/// Since these records are transient status indicators (not long-lived data),
/// deleting them is safe—any ongoing channel opens will simply lose their
/// progress-tracking state and will be re-created on the next attempt.
const MIGRATION_DB_VERSION: &str = "20260302100001";
const CHANNEL_OPEN_RECORD_PREFIX: u8 = 201;

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
    fn migrate<'a>(
        &self,
        db: &'a Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a Store, StoreError> {
        info!(
            "MigrationObj::migrate to {} - clearing channel open record entries ...",
            MIGRATION_DB_VERSION
        );

        let prefix = vec![CHANNEL_OPEN_RECORD_PREFIX];
        let mut batch = db.batch();
        let mut deleted_count = 0;
        for (key, _) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(prefix.as_slice()))
        {
            batch.delete(&key);
            deleted_count += 1;
        }
        batch.commit();

        info!(
            "MigrationObj::migrate to {} - removed {} channel open record entries",
            MIGRATION_DB_VERSION, deleted_count
        );
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
