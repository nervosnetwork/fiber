use tracing::info;

use crate::migration::{Migration, MigrationStore};

use super::decode_as_new;

const MIGRATION_DB_VERSION: &str = "20260812130000";
const WATCHTOWER_CHANNEL_PREFIX: &[u8] = &[224];

pub use fiber_types_090::ChannelData as OldChannelData;
pub use fiber_types_current::ChannelData as NewChannelData;

fn convert_settlement_tlc(
    old: fiber_types_090::SettlementTlc,
) -> Result<fiber_types_current::SettlementTlc, String> {
    let local_key: fiber_types_current::Privkey = decode_as_new(old.local_key)?;
    Ok(fiber_types_current::SettlementTlc {
        tlc_id: decode_as_new(old.tlc_id)?,
        hash_algorithm: decode_as_new(old.hash_algorithm)?,
        payment_amount: old.payment_amount,
        payment_hash: decode_as_new(old.payment_hash)?,
        expiry: old.expiry,
        local_key_pubkey: Some(local_key.pubkey()),
        local_key: Some(local_key),
        local_key_commitment_number: None,
        remote_key: decode_as_new(old.remote_key)?,
    })
}

fn convert_settlement_data(
    old: fiber_types_090::SettlementData,
) -> Result<fiber_types_current::SettlementData, String> {
    Ok(fiber_types_current::SettlementData {
        local_amount: old.local_amount,
        remote_amount: old.remote_amount,
        tlcs: old
            .tlcs
            .into_iter()
            .map(convert_settlement_tlc)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn convert_channel_data(old: OldChannelData) -> Result<NewChannelData, String> {
    let local_settlement_key: fiber_types_current::Privkey =
        decode_as_new(old.local_settlement_key)?;
    Ok(NewChannelData {
        channel_id: decode_as_new(old.channel_id)?,
        funding_udt_type_script: old.funding_udt_type_script,
        local_settlement_key_pubkey: Some(local_settlement_key.pubkey()),
        local_settlement_key: Some(local_settlement_key),
        remote_settlement_key: decode_as_new(old.remote_settlement_key)?,
        local_funding_pubkey: decode_as_new(old.local_funding_pubkey)?,
        remote_funding_pubkey: decode_as_new(old.remote_funding_pubkey)?,
        remote_settlement_data: convert_settlement_data(old.remote_settlement_data)?,
        pending_remote_settlement_data: convert_settlement_data(
            old.pending_remote_settlement_data,
        )?,
        local_settlement_data: convert_settlement_data(old.local_settlement_data)?,
        revocation_data: old.revocation_data.map(decode_as_new).transpose()?,
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
            "Migrating to {}: adding public external-signer material to watch channels ...",
            MIGRATION_DB_VERSION
        );

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
                .map_err(|error| format!("Failed to deserialize old ChannelData: {error}"))?;
            let new = convert_channel_data(old)?;
            let new_bytes = bincode::serialize(&new)
                .map_err(|error| format!("Failed to serialize new ChannelData: {error}"))?;
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

    use crate::backend::StorageBackend;

    use super::{Migration, MigrationObj, NewChannelData, OldChannelData};

    const WATCHTOWER_CHANNEL_PREFIX: u8 = 224;

    fn gen_store() -> crate::Store {
        let tmp_dir = tempfile::Builder::new()
            .prefix("test-watchtower-signer-material-migration")
            .tempdir()
            .unwrap();
        crate::Store::open_db(tmp_dir.path()).unwrap()
    }

    #[test]
    fn migrates_existing_watch_channels_with_derived_public_keys() {
        let store = gen_store();
        let old_samples = OldChannelData::samples(42);
        let expected = old_samples
            .iter()
            .map(|old| {
                (
                    old.channel_id,
                    old.local_settlement_key.pubkey().serialize(),
                )
            })
            .collect::<Vec<_>>();
        let keys = old_samples
            .into_iter()
            .enumerate()
            .map(|(index, old)| {
                let key = vec![WATCHTOWER_CHANNEL_PREFIX, index as u8 + 1];
                StorageBackend::put(&store, &key, bincode::serialize(&old).unwrap());
                key
            })
            .collect::<Vec<_>>();

        let migration = MigrationObj::new();
        migration.migrate(&store).expect("migration should succeed");

        let migrated_bytes = keys
            .iter()
            .zip(expected)
            .map(|(key, (expected_id, expected_settlement_pubkey))| {
                let bytes = StorageBackend::get(&store, key).expect("migrated value");
                let migrated: NewChannelData =
                    bincode::deserialize(&bytes).expect("deserialize migrated ChannelData");
                assert_eq!(migrated.channel_id.as_ref(), expected_id.as_ref());
                assert_eq!(
                    migrated.local_settlement_pubkey().serialize(),
                    expected_settlement_pubkey
                );
                assert!(migrated.local_settlement_key.is_some());
                assert_eq!(
                    migrated.local_settlement_key_pubkey,
                    migrated
                        .local_settlement_key
                        .as_ref()
                        .map(fiber_types_current::Privkey::pubkey)
                );
                for settlement in [
                    &migrated.remote_settlement_data,
                    &migrated.pending_remote_settlement_data,
                    &migrated.local_settlement_data,
                ] {
                    for tlc in &settlement.tlcs {
                        assert!(tlc.local_key.is_some());
                        assert_eq!(
                            tlc.local_key_pubkey,
                            tlc.local_key
                                .as_ref()
                                .map(fiber_types_current::Privkey::pubkey)
                        );
                        assert!(tlc.local_key_commitment_number.is_none());
                    }
                }
                bytes
            })
            .collect::<Vec<_>>();

        migration
            .migrate(&store)
            .expect("running migration twice should safely skip new data");
        for (key, expected_bytes) in keys.iter().zip(migrated_bytes) {
            assert_eq!(StorageBackend::get(&store, key).unwrap(), expected_bytes);
        }
    }
}
