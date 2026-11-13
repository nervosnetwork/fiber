use fiber_store::backend::StorageBackend;
use fiber_store::db_migrate::DbMigrate;
use fiber_store::migrations::decode_as_new;

#[path = "../../../../fiber-store/src/migrations/v2_only/test_utils.rs"]
mod migration_fixtures;
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
    let v1 = "20261114000001";
    let v2 = "20261114000002";
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
        "20261114000001",
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
    migrations.add_migration(Arc::new(BreakChangeMigration::new("20261114000001")));

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
fn test_invalid_previous_epochs_are_rejected_without_changing_records() {
    for version in [b"20260925120000", b"20261113000000"] {
        let path = gen_path();
        {
            let store = fiber_store::Store::open_db(&path).unwrap();
            store.put(MIGRATION_VERSION_KEY, version);
            store.put([0, 1], b"old channel bytes");
        }
        let Err(error) = crate::store::open_store(&path) else {
            panic!("old database must be rejected");
        };
        assert!(
            error.contains("Invalid channel record")
                || error.contains("Unsupported database layout"),
            "{error}"
        );
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(store.get(MIGRATION_VERSION_KEY).unwrap(), version);
        assert_eq!(store.get([0, 1]).unwrap(), b"old channel bytes");
    }
}

#[test]
fn test_nonempty_unversioned_database_is_not_stamped() {
    let store = gen_store();
    store.put([0, 1], b"unversioned channel bytes");
    let result = Migrations::default().auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_err());
    assert!(store.get(MIGRATION_VERSION_KEY).is_none());
    assert_eq!(store.get([0, 1]).unwrap(), b"unversioned channel bytes");
}

#[test]
#[cfg(feature = "watchtower")]
fn test_current_epoch_rejects_other_channel_record_suffixes_without_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::schema::{CHANNEL_OPEN_RECORD_PREFIX, WATCHTOWER_CHANNEL_PREFIX};

    let records = [
        (
            CHANNEL_OPEN_RECORD_PREFIX,
            bincode::serialize(&fiber_types::ChannelOpenRecord::samples(42)[0]).unwrap(),
        ),
        (
            WATCHTOWER_CHANNEL_PREFIX,
            bincode::serialize(&fiber_types::ChannelData::samples(42)[0]).unwrap(),
        ),
    ];
    for (prefix, mut encoded) in records {
        let path = gen_path();
        encoded.push(0);
        let key = [prefix, 1];
        {
            let store = fiber_store::Store::open_db(&path).unwrap();
            store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
            store.put(key, &encoded);
        }
        assert!(crate::store::open_store(&path).is_err());
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(store.get(key).unwrap(), encoded);
        assert_eq!(
            store.get(MIGRATION_VERSION_KEY).unwrap(),
            LATEST_DB_VERSION.as_bytes()
        );
    }
}

#[test]
fn test_v2_current_preflight_pages_closed_history_before_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, ChannelState, CloseFlags};
    let path = gen_path();
    let mut records = std::collections::BTreeMap::new();
    for seed in 0..205 {
        let channel = ChannelActorData::samples(seed).remove(0);
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
    records.last_entry().unwrap().get_mut().state = ChannelState::Closed(CloseFlags::COOPERATIVE);
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
    let mut record = ChannelActorData::samples(1561).remove(0);
    record.channel_features = fiber_types::ChannelFeatures::V2;
    record.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
    let key = [&[0], record.id.as_ref()].concat();
    let mut original = bincode::serialize(&record).unwrap();
    original.truncate(original.len() - bincode::serialize(&record.session).unwrap().len());
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
    let mut record = fiber_types::ChannelActorData::samples(1561).remove(0);
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

#[tokio::test]
async fn test_upgrade_retains_real_v2_signed_history_and_remains_valid() {
    use crate::fiber::channel::ChannelActorStateStore;
    use crate::tests::test_utils::create_nodes_with_established_channel;
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let payment = a.send_payment_keysend(&b, 1000, false).await.unwrap();
    a.wait_until_success(payment.payment_hash).await;
    crate::tests::test_utils::wait_until_timeout(5000, || {
        let channel = a.get_channel_actor_state(id);
        channel.get_offered_tlc_balance().unwrap() == 0 && !channel.tlc_state.waiting_ack
    })
    .await;
    let channel = a.get_channel_actor_state(id);
    assert!(channel.session.own.number > 2);
    assert!(channel.session.outgoing.is_some());
    assert!(channel.session.incoming.is_some());
    let original = bincode::serialize(&channel.core).unwrap();
    let old = migration_fixtures::released_v2(channel.core.clone());
    let key = [&[0], id.as_ref()].concat();
    let path = gen_path();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
        store.put(&key, bincode::serialize(&old).unwrap());
    }
    crate::store::check_validate(&path)
        .expect("read-only validation accepts an upgradeable V2 store");
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(
            store.get(MIGRATION_VERSION_KEY).unwrap(),
            INIT_DB_VERSION.as_bytes()
        );
    }
    let store = crate::store::open_store(&path).expect("migrate existing V2 channel");
    let restored = store.get_channel_actor_state(&id).unwrap();
    assert_eq!(bincode::serialize(&restored.core).unwrap(), original);
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );
    drop(store);
    crate::store::check_validate(&path).expect("upgraded signed history remains valid");
}
