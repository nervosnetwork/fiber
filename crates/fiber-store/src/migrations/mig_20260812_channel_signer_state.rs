use tracing::info;

use crate::migration::{Migration, MigrationStore};

use super::decode_as_new;

const MIGRATION_DB_VERSION: &str = "20260812120000";

const CHANNEL_ACTOR_STATE_PREFIX: &[u8] = &[0x00];

pub use fiber_types_090::channel::ChannelActorData as OldChannelActorData;
pub use fiber_types_current::channel::ChannelActorData as NewChannelActorData;

fn convert_channel_actor_data(old: OldChannelActorData) -> Result<NewChannelActorData, String> {
    Ok(NewChannelActorData {
        state: decode_as_new(old.state)?,
        signer_state: fiber_types_current::ChannelSignerState::Internal,
        public_channel_info: old
            .public_channel_info
            .map(decode_as_new)
            .transpose()?,
        local_tlc_info: decode_as_new(old.local_tlc_info)?,
        remote_tlc_info: old.remote_tlc_info.map(decode_as_new).transpose()?,
        local_pubkey: decode_as_new(old.local_pubkey)?,
        remote_pubkey: decode_as_new(old.remote_pubkey)?,
        id: decode_as_new(old.id)?,
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
        signer: decode_as_new(old.signer)?,
        local_channel_public_keys: decode_as_new(old.local_channel_public_keys)?,
        local_commitment_points: Default::default(),
        local_public_nonces: Default::default(),
        commitment_numbers: decode_as_new(old.commitment_numbers)?,
        local_constraints: decode_as_new(old.local_constraints)?,
        remote_constraints: decode_as_new(old.remote_constraints)?,
        tlc_state: decode_as_new(old.tlc_state)?,
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
            .map(decode_as_new)
            .transpose()?,
        local_shutdown_info: old.local_shutdown_info.map(decode_as_new).transpose()?,
        remote_shutdown_info: old.remote_shutdown_info.map(decode_as_new).transpose()?,
        shutdown_transaction_hash: old.shutdown_transaction_hash,
        reestablishing: old.reestablishing,
        last_revoke_ack_msg: old.last_revoke_ack_msg.map(decode_as_new).transpose()?,
        created_at: old.created_at,
        pending_replay_updates: old
            .pending_replay_updates
            .into_iter()
            .map(decode_as_new)
            .collect::<Result<Vec<_>, _>>()?,
        last_was_revoke: old.last_was_revoke,
        connectivity_state: decode_as_new(old.connectivity_state)?,
        external_funding: old.external_funding.map(decode_as_new).transpose()?,
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
            "Migrating to {}: adding signer_state to ChannelActorData ...",
            MIGRATION_DB_VERSION
        );

        let entries = store.collect_prefix(CHANNEL_ACTOR_STATE_PREFIX);
        let total = entries.len();
        let mut migrated = 0u64;
        let mut skipped = 0u64;

        for (key, value) in entries {
            if bincode::deserialize::<NewChannelActorData>(&value).is_ok() {
                skipped += 1;
                continue;
            }

            let old: OldChannelActorData = bincode::deserialize(&value)
                .map_err(|e| format!("Failed to deserialize old ChannelActorData: {e}"))?;
            let new = convert_channel_actor_data(old)?;
            let new_bytes = bincode::serialize(&new)
                .map_err(|e| format!("Failed to serialize new ChannelActorData: {e}"))?;

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
    use fiber_types_090::sample::StoreSample;
    use fiber_types_current::ChannelSignerState;

    use crate::backend::StorageBackend;

    use super::{Migration, MigrationObj, NewChannelActorData, OldChannelActorData};

    const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0x00;

    fn gen_store() -> crate::Store {
        let tmp_dir = tempfile::Builder::new()
            .prefix("test-channel-signer-state-migration")
            .tempdir()
            .unwrap();
        crate::Store::open_db(tmp_dir.path()).unwrap()
    }

    #[test]
    fn migrates_existing_channels_to_internal_signer_state() {
        let store = gen_store();
        let old_samples = OldChannelActorData::samples(42);
        let expected = old_samples
            .iter()
            .map(|old| {
                (
                    old.id,
                    old.to_local_amount,
                    bincode::serialize(&old.commitment_numbers).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let keys = old_samples
            .into_iter()
            .enumerate()
            .map(|(index, old)| {
                let key = vec![CHANNEL_ACTOR_STATE_PREFIX, index as u8 + 1];
                let old_bytes =
                    bincode::serialize(&old).expect("serialize old channel actor data");
                StorageBackend::put(&store, &key, &old_bytes);
                key
            })
            .collect::<Vec<_>>();

        let migration = MigrationObj::new();
        migration.migrate(&store).expect("migration should succeed");

        let migrated_bytes = keys
            .iter()
            .zip(expected)
            .map(|(key, (expected_id, expected_to_local_amount, expected_commitment_numbers))| {
                let new_bytes = StorageBackend::get(&store, key).expect("migrated value");
                let new: NewChannelActorData = bincode::deserialize(&new_bytes)
                    .expect("deserialize migrated channel actor data");
                assert!(matches!(new.signer_state, ChannelSignerState::Internal));
                assert_eq!(new.id.as_ref(), expected_id.as_ref());
                assert_eq!(new.to_local_amount, expected_to_local_amount);
                assert_eq!(
                    bincode::serialize(&new.commitment_numbers).unwrap(),
                    expected_commitment_numbers
                );
                new_bytes
            })
            .collect::<Vec<_>>();

        migration
            .migrate(&store)
            .expect("running migration twice should safely skip new data");
        for (key, expected_bytes) in keys.iter().zip(migrated_bytes) {
            assert_eq!(
                StorageBackend::get(&store, key).expect("migrated value after retry"),
                expected_bytes
            );
        }
    }
}
