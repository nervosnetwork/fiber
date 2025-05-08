use fiber::{store::migration::Migration, Error};
use indicatif::ProgressBar;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250112205923";

use crate::util::convert;
pub use fiber_v020::fiber::channel::ChannelActorState as ChannelActorStateV020;
pub use fiber_v021::fiber::channel::ChannelActorState as ChannelActorStateV021;
pub use fiber_v030::fiber::channel::ChannelActorState as ChannelActorStateV030;

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
        db: &'a fiber::store::Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a fiber::store::Store, Error> {
        info!(
            "MigrationObj::migrate to {} ...........",
            MIGRATION_DB_VERSION
        );

        const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.clone().as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            // there maybe some existing nodes didn't set correct db version,
            // if we can deserialize the data correctly, just skip it.
            if let Ok(_) = bincode::deserialize::<ChannelActorStateV030>(&v) {
                continue;
            }

            if let Ok(_) = bincode::deserialize::<ChannelActorStateV021>(&v) {
                continue;
            }

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

            let new_channel_state = ChannelActorStateV021 {
                state: convert(old_channel_state.state),
                public_channel_info: old_channel_state
                    .public_channel_info
                    .map(|info| convert(info)),
                local_pubkey: convert(old_channel_state.local_pubkey),
                remote_pubkey: convert(old_channel_state.remote_pubkey),
                id: convert(old_channel_state.id),
                funding_tx: old_channel_state.funding_tx,
                funding_tx_confirmed_at: old_channel_state.funding_tx_confirmed_at,
                funding_udt_type_script: old_channel_state.funding_udt_type_script,
                is_acceptor: old_channel_state.is_acceptor,
                to_local_amount: old_channel_state.to_local_amount,
                to_remote_amount: old_channel_state.to_remote_amount,
                local_reserved_ckb_amount: old_channel_state.local_reserved_ckb_amount,
                remote_reserved_ckb_amount: old_channel_state.remote_reserved_ckb_amount,
                commitment_fee_rate: old_channel_state.commitment_fee_rate,
                commitment_delay_epoch: old_channel_state.commitment_delay_epoch,
                funding_fee_rate: old_channel_state.funding_fee_rate,
                signer: convert(old_channel_state.signer),
                local_channel_public_keys: convert(old_channel_state.local_channel_public_keys),
                commitment_numbers: convert(old_channel_state.commitment_numbers),
                local_constraints: convert(old_channel_state.local_constraints),
                remote_constraints: convert(old_channel_state.remote_constraints),
                tlc_state: convert(old_channel_state.tlc_state),
                remote_shutdown_script: old_channel_state.remote_shutdown_script,
                local_shutdown_script: old_channel_state.local_shutdown_script,
                // ++++++++ new fields +++++++++++
                last_commitment_signed_remote_nonce: old_channel_state
                    .last_used_nonce_in_commitment_signed,
                last_revoke_and_ack_remote_nonce,
                last_committed_remote_nonce,
                // --------- new fields ----------
                latest_commitment_transaction: old_channel_state.latest_commitment_transaction,
                remote_commitment_points: convert(old_channel_state.remote_commitment_points),
                remote_channel_public_keys: convert(old_channel_state.remote_channel_public_keys),
                local_shutdown_info: convert(old_channel_state.local_shutdown_info),
                remote_shutdown_info: convert(old_channel_state.remote_shutdown_info),
                reestablishing: old_channel_state.reestablishing,
                created_at: old_channel_state.created_at,
            };

            let new_channel_state_bytes =
                bincode::serialize(&new_channel_state).expect("serialize to new channel state");
            db.put(k, new_channel_state_bytes);
        }
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }
}
