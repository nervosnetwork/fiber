use crate::store::db_migrate::DbMigrate;
use crate::store::migration::DefaultMigration;
use crate::store::migration::Migration;
use crate::store::migration::Migrations;
use crate::store::migration::LATEST_DB_VERSION;
use crate::store::migration::MIGRATION_VERSION_KEY;
use crate::store::Store;
use crate::Error;
use indicatif::ProgressBar;
use ouroboros::self_referencing;
use std::cmp::Ordering;
use std::path::Path;
use std::sync::{Arc, RwLock};
fn gen_path() -> std::path::PathBuf {
    let tmp_dir = tempfile::Builder::new()
        .prefix("test-store")
        .tempdir()
        .unwrap();
    tmp_dir.as_ref().to_path_buf()
}
#[self_referencing]
struct StoreAndMigrate {
    store: Store,
    #[borrows(store)]
    #[covariant]
    migrate: DbMigrate<'this>,
}

impl StoreAndMigrate {
    fn new_with_path(path: impl AsRef<Path>) -> Self {
        StoreAndMigrateBuilder {
            store: Store::open_db(path.as_ref()).unwrap(),
            migrate_builder: |store: &Store| DbMigrate::new(store),
        }
        .build()
    }
}

fn gen_migrate() -> StoreAndMigrate {
    let path = gen_path();
    StoreAndMigrate::new_with_path(path)
}

#[test]
fn test_default_migration() {
    let migrate = gen_migrate();
    assert!(migrate.borrow_migrate().need_init());
    assert_eq!(migrate.borrow_migrate().check(), Ordering::Less);
    migrate.borrow_migrate().init_db_version().unwrap();
    assert!(!migrate.borrow_migrate().need_init());
    assert_eq!(migrate.borrow_migrate().check(), Ordering::Equal);
}

#[test]
fn test_run_migration() {
    let run_count = Arc::new(RwLock::new(0));

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
        fn migrate<'a>(
            &self,
            db: &'a Store,
            _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
        ) -> Result<&'a Store, Error> {
            eprintln!("DummyMigration::migrate {} ... ", self.version);
            let mut count = self.run_count.write().unwrap();
            *count += 1;
            Ok(db)
        }

        fn version(&self) -> &str {
            &self.version
        }
    }

    let migrate = gen_migrate();
    migrate.borrow_migrate().init_db_version().unwrap();
    let db = migrate.borrow_store();

    let mut migrations = Migrations::default();
    // a smaller version
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20221116135521",
        run_count.clone(),
    )));

    migrations.add_migration(Arc::new(DummyMigration::new(
        "20241216135521",
        run_count.clone(),
    )));
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20241216135522",
        run_count.clone(),
    )));
    migrations.add_migration(Arc::new(DummyMigration::new(
        LATEST_DB_VERSION,
        run_count.clone(),
    )));
    assert_eq!(migrations.check(db), Ordering::Equal);

    // now manually set db version to a lower one
    db.put(MIGRATION_VERSION_KEY, "20221116135521");
    assert_eq!(migrations.check(db), Ordering::Less);

    migrations.migrate(db).unwrap();
    assert_eq!(*run_count.read().unwrap(), 3);
    assert_eq!(migrations.check(db), Ordering::Equal);

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(DefaultMigration::new()));
    // checked by the LATEST_DB_VERSION
    assert_eq!(migrations.check(db), Ordering::Equal);
}
