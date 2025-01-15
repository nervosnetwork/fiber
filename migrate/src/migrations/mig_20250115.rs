use fiber::{
    fiber::config::{
        DEFAULT_TLC_EXPIRY_DELTA, DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS, DEFAULT_TLC_MAX_VALUE,
        DEFAULT_TLC_MIN_VALUE,
    },
    now_timestamp_as_millis_u64,
    store::migration::Migration,
    Error,
};
use indicatif::ProgressBar;
use rocksdb::ops::Iterate;
use rocksdb::ops::Put;
use rocksdb::DB;
use std::sync::Arc;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20250115051223";

pub use fiber_v022::fiber::channel::{
    ChannelTlcInfo as ChannelTlcInfoV022, PublicChannelInfo as PublicChannelInfoV022,
};

pub use fiber_v021::fiber::channel::ChannelActorState as ChannelActorStateV021;
pub use fiber_v022::fiber::channel::ChannelActorState as ChannelActorStateV022;

use crate::util::convert;

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

        const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0;
        let prefix = vec![CHANNEL_ACTOR_STATE_PREFIX];

        for (k, v) in db
            .prefix_iterator(prefix.as_slice())
            .take_while(move |(col_key, _)| col_key.starts_with(prefix.as_slice()))
        {
            let old_channel_state: ChannelActorStateV021 =
                bincode::deserialize(&v).expect("deserialize to old channel state");

            let now_timestamp = now_timestamp_as_millis_u64();
            let local_tlc_info = match old_channel_state.public_channel_info {
                Some(ref info) => ChannelTlcInfoV022 {
                    timestamp: now_timestamp,
                    enabled: info.enabled,
                    tlc_fee_proportional_millionths: info.tlc_fee_proportional_millionths,
                    tlc_expiry_delta: info.tlc_expiry_delta,
                    tlc_minimum_value: info.tlc_min_value,
                    tlc_maximum_value: DEFAULT_TLC_MAX_VALUE,
                },
                None => ChannelTlcInfoV022 {
                    timestamp: now_timestamp,
                    enabled: true,
                    tlc_fee_proportional_millionths: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                    tlc_minimum_value: DEFAULT_TLC_MIN_VALUE,
                    tlc_maximum_value: DEFAULT_TLC_MAX_VALUE,
                },
            };
            let remote_tlc_info = None;
            let public_channel_info =
                old_channel_state
                    .public_channel_info
                    .clone()
                    .map(|info| PublicChannelInfoV022 {
                        local_channel_announcement_signature: info
                            .remote_channel_announcement_signature
                            .clone()
                            .map(convert),
                        remote_channel_announcement_signature: info
                            .remote_channel_announcement_signature
                            .clone()
                            .map(convert),
                        remote_channel_announcement_nonce: info.remote_channel_announcement_nonce,
                        channel_announcement: info.channel_announcement.clone().map(convert),
                        channel_update: info.channel_update.clone().map(convert),
                    });

            let new_channel_state = ChannelActorStateV022 {
                state: convert(old_channel_state.state),
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
                last_commitment_signed_remote_nonce: old_channel_state
                    .last_commitment_signed_remote_nonce,
                last_revoke_and_ack_remote_nonce: old_channel_state
                    .last_revoke_and_ack_remote_nonce,
                last_committed_remote_nonce: old_channel_state.last_committed_remote_nonce,
                latest_commitment_transaction: old_channel_state.latest_commitment_transaction,
                remote_commitment_points: convert(old_channel_state.remote_commitment_points),
                remote_channel_public_keys: convert(old_channel_state.remote_channel_public_keys),
                local_shutdown_info: convert(old_channel_state.local_shutdown_info),
                remote_shutdown_info: convert(old_channel_state.remote_shutdown_info),
                reestablishing: old_channel_state.reestablishing,
                created_at: old_channel_state.created_at,
                // --------- changed fields ----------
                public_channel_info,
                // --------- new fields ----------
                local_tlc_info,
                remote_tlc_info,
            };

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
