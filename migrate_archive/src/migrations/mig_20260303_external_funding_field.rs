use std::sync::Arc;

use indicatif::ProgressBar;
use tracing::info;

use fiber_store::{migration::Migration, StorageBackend, Store, StoreError};
use fiber_v070::fiber::channel::ChannelActorState;

/// Append the trailing `external_funding: Option<ExternalFundingPersistState>`
/// field added to `ChannelActorState`.
///
/// Existing channel actor states should have already been migrated by
/// `mig_20260302_channel_replay_fields`, so they consist of the v0.7.0
/// serialized state plus:
///
///   - `pending_replay_updates: Vec<TlcReplayUpdate>` (default: empty vec)
///   - `last_was_revoke: bool`                        (default: false)
///
/// The default value for `external_funding` is `None`, and bincode 1.x
/// serializes `Option::None` as one 0u8 byte.
///
/// Detection: deserialize as the old v0.7.0 `ChannelActorState`, re-serialize,
/// and compare the stored length with the expected post-replay/pre-external
/// length. If the stored value already has extra bytes beyond that size, the
/// entry was already migrated or is from a newer format, so skip it.
const MIGRATION_DB_VERSION: &str = "20260303100000";
const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
const REPLAY_FIELD_SUFFIX_LEN: usize = 9;
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

fn append_external_funding_none(value: &[u8]) -> Vec<u8> {
    let mut migrated = value.to_vec();
    migrated.push(EXTERNAL_FUNDING_NONE);
    migrated
}

pub(crate) fn migrate_channel_state_external_funding(value: &[u8]) -> Option<Vec<u8>> {
    let old_state: ChannelActorState = bincode::deserialize(value).ok()?;
    let old_bytes = bincode::serialize(&old_state).expect("re-serialize v0.7.0 ChannelActorState");
    let expected_pre_external_len = old_bytes.len() + REPLAY_FIELD_SUFFIX_LEN;

    if value.len() != expected_pre_external_len {
        return None;
    }

    Some(append_external_funding_none(value))
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
        let mut skipped_count = 0;

        for (key, value) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(prefix.as_slice()))
        {
            let Some(new_value) = migrate_channel_state_external_funding(&value) else {
                skipped_count += 1;
                continue;
            };

            db.put(key, new_value);
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

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_v070::fiber::channel::ChannelActorState as ChannelActorStateV070;
    use fiber_v070::store::sample::StoreSample;

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

        let migrated = migrate_channel_state_external_funding(&old_bytes)
            .expect("replay-migrated v0.7.0 state should migrate");

        assert_eq!(migrated.len(), old_bytes.len() + 1);
        assert_eq!(migrated.last(), Some(&EXTERNAL_FUNDING_NONE));
    }

    #[test]
    fn preserves_existing_channel_state_bytes_before_external_funding_field() {
        let old_bytes = replay_migrated_channel_state_bytes();
        let migrated = migrate_channel_state_external_funding(&old_bytes)
            .expect("replay-migrated v0.7.0 state should migrate");

        assert_eq!(&migrated[..old_bytes.len()], old_bytes.as_slice());
    }

    #[test]
    fn skips_plain_v070_channel_state_before_replay_fields() {
        let state = ChannelActorStateV070::samples(42)
            .into_iter()
            .next()
            .expect("ChannelActorState samples should not be empty");
        let bytes = bincode::serialize(&state).expect("serialize v0.7.0 sample");

        assert!(migrate_channel_state_external_funding(&bytes).is_none());
    }

    #[test]
    fn skips_already_external_funding_migrated_channel_state() {
        let old_bytes = replay_migrated_channel_state_bytes();
        let migrated = append_external_funding_none(&old_bytes);

        assert!(migrate_channel_state_external_funding(&migrated).is_none());
    }

    #[test]
    fn skips_invalid_channel_state_bytes() {
        assert!(migrate_channel_state_external_funding(&[1, 2, 3]).is_none());
    }
}
