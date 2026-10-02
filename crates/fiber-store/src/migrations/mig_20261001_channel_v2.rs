use std::io::Cursor;

use crate::migration::{Migration, MigrationStore, MIGRATION_VERSION_KEY};

const MIGRATION_DB_VERSION: &str = "20261001120000";

/// Preflight for the unpublished channel protocol transition.
pub struct MigrationObj;

impl MigrationObj {
    /// Construct the channel V2 migration.
    pub fn new() -> Self {
        Self
    }
}

impl Default for MigrationObj {
    fn default() -> Self {
        Self::new()
    }
}

impl Migration for MigrationObj {
    fn version(&self) -> &str {
        MIGRATION_DB_VERSION
    }

    fn preflight(&self, store: &dyn MigrationStore) -> Result<(), String> {
        let current = store.get(MIGRATION_VERSION_KEY).is_some_and(|version| {
            version.as_slice() >= MIGRATION_DB_VERSION.as_bytes()
        });
        let mut affected = Vec::new();
        for (_key, value) in store.iter_prefix(&[0x00]) {
            if current {
                let mut cursor = Cursor::new(value.as_slice());
                let channel: fiber_types::ChannelActorData = bincode::deserialize_from(&mut cursor)
                    .map_err(|error| format!("Invalid current channel schema; database preserved: {error}"))?;
                if cursor.position() != value.len() as u64 {
                    return Err(format!("Trailing bytes in current channel {}; database preserved", channel.id));
                }
                fiber_types::channel_v2_validation::validate_channel_v2(&channel)
                    .map_err(|error| format!("Invalid current channel {}: {error}; database preserved", channel.id))?;
                continue;
            }
            // The development layout appended one contract-feature byte to the
            // released 0.9 layout. Decode its actual prefix, rather than assuming
            // the last byte belongs to a feature (or decoding with future types).
            let mut cursor = Cursor::new(value.as_slice());
            if let Ok(channel) = bincode::deserialize_from::<_, fiber_types_090::ChannelActorData>(&mut cursor) {
                if cursor.position() < value.len() as u64 {
                    let features: u8 = bincode::deserialize_from(&mut cursor)
                        .map_err(|error| format!("Invalid stored channel features: {error}"))?;
                    if features != 0 {
                        affected.push(channel.id.to_string());
                    }
                }
            } else if bincode::deserialize::<fiber_types_081::ChannelActorData>(&value).is_err() {
                return Err("Unrecognized pre-V2 channel record; database preserved".to_owned());
            }
        }
        if affected.is_empty() {
            Ok(())
        } else {
            affected.sort();
            Err(format!(
                "Cannot upgrade unpublished full-hash V1 channels to V2; recover with the original binary. Affected channel IDs: {}. Database records and version preserved.",
                affected.join(", ")
            ))
        }
    }

    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        self.preflight(store)?;
        // Prepare every conversion before writing any record. Existing released
        // channels keep their exact legacy payload and gain the zero feature byte
        // and serialized None session tag, which serde(default) cannot supply at EOF.
        let mut converted = Vec::new();
        for (key, value) in store.collect_prefix(&[0x00]) {
            let mut cursor = Cursor::new(value.as_slice());
            let _: fiber_types_090::ChannelActorData = bincode::deserialize_from(&mut cursor)
                .map_err(|error| format!("Cannot decode legacy channel: {error}"))?;
            let prefix_len = cursor.position() as usize;
            let mut new = value[..prefix_len].to_vec();
            new.push(0);
            new.extend(bincode::serialize(&Option::<()>::None)
                .map_err(|error| format!("Cannot encode empty V2 session: {error}"))?);
            converted.push((key, new));
        }
        for (key, value) in converted {
            store.put(&key, &value);
        }
        Ok(())
    }
}
