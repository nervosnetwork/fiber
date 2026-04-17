use fiber_store::{migration::Migration, StorageBackend, Store, StoreError};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

/// Append one new trailing field to every serialized `ChannelActorData`:
///
///   - `connectivity_state: ChannelConnectivityState` (default: `Offline`)
///
/// `ChannelConnectivityState` is serialized by bincode 1.x as a little-endian
/// `u32` enum discriminant. Its variants are:
///
///   - `Online`  => 0u32 => `[0, 0, 0, 0]`
///   - `Offline` => 1u32 => `[1, 0, 0, 0]`
///   - `Syncing` => 2u32 => `[2, 0, 0, 0]`
///
/// Because `connectivity_state` is now the last field in `ChannelActorData`,
/// and this migration runs after `mig_20260302_channel_replay_fields`, we can
/// upgrade every stored channel actor state by appending the 4-byte `Offline`
/// suffix directly without deserializing either the old or new struct.
const MIGRATION_DB_VERSION: &str = "20260416120000";
const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
pub(crate) const OFFLINE_CONNECTIVITY_STATE_SUFFIX: [u8; 4] = [1u8, 0, 0, 0];

pub(crate) fn migrate_channel_state_bytes(value: &[u8]) -> Vec<u8> {
    let mut new_bytes = value.to_vec();
    new_bytes.extend_from_slice(&OFFLINE_CONNECTIVITY_STATE_SUFFIX);
    new_bytes
}

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
            "MigrationObj::migrate to {} - appending channel connectivity state ...",
            MIGRATION_DB_VERSION
        );

        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];
        let mut migrated_count = 0;

        for (key, value) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(prefix.as_slice()))
        {
            let new_bytes = migrate_channel_state_bytes(&value);
            db.put(key, new_bytes);
            migrated_count += 1;
        }

        info!(
            "MigrationObj::migrate to {} - migrated {} channel actor states, skipped {}",
            MIGRATION_DB_VERSION, migrated_count, 0
        );

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
