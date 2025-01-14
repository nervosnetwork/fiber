use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use rocksdb::ops::Iterate;
use rocksdb::ops::Put;
use rocksdb::DB;
use std::sync::Arc;
use tracing::debug;

const MIGRATION_DB_VERSION: &str = "20250112205923";

pub use fiber_v020::fiber::channel::ChannelActorState as ChannelActorStateV020;
pub use fiber_v021::fiber::channel::ChannelActorState as ChannelActorStateV021;

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
        eprintln!("MigrationObj::migrate ...........");

        const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            debug!(
                key = hex::encode(k.as_ref()),
                value = hex::encode(v.as_ref()),
                "Obtained old channel state"
            );
            let old_channel_state: ChannelActorStateV020 =
                bincode::deserialize(&v).expect("deserialize to old channel state");

            let mut all_remote_nonces = old_channel_state.remote_nonces.clone();
            all_remote_nonces.sort_by(|a, b| b.0.cmp(&a.0));
            let last_committed_remote_nonce =
                all_remote_nonces.get(0).map(|(_, nonce)| nonce).cloned();

            // Depending on whether the receiver has received our RevokeAndAck message or not,
            // we need to set different last_revoke_and_ack_remote_nonce.
            // 1. The receiver has received our RevokeAndAck message.
            //    In this case, the last_revoke_and_ack_remote_nonce should be the remote nonce
            //    with the largest commitment number.
            // 2. The receiver has not received our RevokeAndAck message.
            //    In this case, the last_revoke_and_ack_remote_nonce should be the remote nonce
            //    with the second largest commitment number.
            // We can't determine which case is true unless we receive a Reestablish message from the peer,
            // which contains the remote's view on the last commitment number.
            // So this migration is not perfect, but it's the best we can do.
            let last_revoke_and_ack_remote_nonce =
                all_remote_nonces.get(0).map(|(_, nonce)| nonce).cloned();

            let serialized = serde_json::to_string(&old_channel_state).unwrap();
            let mut new_channel_state: ChannelActorStateV021 =
                serde_json::from_str(&serialized).expect("deserialize to new state");
            new_channel_state.last_revoke_and_ack_remote_nonce = last_revoke_and_ack_remote_nonce;
            new_channel_state.last_committed_remote_nonce = last_committed_remote_nonce;

            new_channel_state.last_commitment_signed_remote_nonce =
                old_channel_state.last_used_nonce_in_commitment_signed;

            let new_channel_state_bytes =
                bincode::serialize(&new_channel_state).expect("serialize to new channel state");
            db.put(k, new_channel_state_bytes)
                .expect("save new channel state");
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
