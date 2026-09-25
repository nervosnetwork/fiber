use super::decode_as_new;
use crate::migration::{Migration, MigrationStore};
use tracing::info;

const MIGRATION_DB_VERSION: &str = "20260925120000";

const CHANNEL_ACTOR_STATE_PREFIX: &[u8] = &[0x00];
const CHANNEL_OPEN_RECORD_PREFIX: &[u8] = &[201];
const WATCHTOWER_CHANNEL_PREFIX: &[u8] = &[224];

// Old (0.9.1) types, serialized without the `commitment_contract_features` field.
pub use fiber_types_090::channel::ChannelActorData as OldChannelActorData;
pub use fiber_types_090::channel::ChannelOpenRecord as OldChannelOpenRecord;
pub use fiber_types_090::watchtower::ChannelData as OldChannelData;

// New (0.10.0-rc1) types, with `commitment_contract_features` (defaults to Legacy).
pub use fiber_types_0100::channel::ChannelActorData as NewChannelActorData;
pub use fiber_types_0100::channel::ChannelOpenRecord as NewChannelOpenRecord;
pub use fiber_types_0100::watchtower::ChannelData as NewChannelData;

type NewChannelState = fiber_types_0100::channel::ChannelState;
type NewPublicChannelInfo = fiber_types_0100::channel::PublicChannelInfo;
type NewChannelTlcInfo = fiber_types_0100::channel::ChannelTlcInfo;
type NewHash256 = fiber_types_0100::Hash256;
type NewPubkey = fiber_types_0100::Pubkey;
type NewPrivkey = fiber_types_0100::Privkey;
type NewInMemorySigner = fiber_types_0100::channel::InMemorySigner;
type NewChannelBasePublicKeys = fiber_types_0100::channel::ChannelBasePublicKeys;
type NewCommitmentNumbers = fiber_types_0100::channel::CommitmentNumbers;
type NewChannelConstraints = fiber_types_0100::channel::ChannelConstraints;
type NewTlcState = fiber_types_0100::channel::TlcState;
type NewShutdownInfo = fiber_types_0100::channel::ShutdownInfo;
type NewRevokeAndAck = fiber_types_0100::channel::RevokeAndAck;
type NewTlcReplayUpdate = fiber_types_0100::channel::TlcReplayUpdate;
type NewChannelConnectivityState = fiber_types_0100::channel::ChannelConnectivityState;
type NewExternalFundingPersistState = fiber_types_0100::channel::ExternalFundingPersistState;
type NewChannelOpeningStatus = fiber_types_0100::channel::ChannelOpeningStatus;
type NewSettlementData = fiber_types_0100::watchtower::SettlementData;
type NewRevocationData = fiber_types_0100::watchtower::RevocationData;
type NewCommitmentContractFeatures = fiber_types_0100::channel::CommitmentContractFeatures;

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
        connectivity_state: decode_as_new::<_, NewChannelConnectivityState>(
            old.connectivity_state,
        )?,
        external_funding: old
            .external_funding
            .map(decode_as_new::<_, NewExternalFundingPersistState>)
            .transpose()?,
        commitment_contract_features: NewCommitmentContractFeatures::LEGACY,
    })
}

fn convert_channel_open_record(old: OldChannelOpenRecord) -> Result<NewChannelOpenRecord, String> {
    Ok(NewChannelOpenRecord {
        channel_id: decode_as_new::<_, NewHash256>(old.channel_id)?,
        pubkey: decode_as_new::<_, NewPubkey>(old.pubkey)?,
        is_acceptor: old.is_acceptor,
        status: decode_as_new::<_, NewChannelOpeningStatus>(old.status)?,
        funding_amount: old.funding_amount,
        failure_detail: old.failure_detail,
        created_at: old.created_at,
        last_updated_at: old.last_updated_at,
        commitment_contract_features: NewCommitmentContractFeatures::LEGACY,
    })
}

fn convert_channel_data(old: OldChannelData) -> Result<NewChannelData, String> {
    Ok(NewChannelData {
        channel_id: decode_as_new::<_, NewHash256>(old.channel_id)?,
        funding_udt_type_script: old.funding_udt_type_script,
        local_settlement_key: decode_as_new::<_, NewPrivkey>(old.local_settlement_key)?,
        remote_settlement_key: decode_as_new::<_, NewPubkey>(old.remote_settlement_key)?,
        local_funding_pubkey: decode_as_new::<_, NewPubkey>(old.local_funding_pubkey)?,
        remote_funding_pubkey: decode_as_new::<_, NewPubkey>(old.remote_funding_pubkey)?,
        remote_settlement_data: decode_as_new::<_, NewSettlementData>(old.remote_settlement_data)?,
        pending_remote_settlement_data: decode_as_new::<_, NewSettlementData>(
            old.pending_remote_settlement_data,
        )?,
        local_settlement_data: decode_as_new::<_, NewSettlementData>(old.local_settlement_data)?,
        revocation_data: old
            .revocation_data
            .map(decode_as_new::<_, NewRevocationData>)
            .transpose()?,
        commitment_contract_features: NewCommitmentContractFeatures::LEGACY,
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
            "Migrating to {}: adding commitment_contract_features ...",
            MIGRATION_DB_VERSION
        );

        migrate_channel_actor_data(store)?;
        migrate_channel_open_record(store)?;
        migrate_watchtower_channel_data(store)?;

        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}

fn migrate_channel_actor_data(store: &dyn MigrationStore) -> Result<(), String> {
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
        "Migration {} ChannelActorData complete: {} migrated, {} skipped ({} total)",
        MIGRATION_DB_VERSION, migrated, skipped, total
    );
    Ok(())
}

fn migrate_channel_open_record(store: &dyn MigrationStore) -> Result<(), String> {
    let entries = store.collect_prefix(CHANNEL_OPEN_RECORD_PREFIX);
    let total = entries.len();
    let mut migrated = 0u64;
    let mut skipped = 0u64;

    for (key, value) in entries {
        if bincode::deserialize::<NewChannelOpenRecord>(&value).is_ok() {
            skipped += 1;
            continue;
        }

        let old: OldChannelOpenRecord = bincode::deserialize(&value)
            .map_err(|e| format!("Failed to deserialize old ChannelOpenRecord: {e}"))?;
        let new = convert_channel_open_record(old)?;
        let new_bytes = bincode::serialize(&new)
            .map_err(|e| format!("Failed to serialize new ChannelOpenRecord: {e}"))?;
        store.put(&key, &new_bytes);
        migrated += 1;
    }

    info!(
        "Migration {} ChannelOpenRecord complete: {} migrated, {} skipped ({} total)",
        MIGRATION_DB_VERSION, migrated, skipped, total
    );
    Ok(())
}

fn migrate_watchtower_channel_data(store: &dyn MigrationStore) -> Result<(), String> {
    let entries = store.collect_prefix(WATCHTOWER_CHANNEL_PREFIX);
    let total = entries.len();
    let mut migrated = 0u64;
    let mut skipped = 0u64;

    for (key, value) in entries {
        if bincode::deserialize::<NewChannelData>(&value).is_ok() {
            skipped += 1;
            continue;
        }

        let old: OldChannelData = bincode::deserialize(&value)
            .map_err(|e| format!("Failed to deserialize old watchtower ChannelData: {e}"))?;
        let new = convert_channel_data(old)?;
        let new_bytes = bincode::serialize(&new)
            .map_err(|e| format!("Failed to serialize new watchtower ChannelData: {e}"))?;
        store.put(&key, &new_bytes);
        migrated += 1;
    }

    info!(
        "Migration {} watchtower ChannelData complete: {} migrated, {} skipped ({} total)",
        MIGRATION_DB_VERSION, migrated, skipped, total
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Migration, MigrationObj, NewChannelActorData, NewChannelData, NewChannelOpenRecord,
        OldChannelActorData, OldChannelData, OldChannelOpenRecord,
    };
    use crate::backend::StorageBackend;
    use fiber_types_090::sample::StoreSample;

    const CHANNEL_ACTOR_STATE_PREFIX: u8 = 0x00;
    const CHANNEL_OPEN_RECORD_PREFIX: u8 = 201;
    const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;

    fn gen_store() -> crate::Store {
        let tmp_dir = tempfile::Builder::new()
            .prefix("test-commitment-contract-features-migration")
            .tempdir()
            .unwrap();
        let path = tmp_dir.as_ref().to_path_buf();
        crate::Store::open_db(&path).unwrap()
    }

    #[test]
    fn migrates_channel_actor_data() {
        let store = gen_store();
        let old = OldChannelActorData::samples(42)
            .into_iter()
            .next()
            .expect("sample channel actor data");
        let key = vec![CHANNEL_ACTOR_STATE_PREFIX, 1, 2, 3];
        let old_bytes = bincode::serialize(&old).expect("serialize old channel actor data");
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new().migrate(&store).expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value");
        let new: NewChannelActorData =
            bincode::deserialize(&new_bytes).expect("deserialize migrated channel actor data");

        assert_eq!(new.commitment_contract_features.bits(), 0);
    }

    #[test]
    fn migrates_channel_open_record() {
        let store = gen_store();
        let old = OldChannelOpenRecord::samples(42)
            .into_iter()
            .next()
            .expect("sample channel open record");
        let key = vec![CHANNEL_OPEN_RECORD_PREFIX, 1, 2, 3];
        let old_bytes = bincode::serialize(&old).expect("serialize old channel open record");
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new().migrate(&store).expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value");
        let new: NewChannelOpenRecord =
            bincode::deserialize(&new_bytes).expect("deserialize migrated channel open record");

        assert_eq!(new.commitment_contract_features.bits(), 0);
    }

    #[test]
    fn migrates_watchtower_channel_data() {
        let store = gen_store();
        let old = OldChannelData::samples(42)
            .into_iter()
            .next()
            .expect("sample watchtower channel data");
        let key = vec![WATCHTOWER_CHANNEL_PREFIX, 1, 2, 3];
        let old_bytes = bincode::serialize(&old).expect("serialize old watchtower channel data");
        StorageBackend::put(&store, &key, &old_bytes);

        MigrationObj::new().migrate(&store).expect("migration should succeed");

        let new_bytes = StorageBackend::get(&store, &key).expect("migrated value");
        let new: NewChannelData =
            bincode::deserialize(&new_bytes).expect("deserialize migrated watchtower channel data");

        assert_eq!(new.commitment_contract_features.bits(), 0);
    }
}
