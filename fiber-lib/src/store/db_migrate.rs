use super::migration::{DefaultMigration, Migration, Migrations};
use crate::Error;
use rocksdb::DB;
use std::{cmp::Ordering, path::Path, sync::Arc};
use tracing::warn;
use tracing::{error, info};

/// migrate helper
pub struct DbMigrate {
    migrations: Migrations,
    db: Arc<DB>,
}

impl DbMigrate {
    /// Construct new migrate
    pub fn new(db: Arc<DB>) -> Self {
        let mut migrations = Migrations::default();
        migrations.add_migration(Arc::new(DefaultMigration::new()));
        DbMigrate { migrations, db }
    }

    pub fn add_migration(&mut self, migration: Arc<dyn Migration>) {
        self.migrations.add_migration(migration);
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

    pub fn init_or_check<P: AsRef<Path>>(&self, path: P) -> Result<Arc<DB>, String> {
        if self.need_init() {
            info!("begin to init db version ...");
            self.init_db_version().expect("failed to init db version");
            Ok(self.db())
        } else {
            match self.check() {
                Ordering::Greater => {
                    error!(
                        "The database was created by a higher version fiber executable binary \n\
                     and cannot be opened by the current binary.\n\
                     Please download the latest fiber executable binary."
                    );
                    return Err("incompatible database, need to upgrade fiber binary".to_string());
                }
                Ordering::Equal => {
                    warn!("no need to migrate, everything is OK ...");
                    return Ok(self.db());
                }
                Ordering::Less => {
                    return Err(format!("Fiber need to run some database migrations, please run `fnn-migrate -p {}` to start migrations.", path.as_ref().display()));
                }
            }
        }
    }
}
