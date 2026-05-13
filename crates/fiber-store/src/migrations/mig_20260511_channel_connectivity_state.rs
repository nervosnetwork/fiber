use crate::migration::{Migration, MigrationStore};
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260511120000";

const CHANNEL_ACTOR_STATE_PREFIX: &[u8] = &[0x00];

pub use fiber_types_081::channel::ChannelActorData as OldChannelActorData;
pub use fiber_types_090::channel::ChannelActorData as NewChannelActorData;

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
    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        info!(
            "Migrating to {}: adding connectivity_state to ChannelActorData ...",
            MIGRATION_DB_VERSION
        );

        let entries = store.collect_prefix(CHANNEL_ACTOR_STATE_PREFIX);
        let total = entries.len();
        let mut migrated = 0u64;
        let mut skipped = 0u64;

        for (key, value) in entries {
            if let Ok(_new) = bincode::deserialize::<NewChannelActorData>(&value) {
                skipped += 1;
                continue;
            }

            let old: OldChannelActorData = bincode::deserialize(&value).map_err(|e| {
                format!(
                    "Failed to deserialize old ChannelActorData: {}",
                    e
                )
            })?;

            let mut json_value = serde_json::to_value(&old).map_err(|e| {
                format!(
                    "Failed to serialize old ChannelActorData to JSON: {}",
                    e
                )
            })?;

            json_value
                .as_object_mut()
                .ok_or("Expected JSON object")?
                .insert(
                    "connectivity_state".to_string(),
                    serde_json::json!("Offline"),
                );

            let new: NewChannelActorData =
                serde_json::from_value(json_value).map_err(|e| {
                    format!(
                        "Failed to deserialize new ChannelActorData from JSON: {}",
                        e
                    )
                })?;

            let new_bytes = bincode::serialize(&new).map_err(|e| {
                format!(
                    "Failed to serialize new ChannelActorData: {}",
                    e
                )
            })?;

            store.put(&key, &new_bytes);
            migrated += 1;
        }

        info!(
            "Migration {} complete: {} migrated, {} skipped ({} total)",
            MIGRATION_DB_VERSION, migrated, skipped, total
        );
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}
