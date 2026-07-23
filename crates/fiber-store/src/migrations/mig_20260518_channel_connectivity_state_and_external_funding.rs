use crate::migration::{Migration, MigrationStore};
use super::decode_as_new;
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260518120000";

const CHANNEL_ACTOR_STATE_PREFIX: &[u8] = &[0x00];

pub use fiber_types_081::channel::ChannelActorData as OldChannelActorData;
pub use fiber_types_090::channel::ChannelActorData as NewChannelActorData;

type NewChannelBasePublicKeys = fiber_types_090::channel::ChannelBasePublicKeys;
type NewChannelConstraints = fiber_types_090::channel::ChannelConstraints;
type NewChannelState = fiber_types_090::channel::ChannelState;
type NewChannelTlcInfo = fiber_types_090::channel::ChannelTlcInfo;
type NewCommitmentNumbers = fiber_types_090::channel::CommitmentNumbers;
type NewHash256 = fiber_types_090::Hash256;
type NewInMemorySigner = fiber_types_090::channel::InMemorySigner;
type NewPubkey = fiber_types_090::Pubkey;
type NewPublicChannelInfo = fiber_types_090::channel::PublicChannelInfo;
type NewRevokeAndAck = fiber_types_090::channel::RevokeAndAck;
type NewShutdownInfo = fiber_types_090::channel::ShutdownInfo;
type NewTlcReplayUpdate = fiber_types_090::channel::TlcReplayUpdate;
type NewTlcState = fiber_types_090::channel::TlcState;

fn convert_channel_actor_data(old: OldChannelActorData) -> Result<NewChannelActorData, String> {
    Ok(NewChannelActorData {
        state: decode_as_new::<_, NewChannelState>(old.state)?,
        public_channel_info: old
            .public_channel_info
            .map(decode_as_new::<_, NewPublicChannelInfo>)
            .transpose()?,
        local_tlc_info: decode_as_new::<_, NewChannelTlcInfo>(old.local_tlc_info)?,
        remote_tlc_info: old
            .remote_tlc_info
            .map(decode_as_new::<_, NewChannelTlcInfo>)
            .transpose()?,
        local_pubkey: decode_as_new::<_, NewPubkey>(old.local_pubkey)?,
        remote_pubkey: decode_as_new::<_, NewPubkey>(old.remote_pubkey)?,
        id: decode_as_new::<_, NewHash256>(old.id)?,
        funding_tx: old.funding_tx,
        funding_tx_confirmed_at: old.funding_tx_confirmed_at,
        funding_udt_type_script: old.funding_udt_type_script,
        is_acceptor: old.is_acceptor,
        is_one_way: old.is_one_way,
        to_local_amount: old.to_local_amount,
        to_remote_amount: old.to_remote_amount,
        local_reserved_ckb_amount: old.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: old.remote_reserved_ckb_amount,
        commitment_fee_rate: old.commitment_fee_rate,
        commitment_delay_epoch: old.commitment_delay_epoch,
        funding_fee_rate: old.funding_fee_rate,
        signer: decode_as_new::<_, NewInMemorySigner>(old.signer)?,
        local_channel_public_keys: decode_as_new::<_, NewChannelBasePublicKeys>(
            old.local_channel_public_keys,
        )?,
        commitment_numbers: decode_as_new::<_, NewCommitmentNumbers>(old.commitment_numbers)?,
        local_constraints: decode_as_new::<_, NewChannelConstraints>(old.local_constraints)?,
        remote_constraints: decode_as_new::<_, NewChannelConstraints>(old.remote_constraints)?,
        tlc_state: decode_as_new::<_, NewTlcState>(old.tlc_state)?,
        retryable_tlc_operations: decode_as_new(old.retryable_tlc_operations)?,
        waiting_forward_tlc_tasks: decode_as_new(old.waiting_forward_tlc_tasks)?,
        remote_shutdown_script: old.remote_shutdown_script,
        local_shutdown_script: old.local_shutdown_script,
        last_committed_remote_nonce: old.last_committed_remote_nonce,
        remote_revocation_nonce_for_verify: old.remote_revocation_nonce_for_verify,
        remote_revocation_nonce_for_send: old.remote_revocation_nonce_for_send,
        remote_revocation_nonce_for_next: old.remote_revocation_nonce_for_next,
        latest_commitment_transaction: old.latest_commitment_transaction,
        remote_commitment_points: decode_as_new(old.remote_commitment_points)?,
        remote_channel_public_keys: old
            .remote_channel_public_keys
            .map(decode_as_new::<_, NewChannelBasePublicKeys>)
            .transpose()?,
        local_shutdown_info: old
            .local_shutdown_info
            .map(decode_as_new::<_, NewShutdownInfo>)
            .transpose()?,
        remote_shutdown_info: old
            .remote_shutdown_info
            .map(decode_as_new::<_, NewShutdownInfo>)
            .transpose()?,
        shutdown_transaction_hash: old.shutdown_transaction_hash,
        reestablishing: old.reestablishing,
        last_revoke_ack_msg: old
            .last_revoke_ack_msg
            .map(decode_as_new::<_, NewRevokeAndAck>)
            .transpose()?,
        created_at: old.created_at,
        pending_replay_updates: old
            .pending_replay_updates
            .into_iter()
            .map(decode_as_new::<_, NewTlcReplayUpdate>)
            .collect::<Result<Vec<_>, _>>()?,
        last_was_revoke: old.last_was_revoke,
        connectivity_state: fiber_types_090::channel::ChannelConnectivityState::Offline,
        external_funding: None,
    })
}

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

impl Migration for MigrationObj {
    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        info!(
            "Migrating to {}: adding connectivity_state and external_funding to ChannelActorData ...",
            MIGRATION_DB_VERSION
        );

        let entries = store.collect_prefix(CHANNEL_ACTOR_STATE_PREFIX);
        let total = entries.len();
        let mut migrated = 0u64;
        let mut skipped = 0u64;

        for (key, value) in entries {
            if let Ok(_new) = bincode::deserialize::<NewChannelActorData>(&value) {
                skipped += 1;
                continue;
            }

            let old: OldChannelActorData = bincode::deserialize(&value).map_err(|e| {
                format!(
                    "Failed to deserialize old ChannelActorData: {}",
                    e
                )
            })?;

            let new = convert_channel_actor_data(old)?;

            let new_bytes = bincode::serialize(&new).map_err(|e| {
                format!(
                    "Failed to serialize new ChannelActorData: {}",
                    e
                )
            })?;

            store.put(&key, &new_bytes);
            migrated += 1;
        }

        info!(
            "Migration {} complete: {} migrated, {} skipped ({} total)",
            MIGRATION_DB_VERSION, migrated, skipped, total
        );
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}

#[cfg(test)]
mod tests {
    use super::{Migration, MigrationObj, NewChannelActorData, OldChannelActorData};
    use crate::backend::StorageBackend;
    use fiber_types_081::sample::StoreSample;

    const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0x00;

    fn gen_path() -> std::path::PathBuf {
        let tmp_dir = tempfile::Builder::new()
            .prefix("test-channel-connectivity-migration")
            .tempdir()
            .unwrap();
        tmp_dir.as_ref().to_path_buf()
    }

    fn gen_store() -> crate::Store {
        let path = gen_path();
        crate::Store::open_db(&path).unwrap()
    }

    #[test]
    fn migrates_channel_actor_data_with_large_u128_fields() {
        let store = gen_store();
        let mut old = OldChannelActorData::samples(42)
            .into_iter()
            .next()
            .expect("sample channel actor data");
        old.local_constraints.max_tlc_value_in_flight = u128::MAX;
        old.remote_constraints.max_tlc_value_in_flight = u128::MAX;

        let key = vec![CHANNEL_ACTOR_STATE_PREFIX, 1];
        let old_bytes = bincode::serialize(&old).expect("serialize old channel actor data");
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new().migrate(&store).expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value");
        let new: NewChannelActorData =
            bincode::deserialize(&new_bytes).expect("deserialize migrated channel actor data");

        assert_eq!(new.local_constraints.max_tlc_value_in_flight, u128::MAX);
        assert_eq!(new.remote_constraints.max_tlc_value_in_flight, u128::MAX);
        assert_eq!(
            new.connectivity_state,
            fiber_types_090::channel::ChannelConnectivityState::Offline
        );
        assert!(new.external_funding.is_none());
    }
}
