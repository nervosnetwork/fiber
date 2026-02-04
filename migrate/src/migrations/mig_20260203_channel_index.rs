use ckb_types::prelude::Entity;
use fiber_v070::{
    fiber::payment::{Attempt, PaymentSession},
    store::{migration::Migration, Store},
    Error,
};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

// Build channel -> attempts index for non-final payments
// This migration creates the index entries for payments that are still in progress
const MIGRATION_DB_VERSION: &str = "20260203131851";

// Storage prefixes
const PAYMENT_SESSION_PREFIX: u8 = 192;
const ATTEMPT_PREFIX: u8 = 195;
const ATTEMPT_CHANNEL_INDEX_PREFIX: u8 = 196;

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
            "MigrationObj::migrate to {} - building channel index for non-final payments ...",
            MIGRATION_DB_VERSION
        );

        let mut batch = db.batch();
        let mut index_count = 0;

        // Iterate over all payment sessions
        let session_prefix = vec![PAYMENT_SESSION_PREFIX];
        for (_, value) in db
            .prefix_iterator(session_prefix.as_slice())
            .take_while(|(key, _)| key.starts_with(session_prefix.as_slice()))
        {
            // Deserialize payment session
            let session: PaymentSession = match bincode::deserialize(&value) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Skip final payments (Success or Failed)
            if session.status.is_final() {
                continue;
            }

            // Get all attempts for this payment
            let payment_hash = session.payment_hash();
            let attempt_prefix = [&[ATTEMPT_PREFIX], payment_hash.as_ref()].concat();

            for (_, attempt_value) in db
                .prefix_iterator(attempt_prefix.as_slice())
                .take_while(|(key, _)| key.starts_with(attempt_prefix.as_slice()))
            {
                let attempt: Attempt = match bincode::deserialize(&attempt_value) {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                // Create channel index entry if attempt has first hop channel outpoint
                // first_hop_channel_outpoint() was added after v0.6.1, so we access the route directly
                if let Some(first_node) = attempt.route.nodes.first() {
                    let outpoint = &first_node.channel_outpoint;
                    let index_key = [
                        &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                        outpoint.as_slice(),
                        attempt.payment_hash.as_ref(),
                        &attempt.id.to_le_bytes(),
                    ]
                    .concat();
                    batch.put(index_key, []);
                    index_count += 1;
                }
            }
        }

        batch.commit();

        info!(
            "MigrationObj::migrate to {} - created {} channel index entries for non-final payments",
            MIGRATION_DB_VERSION, index_count
        );

        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
