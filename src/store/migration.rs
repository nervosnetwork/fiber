use crate::Error;
use console::Term;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use rocksdb::ops::Get;
use rocksdb::ops::Put;
use rocksdb::DB;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::{debug, error, info};

pub const MIGRATION_VERSION_KEY: &[u8] = b"db-version";

fn internal_error(reason: String) -> Error {
    Error::DBInternalError(reason)
}

#[derive(Default)]
pub struct Migrations {
    migrations: BTreeMap<String, Arc<dyn Migration>>,
}

impl Migrations {
    pub fn add_migration(&mut self, migration: Arc<dyn Migration>) {
        self.migrations
            .insert(migration.version().to_string(), migration);
    }

    /// Check if database's version is matched with the executable binary version.
    ///
    /// Returns
    /// - Less: The database version is less than the matched version of the executable binary.
    ///   Requires migration.
    /// - Equal: The database version is matched with the executable binary version.
    /// - Greater: The database version is greater than the matched version of the executable binary.
    ///   Requires upgrade the executable binary.
    pub fn check(&self, db: Arc<DB>) -> Ordering {
        let db_version = match db
            .get(MIGRATION_VERSION_KEY)
            .expect("get the version of database")
        {
            Some(version_bytes) => {
                String::from_utf8(version_bytes.to_vec()).expect("version bytes to utf8")
            }
            None => {
                return Ordering::Less;
            }
        };

        debug!("Current database version [{}]", db_version);
        let migrations = self.migrations.values();
        let latest_version = migrations
            .last()
            .unwrap_or_else(|| panic!("should have at least one version"))
            .version();
        debug!("Latest database version [{}]", latest_version);

        db_version.as_str().cmp(latest_version)
    }

    fn run_migrate(&self, mut db: Arc<DB>, v: &str) -> Result<Arc<DB>, Error> {
        let mpb = Arc::new(MultiProgress::new());
        let migrations: BTreeMap<_, _> = self
            .migrations
            .iter()
            .filter(|(mv, _)| mv.as_str() > v)
            .collect();
        let migrations_count = migrations.len();
        for (idx, (_, m)) in migrations.iter().enumerate() {
            let mpbc = Arc::clone(&mpb);
            let pb = move |count: u64| -> ProgressBar {
                let pb = mpbc.add(ProgressBar::new(count));
                pb.set_draw_target(ProgressDrawTarget::term(Term::stdout(), None));
                pb.set_prefix(format!("[{}/{}]", idx + 1, migrations_count));
                pb
            };
            db = m.migrate(db, Arc::new(pb))?;
            db.put(MIGRATION_VERSION_KEY, m.version())
                .map_err(|err| internal_error(format!("failed to migrate the database: {err}")))?;
        }
        mpb.join_and_clear().expect("MultiProgress join");
        Ok(db)
    }

    fn get_migration_version(&self, db: &Arc<DB>) -> Result<Option<String>, Error> {
        let raw = db.get(MIGRATION_VERSION_KEY).map_err(|err| {
            internal_error(format!("failed to get the version of database: {err}"))
        })?;

        Ok(raw.map(|version_bytes| {
            String::from_utf8(version_bytes.to_vec()).expect("version bytes to utf8")
        }))
    }

    /// Initial db version
    pub fn init_db_version(&self, db: Arc<DB>) -> Result<(), Error> {
        if self.need_init(&db) {
            if let Some(m) = self.migrations.values().last() {
                eprintln!("Init database version {}", m.version());
                db.put(MIGRATION_VERSION_KEY, m.version()).map_err(|err| {
                    internal_error(format!("failed to migrate the database: {err}"))
                })?;
            }
        }
        Ok(())
    }

    pub fn need_init(&self, db: &Arc<DB>) -> bool {
        self.get_migration_version(db)
            .expect("get migration failed")
            .is_none()
    }

    pub fn migrate(&self, db: Arc<DB>) -> Result<Arc<DB>, Error> {
        let db_version = self.get_migration_version(&db)?;
        match db_version {
            Some(ref v) => {
                info!("Current database version {}", v);
                self.check_migration_downgrade(v)?;
                let db = self.run_migrate(db, v.as_str())?;
                Ok(db)
            }
            None => Ok(db),
        }
    }

    fn check_migration_downgrade(&self, cur_version: &str) -> Result<(), Error> {
        if let Some(m) = self.migrations.values().last() {
            if m.version() < cur_version {
                error!(
                    "Database downgrade detected. \
                    The database schema version is newer than `fiber` schema version,\
                    please upgrade `fiber` to the latest version"
                );
                return Err(internal_error(
                    "Database downgrade is not supported".to_string(),
                ));
            }
        }
        Ok(())
    }
}

pub trait Migration: Send + Sync {
    fn migrate(
        &self,
        _db: Arc<DB>,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<Arc<DB>, Error>;

    /// returns migration version, use `date +'%Y%m%d%H%M%S'` timestamp format
    fn version(&self) -> &str;

    /// Will cost a lot of time to perform this migration operation.
    ///
    /// Override this function for `Migrations` which could be executed very fast.
    fn expensive(&self) -> bool {
        true
    }

    /// Check if the background migration can be resumed.
    ///
    /// If a migration can be resumed, it should implement the recovery logic in `migrate` function.
    /// and the `MigirateWorker` will add the migration's handler with `register_thread`, so that then
    /// main thread can wait for the background migration to store the progress and exit.
    ///
    /// Otherwise, the migration will be restarted from the beginning.
    ///
    fn can_resume(&self) -> bool {
        false
    }
}

const INIT_DB_VERSION: &str = "20241116135521";

pub struct DefaultMigration {
    version: String,
}

impl DefaultMigration {
    pub fn new() -> Self {
        Self {
            version: INIT_DB_VERSION.to_string(),
        }
    }
}

impl Migration for DefaultMigration {
    fn migrate(
        &self,
        db: Arc<DB>,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<Arc<DB>, Error> {
        Ok(db)
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn expensive(&self) -> bool {
        false
    }
}
