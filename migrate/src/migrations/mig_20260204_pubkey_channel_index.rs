use fiber_v070::{
    fiber::channel::ChannelActorState,
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260204120000";
const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
const PEER_ID_CHANNEL_ID_PREFIX: u8 = 64;

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
            "MigrationObj::migrate to {} - rebuilding peer/channel index with pubkey keys ...",
            MIGRATION_DB_VERSION
        );

        let mut batch = db.batch();
        let mut deleted_old_index_count = 0;
        let mut rebuilt_index_count = 0;
        let mut skipped_state_count = 0;

        let index_prefix = vec![PEER_ID_CHANNEL_ID_PREFIX];
        for (key, _) in db
            .prefix_iterator(index_prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(index_prefix.as_slice()))
        {
            batch.delete(key.to_vec());
            deleted_old_index_count += 1;
        }

        let state_prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];
        for (key, value) in db
            .prefix_iterator(state_prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(state_prefix.as_slice()))
        {
            let channel_id = &key[state_prefix.len()..];
            if channel_id.len() != 32 {
                skipped_state_count += 1;
                continue;
            }

            let state: ChannelActorState = match bincode::deserialize(&value) {
                Ok(state) => state,
                Err(_) => {
                    skipped_state_count += 1;
                    continue;
                }
            };

            let pubkey_bytes = state.remote_pubkey.serialize();
            let index_key = [&[PEER_ID_CHANNEL_ID_PREFIX][..], &pubkey_bytes[..], channel_id].concat();
            let index_value =
                bincode::serialize(&state.state).expect("serialize ChannelState should be OK");
            batch.put(index_key, index_value);
            rebuilt_index_count += 1;
        }

        batch.commit();

        info!(
            "MigrationObj::migrate to {} - deleted {} old index entries, rebuilt {} pubkey index entries, skipped {} malformed channel states",
            MIGRATION_DB_VERSION,
            deleted_old_index_count,
            rebuilt_index_count,
            skipped_state_count
        );

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
