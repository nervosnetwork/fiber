use fiber_store::backend::StorageBackend;
use fiber_store::db_migrate::DbMigrate;
use fiber_store::migration::{
    MigrateError, Migration, Migrations, INIT_DB_VERSION, LATEST_DB_VERSION, MIGRATION_VERSION_KEY,
};
use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

fn gen_path() -> std::path::PathBuf {
    let tmp_dir = tempfile::Builder::new()
        .prefix("test-store")
        .tempdir()
        .unwrap();
    tmp_dir.as_ref().to_path_buf()
}

fn gen_store() -> fiber_store::Store {
    let path = gen_path();
    fiber_store::Store::open_db(&path).unwrap()
}

#[test]
fn test_new_db_gets_latest_version() {
    let store = gen_store();
    let migrations = Migrations::default();

    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_ok());

    let version = store.get(MIGRATION_VERSION_KEY).unwrap();
    let version_str = String::from_utf8(version).unwrap();
    assert_eq!(version_str, LATEST_DB_VERSION);
}

#[test]
fn test_current_db_no_migration_needed() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);

    let migrations = Migrations::default();
    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_ok());
}

#[test]
fn test_old_db_returns_error() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, "20240101000000");

    let migrations = Migrations::default();
    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));

    assert!(matches!(result, Err(MigrateError::DatabaseTooOld { .. })));
}

#[test]
fn test_newer_db_returns_error() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, "99991231235959");

    let migrations = Migrations::default();
    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));

    assert!(matches!(result, Err(MigrateError::DatabaseTooNew { .. })));
}

pub struct DummyMigration {
    version: String,
    run_count: Arc<RwLock<usize>>,
}

impl DummyMigration {
    pub fn new(version: &str, run_count: Arc<RwLock<usize>>) -> Self {
        Self {
            version: version.to_string(),
            run_count,
        }
    }
}

impl Migration for DummyMigration {
    fn migrate(&self, _store: &dyn fiber_store::migration::MigrationStore) -> Result<(), String> {
        eprintln!("DummyMigration::migrate {} ... ", self.version);
        let mut count = self.run_count.write().unwrap();
        *count += 1;
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}

pub struct BreakChangeMigration {
    version: String,
}

impl BreakChangeMigration {
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
        }
    }
}

impl Migration for BreakChangeMigration {
    fn migrate(&self, _store: &dyn fiber_store::migration::MigrationStore) -> Result<(), String> {
        eprintln!("BreakChangeMigration::migrate {} ... ", self.version);
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn is_break_change(&self) -> bool {
        true
    }
}

#[test]
fn test_run_migration() {
    let run_count = Arc::new(RwLock::new(0));
    let store = gen_store();

    // Initialize with INIT_DB_VERSION
    store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

    let mut migrations = Migrations::default();
    // Add migrations after INIT_DB_VERSION
    let v1 = "20260302200001";
    let v2 = "20260302200002";
    migrations.add_migration(Arc::new(DummyMigration::new(v1, run_count.clone())));
    migrations.add_migration(Arc::new(DummyMigration::new(v2, run_count.clone())));

    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_ok());
    assert_eq!(*run_count.read().unwrap(), 2);

    // Verify version was updated to the last migration
    let version = store.get(MIGRATION_VERSION_KEY).unwrap();
    let version_str = String::from_utf8(version).unwrap();
    assert_eq!(version_str, v2);
}

#[test]
fn test_user_cancel_returns_error() {
    let run_count = Arc::new(RwLock::new(0));
    let store = gen_store();

    store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20260302200001",
        run_count.clone(),
    )));

    // User declines
    let result = migrations.auto_migrate(&store, Box::new(|_| false), Box::new(|_| {}));

    assert!(matches!(result, Err(MigrateError::UserCancelled)));
    // Migration should NOT have run
    assert_eq!(*run_count.read().unwrap(), 0);
}

#[test]
fn test_break_change_migration() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(BreakChangeMigration::new("20260302200001")));

    // auto_migrate should present a plan with has_break_change=true
    let result = migrations.auto_migrate(
        &store,
        Box::new(|plan| {
            assert!(plan.has_break_change);
            true // confirm anyway
        }),
        Box::new(|_| {}),
    );
    assert!(result.is_ok());
}

#[test]
fn test_db_migrate_check() {
    let store = gen_store();

    let migrate = DbMigrate::new();
    // No version set yet
    assert_eq!(migrate.check(&store), Ordering::Less);

    store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
    assert_eq!(migrate.check(&store), Ordering::Equal);

    store.put(MIGRATION_VERSION_KEY, "99991231235959");
    assert_eq!(migrate.check(&store), Ordering::Greater);
}

#[test]
fn test_v2_migration_rejects_full_hash_v1_without_partial_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, ChannelState, CloseFlags, CommitmentContractFeatures};

    // Put legacy data first so a migration that validates while writing would
    // have rewritten it before discovering the incompatible development record.
    let path = gen_path();
    let before;
    let affected_ids;
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
        let mut samples = ChannelActorData::samples(1561);
        let mut legacy = samples.remove(0);
        legacy.commitment_contract_features = CommitmentContractFeatures::LEGACY;
        let full_hash = samples.remove(0);
        let mut closed_full_hash = full_hash.clone();
        closed_full_hash.id = [163; 32].into();
        closed_full_hash.state = ChannelState::Closed(CloseFlags::empty());
        affected_ids = [full_hash.id, closed_full_hash.id];
        for (index, record) in [legacy, full_hash, closed_full_hash].iter().enumerate() {
            // Genuine development V1 layout ends at the feature byte, before
            // the newly appended optional V2 session tag.
            let encoded = bincode::serialize(record).unwrap();
            store.put([0, index as u8], &encoded[..encoded.len() - 1]);
        }
        store.put(b"unrelated", b"must survive unchanged");
        before = store.collect_iterator(
            vec![],
            fiber_store::iterator::IteratorDirection::Forward,
            Box::new(|_| true),
            0,
        );
    }

    let result = crate::store::open_store_with_migration(
        &path,
        Box::new(|_| panic!("incompatible development data must be rejected before confirmation")),
        Box::new(|_| panic!("incompatible development data must be rejected before migration")),
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("full-hash V1 development data cannot become V2"),
    };
    for id in affected_ids {
        assert!(
            error.contains(&id.to_string()),
            "missing affected channel {id}: {error}"
        );
    }
    let store = fiber_store::Store::open_db(&path).unwrap();
    let after = store.collect_iterator(
        vec![],
        fiber_store::iterator::IteratorDirection::Forward,
        Box::new(|_| true),
        0,
    );
    assert_eq!(
        before
            .iter()
            .map(|kv| (&kv.key, &kv.value))
            .collect::<Vec<_>>(),
        after
            .iter()
            .map(|kv| (&kv.key, &kv.value))
            .collect::<Vec<_>>(),
        "rejection must preserve every record and the original DB version"
    );
}

#[test]
fn test_v2_current_preflight_pages_closed_history_before_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, ChannelState, CloseFlags, CommitmentContractFeatures};
    let path = gen_path();
    let mut records = std::collections::BTreeMap::new();
    for seed in 0..205 {
        let mut channel = ChannelActorData::samples(seed).remove(0);
        channel.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
        records.insert([&[0], channel.id.as_ref()].concat(), channel);
    }
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
        for (key, channel) in &records {
            store.put(key, bincode::serialize(channel).unwrap());
        }
    }
    drop(crate::store::open_store(&path).expect("valid multi-page history"));
    records
        .last_entry()
        .unwrap()
        .get_mut()
        .commitment_contract_features = CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH;
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        let (key, channel) = records.last_key_value().unwrap();
        store.put(key, bincode::serialize(channel).unwrap());
    }
    assert!(
        crate::store::open_store(&path).is_err(),
        "invalid closed record on page three must be scanned"
    );
    let store = fiber_store::Store::open_db(&path).unwrap();
    for (key, channel) in &records {
        assert_eq!(
            store.get(key).unwrap(),
            bincode::serialize(channel).unwrap()
        );
    }
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );
}

#[test]
fn test_v2_current_epoch_rejects_missing_session_before_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, ChannelState, CloseFlags};
    let path = gen_path();
    let mut record = ChannelActorData::samples(1561).remove(1);
    record.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
    record.session_v2 = None;
    let key = [&[0], record.id.as_ref()].concat();
    let original = bincode::serialize(&record).unwrap();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
        store.put(&key, &original);
    }
    assert!(
        crate::store::open_store(&path).is_err(),
        "epoch cannot validate a full-hash channel without sessions"
    );
    let store = fiber_store::Store::open_db(&path).unwrap();
    assert_eq!(store.get(&key).unwrap(), original);
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );
}

#[test]
fn test_v2_current_epoch_rejects_bootstrap_session_as_closed_channel() {
    use crate::store::sample::StoreSample;
    let path = gen_path();
    let mut record = fiber_types::ChannelActorData::samples(1561).remove(2);
    record.state = fiber_types::ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
    let key = [&[0], record.id.as_ref()].concat();
    let encoded = bincode::serialize(&record).unwrap();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
        store.put(&key, &encoded);
    }
    assert!(
        crate::store::open_store(&path).is_err(),
        "a genuine bootstrap nonce is insufficient evidence for a funded closed channel"
    );
    let store = fiber_store::Store::open_db(&path).unwrap();
    assert_eq!(store.get(&key).unwrap(), encoded);
}

#[test]
fn test_v2_migration_preserves_legacy_zero_channel() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, CommitmentContractFeatures};

    let path = gen_path();
    let mut legacy = ChannelActorData::samples(42).remove(1);
    legacy.commitment_contract_features = CommitmentContractFeatures::LEGACY;
    let key = [&[0x00], legacy.id.as_ref()].concat();
    let original = bincode::serialize(&legacy).unwrap();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, "20260618120000");
        // Released 0.9 records predate the feature byte and optional V2 session tag.
        let released = &original[..original.len() - 2];
        store.put(&key, released);
    }
    {
        let store = crate::store::open_store(&path).expect("legacy migration succeeds");
        let bytes = store.get(&key).expect("legacy channel retained");
        let migrated: ChannelActorData = bincode::deserialize(&bytes)
            .expect("released legacy channel readable after V2 migration");
        assert_eq!(
            migrated.commitment_contract_features,
            CommitmentContractFeatures::LEGACY
        );
        assert_eq!(bincode::serialize(&migrated).unwrap(), original);
        assert_eq!(
            store.get(MIGRATION_VERSION_KEY).unwrap(),
            LATEST_DB_VERSION.as_bytes()
        );
    }
    let store = crate::store::open_store(&path).expect("migrated legacy DB reopens");
    assert_eq!(store.get(&key).unwrap(), original);
}
