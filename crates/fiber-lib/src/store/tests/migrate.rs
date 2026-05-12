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
