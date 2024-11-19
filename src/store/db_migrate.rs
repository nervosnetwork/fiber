use super::migration::{DefaultMigration, Migrations};
use crate::Error;
use rocksdb::{ops::Open, DB};
use std::{cmp::Ordering, path::Path, sync::Arc};

/// migrate helper
pub struct DbMigrate {
    migrations: Migrations,
    db: Arc<DB>,
}

impl DbMigrate {
    /// Construct new migrate
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let mut migrations = Migrations::default();
        migrations.add_migration(Arc::new(DefaultMigration::new()));
        // add more migrations here
        let db = Arc::new(DB::open_default(path).expect("Failed to open rocksdb"));

        DbMigrate { migrations, db }
    }

    /// Check if database's version is matched with the executable binary version.
    ///
    /// Returns
    /// - Less: The database version is less than the matched version of the executable binary.
    ///   Requires migration.
    /// - Equal: The database version is matched with the executable binary version.
    /// - Greater: The database version is greater than the matched version of the executable binary.
    ///   Requires upgrade the executable binary.
    pub fn check(&self) -> Ordering {
        self.migrations.check(self.db.clone())
    }

    /// Perform migrate.
    pub fn migrate(&self) -> Result<Arc<DB>, Error> {
        self.migrations.migrate(self.db.clone())
    }

    /// Perform init_db_version.
    pub fn init_db_version(&self) -> Result<(), Error> {
        self.migrations.init_db_version(self.db.clone())
    }

    pub fn db(&self) -> Arc<DB> {
        self.db.clone()
    }

    pub fn need_init(&self) -> bool {
        self.migrations.need_init(&self.db)
    }
}
