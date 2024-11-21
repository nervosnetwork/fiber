use crate::store::db_migrate::DbMigrate;
use crate::store::migration::DefaultMigration;
use crate::store::migration::Migration;
use crate::store::migration::Migrations;
use crate::Error;
use indicatif::ProgressBar;
use rocksdb::ops::Open;
use rocksdb::DBCompressionType;
use rocksdb::Options;
use rocksdb::DB;
use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

fn gen_path() -> std::path::PathBuf {
    let tmp_dir = tempfile::Builder::new()
        .prefix("test_fiber_")
        .tempdir()
        .unwrap();
    tmp_dir.as_ref().to_path_buf()
}

fn gen_migrate() -> DbMigrate {
    let path = gen_path();
    let mut options = Options::default();
    options.create_if_missing(true);
    options.set_compression_type(DBCompressionType::Lz4);
    let db = Arc::new(DB::open(&options, path).unwrap());
    DbMigrate::new(db)
}

#[test]
fn test_default_migration() {
    let migrate = gen_migrate();
    assert!(migrate.need_init());
    assert_eq!(migrate.check(), Ordering::Less);
    migrate.init_db_version().unwrap();
    assert!(!migrate.need_init());
    assert_eq!(migrate.check(), Ordering::Equal);
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
        fn migrate(
            &self,
            db: Arc<DB>,
            _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
        ) -> Result<Arc<DB>, Error> {
            eprintln!("DummyMigration::migrate {} ... ", self.version);
            let mut count = self.run_count.write().unwrap();
            *count += 1;
            Ok(db)
        }

        fn version(&self) -> &str {
            &self.version
        }

        fn expensive(&self) -> bool {
            false
        }
    }

    let migrate = gen_migrate();
    migrate.init_db_version().unwrap();
    let db = migrate.db();

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20221116135521",
        run_count.clone(),
    )));

    migrations.add_migration(Arc::new(DummyMigration::new(
        "20251116135521",
        run_count.clone(),
    )));
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20251116135522",
        run_count.clone(),
    )));
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20251116135523",
        run_count.clone(),
    )));

    assert_eq!(migrations.check(db.clone()), Ordering::Less);
    migrations.migrate(db.clone()).unwrap();
    assert_eq!(*run_count.read().unwrap(), 3);
    assert_eq!(migrations.check(db.clone()), Ordering::Equal);

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(DefaultMigration::new()));
    assert_eq!(migrations.check(db), Ordering::Greater);
}
