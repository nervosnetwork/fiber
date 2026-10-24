use std::sync::{Arc, RwLock};

use clap_serde_derive::ClapSerde;
use fiber_store::backend::StorageBackend;
use fiber_store::db_migrate::DbMigrate;
use fiber_store::migration::{Migration, MigrationStore, INIT_DB_VERSION, MIGRATION_VERSION_KEY};

use crate::store::store_impl::{open_store_with_migration, open_store_with_migrations};
use crate::tests::test_utils::TempDir;
use crate::FiberConfig;

#[test]
fn test_store_sync_writes_is_off_by_default() {
    assert!(!FiberConfig::default().store_sync_writes());
}

#[test]
fn test_store_sync_writes_is_read_from_the_config_file() {
    let on: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str("store_sync_writes: true").expect("parse fiber config");
    assert!(FiberConfig::from(on).store_sync_writes());

    let unset: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str("sync_network_graph: false").expect("parse fiber config");
    assert!(!FiberConfig::from(unset).store_sync_writes());
}

#[test]
fn test_store_sync_writes_reaches_the_backend() {
    for sync in [true, false] {
        let dir = TempDir::new("store-sync-writes");
        let store =
            open_store_with_migration(dir.as_ref(), sync, Box::new(|_| true), Box::new(|_| {}))
                .expect("open store");
        assert_eq!(store.sync_writes(), sync);
        assert_eq!(store.clone().sync_writes(), sync);
    }
}

/// Records the store's sync mode at the moment a migration writes.
struct SyncProbe {
    seen: Arc<RwLock<Option<bool>>>,
}

impl Migration for SyncProbe {
    fn version(&self) -> &str {
        "20260302200001"
    }

    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        store.put(b"sync-probe", b"written");
        *self.seen.write().unwrap() = Some(store.sync_writes());
        Ok(())
    }
}

#[test]
fn test_store_sync_writes_cover_the_migrations() {
    for sync in [true, false] {
        let dir = TempDir::new("store-sync-migration");
        // A database one migration behind, as a node upgraded in place finds it.
        let db = fiber_store::Store::open_db(dir.as_ref()).expect("open db");
        StorageBackend::put(&db, MIGRATION_VERSION_KEY, INIT_DB_VERSION);
        drop(db);

        let seen = Arc::new(RwLock::new(None));
        let mut migrate = DbMigrate::new();
        migrate.add_migration(Arc::new(SyncProbe { seen: seen.clone() }));
        let store = open_store_with_migrations(
            dir.as_ref(),
            sync,
            migrate,
            Box::new(|_| true),
            Box::new(|_| {}),
        )
        .expect("open store");
        assert_eq!(
            *seen.read().unwrap(),
            Some(sync),
            "the migration ran with sync {sync}"
        );
        assert_eq!(store.sync_writes(), sync);
    }
}
