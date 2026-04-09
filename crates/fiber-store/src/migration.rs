use crate::backend::StorageBackend;
use crate::StoreError;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::{error, info};

use crate::Store;

pub const MIGRATION_VERSION_KEY: &[u8] = b"db-version";
pub const INIT_DB_VERSION: &str = "20260302100001";
include!(concat!(env!("OUT_DIR"), "/latest_db_version.rs"));

fn internal_error(reason: String) -> StoreError {
    StoreError::DBInternalError(reason)
}

// --- Callback types for platform-specific UI ---

/// Describes a pending migration plan for user confirmation.
pub struct MigrationPlan {
    pub current_version: String,
    pub target_version: String,
    pub pending_count: usize,
    pub has_break_change: bool,
    pub message: String,
}

/// Reports progress of an in-flight migration step.
pub struct MigrationProgress {
    pub current_step: usize,
    pub total_steps: usize,
    pub current_version: String,
    pub message: String,
}

/// Migration-specific errors.
pub enum MigrateError {
    UserCancelled,
    DatabaseTooOld {
        db_version: String,
        min_version: String,
    },
    DatabaseTooNew {
        db_version: String,
        latest_version: String,
    },
    MigrationFailed {
        version: String,
        error: String,
    },
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::UserCancelled => write!(f, "Migration cancelled by user"),
            MigrateError::DatabaseTooOld {
                db_version,
                min_version,
            } => {
                write!(
                    f,
                    "Database version {} is too old. Minimum supported version is {}. \
                     Please use fnn-migrate v0.7.x to upgrade first.",
                    db_version, min_version
                )
            }
            MigrateError::DatabaseTooNew {
                db_version,
                latest_version,
            } => {
                write!(
                    f,
                    "Database version {} is newer than the latest supported version {}. \
                     Please upgrade the fiber binary.",
                    db_version, latest_version
                )
            }
            MigrateError::MigrationFailed { version, error } => {
                write!(f, "Migration {} failed: {}", version, error)
            }
        }
    }
}

impl std::fmt::Debug for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for MigrateError {}

/// Platform-specific confirmation callback.
/// Receives a MigrationPlan, returns true if user confirms.
pub type MigrateConfirmFn = Box<dyn FnOnce(MigrationPlan) -> bool>;

/// Platform-specific progress callback.
/// Called before each migration step executes.
pub type MigrateProgressFn = Box<dyn Fn(MigrationProgress)>;

// --- Migration trait ---

pub trait Migration: Send + Sync {
    /// Timestamp-based version string: "YYYYMMDDHHMMSS"
    fn version(&self) -> &str;

    /// Execute migration using the store backend.
    fn migrate(&self, store: &Store) -> Result<(), String>;

    /// Whether this migration is a breaking change requiring user action.
    fn is_break_change(&self) -> bool {
        false
    }
}

// --- Migrations registry ---

#[derive(Default)]
pub struct Migrations {
    migrations: BTreeMap<String, Arc<dyn Migration>>,
}

impl Migrations {
    pub fn add_migration(&mut self, migration: Arc<dyn Migration>) {
        self.migrations
            .insert(migration.version().to_string(), migration);
    }

    pub fn get_db_version(&self, store: &Store) -> Option<String> {
        store
            .get(MIGRATION_VERSION_KEY)
            .map(|v| String::from_utf8(v).expect("version bytes to utf8"))
    }

    /// Check database version against binary version.
    pub fn check(&self, store: &Store) -> Ordering {
        let db_version = match self.get_db_version(store) {
            Some(v) => v,
            None => return Ordering::Less,
        };
        eprintln!(
            "Current database version: [{}], latest db version: [{}]",
            db_version, LATEST_DB_VERSION
        );
        db_version.as_str().cmp(LATEST_DB_VERSION)
    }

    /// Initialize a new database with LATEST_DB_VERSION.
    pub fn init_db_version(&self, store: &Store) {
        info!("Init database version {}", LATEST_DB_VERSION);
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
    }

    /// Collect pending migrations (version > current).
    fn pending_migrations(&self, current_version: &str) -> Vec<&Arc<dyn Migration>> {
        self.migrations
            .iter()
            .filter(|(v, _)| v.as_str() > current_version)
            .map(|(_, m)| m)
            .collect()
    }

    /// Check if any pending migration is a breaking change.
    fn has_break_change(&self, current_version: &str) -> bool {
        self.pending_migrations(current_version)
            .iter()
            .any(|m| m.is_break_change())
    }

    /// Run the full auto-migration flow.
    pub fn auto_migrate(
        &self,
        store: &Store,
        confirm_fn: MigrateConfirmFn,
        progress_fn: MigrateProgressFn,
    ) -> Result<(), MigrateError> {
        let db_version = match self.get_db_version(store) {
            None => {
                // New database -- stamp with latest version
                self.init_db_version(store);
                return Ok(());
            }
            Some(v) => v,
        };

        // Already up to date
        if db_version.as_str() == LATEST_DB_VERSION {
            info!(
                "Database version {} is current, no migration needed",
                db_version
            );
            return Ok(());
        }

        // Database too new
        if db_version.as_str() > LATEST_DB_VERSION {
            return Err(MigrateError::DatabaseTooNew {
                db_version,
                latest_version: LATEST_DB_VERSION.to_string(),
            });
        }

        // Database too old (before new epoch)
        if db_version.as_str() < INIT_DB_VERSION {
            return Err(MigrateError::DatabaseTooOld {
                db_version,
                min_version: INIT_DB_VERSION.to_string(),
            });
        }

        // Collect pending migrations
        let pending = self.pending_migrations(&db_version);
        if pending.is_empty() {
            // Between INIT and LATEST but no migrations to run
            self.init_db_version(store);
            return Ok(());
        }

        let has_break = self.has_break_change(&db_version);

        // Build plan and request confirmation
        let plan = MigrationPlan {
            current_version: db_version.clone(),
            target_version: LATEST_DB_VERSION.to_string(),
            pending_count: pending.len(),
            has_break_change: has_break,
            message: format!(
                "Database migration required ({} -> {}), {} pending migration(s).{}",
                db_version,
                LATEST_DB_VERSION,
                pending.len(),
                if has_break {
                    " WARNING: Contains breaking changes -- backup strongly recommended."
                } else {
                    " Backup recommended."
                }
            ),
        };

        if !confirm_fn(plan) {
            return Err(MigrateError::UserCancelled);
        }

        // Execute migrations
        let total = pending.len();
        for (idx, m) in pending.iter().enumerate() {
            progress_fn(MigrationProgress {
                current_step: idx + 1,
                total_steps: total,
                current_version: m.version().to_string(),
                message: format!("Migrating v{}", m.version()),
            });

            m.migrate(store)
                .map_err(|e| MigrateError::MigrationFailed {
                    version: m.version().to_string(),
                    error: e,
                })?;

            // Update db-version after each successful migration
            store.put(MIGRATION_VERSION_KEY, m.version());
        }

        info!(
            "Migration complete: {} -> {}",
            db_version, LATEST_DB_VERSION
        );
        Ok(())
    }
}
