use std::sync::Arc;

use fiber_store::{migration::Migration, StorageBackend, Store, StoreError};
use indicatif::ProgressBar;
use tracing::info;

/// Append the trailing `external_funding: Option<ExternalFundingPersistState>`
/// field added to `ChannelActorState`.
///
/// Existing channel actor states do not have this field. The default value is
/// `None`, and bincode 1.x serializes `Option::None` as one 0u8 byte.
const MIGRATION_DB_VERSION: &str = "20260303100000";
const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
const EXTERNAL_FUNDING_NONE: u8 = 0;

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

pub(crate) fn append_external_funding_none(value: &[u8]) -> Vec<u8> {
    let mut migrated = value.to_vec();
    migrated.push(EXTERNAL_FUNDING_NONE);
    migrated
}

impl Migration for MigrationObj {
    fn migrate<'a>(
        &self,
        db: &'a Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a Store, StoreError> {
        info!(
            "MigrationObj::migrate to {} - appending external funding field ...",
            MIGRATION_DB_VERSION
        );

        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];
        let mut migrated_count = 0;

        for (key, value) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(prefix.as_slice()))
        {
            db.put(key, append_external_funding_none(&value));
            migrated_count += 1;
        }

        info!(
            "MigrationObj::migrate to {} - migrated {} channel actor states",
            MIGRATION_DB_VERSION, migrated_count
        );

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_v070::fiber::channel::ChannelActorState as ChannelActorStateV070;
    use fiber_v070::store::sample::StoreSample;

    const REPLAY_FIELD_SUFFIX_LEN: usize = 9;

    fn replay_migrated_channel_state_bytes() -> Vec<u8> {
        let state = ChannelActorStateV070::samples(42)
            .into_iter()
            .next()
            .expect("ChannelActorState samples should not be empty");
        let mut bytes = bincode::serialize(&state).expect("serialize v0.7.0 sample");
        bytes.extend_from_slice(&[0u8; REPLAY_FIELD_SUFFIX_LEN]);
        bytes
    }

    #[test]
    fn appends_external_funding_none_to_replay_migrated_channel_state() {
        let old_bytes = replay_migrated_channel_state_bytes();

        let migrated = append_external_funding_none(&old_bytes);

        assert_eq!(migrated.len(), old_bytes.len() + 1);
        assert_eq!(migrated.last(), Some(&EXTERNAL_FUNDING_NONE));
    }

    #[test]
    fn preserves_existing_channel_state_bytes_before_external_funding_field() {
        let old_bytes = replay_migrated_channel_state_bytes();
        let migrated = append_external_funding_none(&replay_migrated_channel_state_bytes());

        assert_eq!(&migrated[..old_bytes.len()], old_bytes.as_slice());
    }
}
