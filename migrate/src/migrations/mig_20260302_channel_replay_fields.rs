use fiber_v070::{
    fiber::channel::ChannelActorState,
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

/// Append two new trailing fields to every serialized `ChannelActorData`:
///
///   - `pending_replay_updates: Vec<TlcReplayUpdate>` (default: empty vec)
///   - `last_was_revoke: bool`                        (default: false)
///
/// bincode 1.x serializes `Vec::new()` as 8 zero bytes (u64 length = 0)
/// and `false` as a single 0u8 byte, totalling 9 bytes.
///
/// Detection: deserialize as the old v0.7.0 `ChannelActorState`, re-serialize,
/// and compare lengths. If the stored value already has extra bytes beyond the
/// v0.7.0 size, the entry was already migrated—skip it.
const MIGRATION_DB_VERSION: &str = "20260302100000";
const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;

/// bincode encoding of the two new default-value fields:
///   - `Vec::<_>::new()`: 8 bytes (u64 length = 0)
///   - `false`:           1 byte  (0u8)
const NEW_FIELD_SUFFIX: [u8; 9] = [0u8; 9];

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
    ) -> Result<&'a Store, Error> {
        info!(
            "MigrationObj::migrate to {} - appending replay fields to channel actor state ...",
            MIGRATION_DB_VERSION
        );

        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];
        let mut migrated_count = 0;
        let mut skipped_count = 0;

        for (key, value) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(prefix.as_slice()))
        {
            // Try to deserialize as v0.7.0 format to validate the entry and
            // compute the expected serialized length without the new fields.
            let old_state: ChannelActorState = match bincode::deserialize(&value) {
                Ok(s) => s,
                Err(_) => {
                    // Cannot parse as v0.7.0 — entry is either already migrated,
                    // corrupt, or from a newer format.  Skip it.
                    skipped_count += 1;
                    continue;
                }
            };

            let old_bytes =
                bincode::serialize(&old_state).expect("re-serialize v0.7.0 ChannelActorState");

            if value.len() > old_bytes.len() {
                // Already has extra bytes (likely already migrated).
                skipped_count += 1;
                continue;
            }

            // Append the 9-byte suffix for the two new default fields.
            let mut new_bytes = value.to_vec();
            new_bytes.extend_from_slice(&NEW_FIELD_SUFFIX);
            db.put(key, new_bytes);
            migrated_count += 1;
        }

        info!(
            "MigrationObj::migrate to {} - migrated {} channel actor states, skipped {}",
            MIGRATION_DB_VERSION, migrated_count, skipped_count
        );

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
