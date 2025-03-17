use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use rocksdb::ops::{Delete, Get, Iterate, Put};
use rocksdb::DB;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250308000623";

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

        const BROADCAST_MESSAGE_TIMESTAMP_PREFIX: u8 = 97;
        const CHANNEL_ANNOUNCEMENT_PREFIX: u8 = 0;
        const CHANNEL_UPDATE_PREFIX: u8 = 1;
        let channel_announcement_timestamp_prefix = [
            BROADCAST_MESSAGE_TIMESTAMP_PREFIX,
            CHANNEL_ANNOUNCEMENT_PREFIX,
        ];
        let channel_update_timestamp_prefix =
            [BROADCAST_MESSAGE_TIMESTAMP_PREFIX, CHANNEL_UPDATE_PREFIX];

        for (k, channel_update_timestamps) in db
            .prefix_iterator(channel_update_timestamp_prefix.as_slice())
            .take_while(move |(col_key, _)| {
                col_key.starts_with(channel_update_timestamp_prefix.as_slice())
            })
        {
            assert_eq!(channel_update_timestamps.len(), 24);
            let outpoint = &k[2..];
            let channel_announcement_key =
                [channel_announcement_timestamp_prefix.as_slice(), outpoint].concat();
            if let Some(channel_announcement_timestamps) = db
                .get(channel_announcement_timestamp_prefix)
                .expect("read channel announcement timestamps")
            {
                assert_eq!(channel_update_timestamps.len(), 24);
                let mut timestamps = [0u8; 24];
                timestamps[..8].copy_from_slice(&channel_announcement_timestamps[..8]);
                timestamps[8..24].copy_from_slice(&channel_update_timestamps[8..24]);
                db.put(channel_announcement_key, timestamps)
                    .expect("save new channel state");
                db.delete(k).expect("delete old channel state");
            }
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
