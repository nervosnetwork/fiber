use crate::migration::{MigrateConfirmFn, MigrateError, MigrateProgressFn, Migration, Migrations};
use crate::Store;
use std::sync::Arc;

/// Migration coordinator.
///
/// Usage:
/// 1. Create with `DbMigrate::new()`
/// 2. Optionally register migrations via `add_migration()`
/// 3. Call `auto_migrate(store, confirm_fn, progress_fn)`
pub struct DbMigrate {
    migrations: Migrations,
}

impl DbMigrate {
    pub fn new() -> Self {
        DbMigrate {
            migrations: Migrations::default(),
        }
    }

    pub fn add_migration(&mut self, migration: Arc<dyn Migration>) {
        self.migrations.add_migration(migration);
    }

    /// Run the full migration flow: check version, confirm with user, execute.
    pub fn auto_migrate(
        &self,
        store: &Store,
        confirm_fn: MigrateConfirmFn,
        progress_fn: MigrateProgressFn,
    ) -> Result<(), MigrateError> {
        self.migrations.auto_migrate(store, confirm_fn, progress_fn)
    }

    /// Check database version ordering (for external queries).
    pub fn check(&self, store: &Store) -> std::cmp::Ordering {
        self.migrations.check(store)
    }
}

impl Default for DbMigrate {
    fn default() -> Self {
        Self::new()
    }
}
