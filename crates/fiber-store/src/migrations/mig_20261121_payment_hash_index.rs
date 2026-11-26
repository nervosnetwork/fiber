//! Build the payment hash index for channels stored before it existed.
use bincode::Options;
use fiber_types::schema::{
    channel_payment_hashes_key, payment_hash_channel_id_key, CHANNEL_ACTOR_STATE_PREFIX,
    CHANNEL_PAYMENT_HASHES_PREFIX, PAYMENT_HASH_CHANNEL_ID_PREFIX,
};
use fiber_types::{ChannelMutableData, Hash256};

use crate::migration::{Migration, MigrationStore};

const MIGRATION_DB_VERSION: &str = "20261121000001";
const BATCH_SIZE: usize = 1024;

#[derive(Default)]
pub struct MigrationObj;

impl MigrationObj {
    pub fn new() -> Self {
        Self
    }
}

impl Migration for MigrationObj {
    fn version(&self) -> &str {
        MIGRATION_DB_VERSION
    }

    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        // Start from no rows, so a rerun after an interrupted migration gives the same index.
        let mut deletes = Vec::new();
        for prefix in [PAYMENT_HASH_CHANNEL_ID_PREFIX, CHANNEL_PAYMENT_HASHES_PREFIX] {
            deletes.extend(store.iter_prefix(&[prefix]).map(|(key, _)| key));
        }
        for chunk in deletes.chunks(BATCH_SIZE) {
            store.write_batch(&[], chunk);
        }

        // The earlier migration split channel records; prefix 0 holds the mutable half, which
        // carries the TLCs and the V2 session.
        let mut puts = Vec::new();
        for (key, value) in store.iter_prefix(&[CHANNEL_ACTOR_STATE_PREFIX]) {
            let channel_id: [u8; 32] = key[1..]
                .try_into()
                .map_err(|_| format!("Invalid channel state key {}", hex(&key)))?;
            let channel_id = Hash256::from(channel_id);
            let channel: ChannelMutableData = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_limit(value.len() as u64)
                .reject_trailing_bytes()
                .deserialize(&value)
                .map_err(|error| format!("Failed to decode channel {channel_id}: {error}"))?;
            let hashes = channel.pending_payment_hashes();
            if hashes.is_empty() {
                continue;
            }
            for hash in &hashes {
                puts.push((payment_hash_channel_id_key(hash, &channel_id), Vec::new()));
            }
            let hashes: Vec<Hash256> = hashes.into_iter().collect();
            let value = bincode::serialize(&hashes)
                .map_err(|error| format!("Failed to encode payment hashes: {error}"))?;
            puts.push((channel_payment_hashes_key(&channel_id), value));
            if puts.len() >= BATCH_SIZE {
                store.write_batch(&puts, &[]);
                puts.clear();
            }
        }
        store.write_batch(&puts, &[]);
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
