# Unified Migration System — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the standalone `migrate/` crate with a unified migration system inside `fiber-store` that works across all backends (RocksDB, SQLite, IndexedDB) and auto-migrates at startup with user confirmation.

**Architecture:** Migrations move from the standalone `migrate/` workspace into `fiber-store/src/migrations/`. The `Migration` trait is refactored to accept `&impl StorageBackend` instead of `&Store`. A new `auto_migrate()` flow replaces `init_or_check()`, handling version checks, user confirmation via callbacks, and incremental migration execution. The old `migrate/` directory is archived as `migrate_archive/`.

**Tech Stack:** Rust, `fiber-store` crate, `StorageBackend` trait, RocksDB/SQLite/IndexedDB backends, `wasm-bindgen` (WASM), `SharedArrayBuffer + Atomics` (WASM IPC)

---

## Current State Analysis

### Key Files (current)

| File | Role |
|------|------|
| `migrate/` | Standalone `fnn-migrate` binary, own `[workspace]`, depends on pinned old git branches |
| `migrate/build.rs` | Scans `src/migrations/` → generates `add_migrations()` |
| `migrate/src/main.rs` | CLI: open DB, run migrations, uses `ouroboros` for self-referential struct |
| `crates/fiber-store/src/migration.rs` | `Migration` trait (takes `&Store`), `Migrations` registry, `INIT_DB_VERSION`, `LATEST_DB_VERSION` |
| `crates/fiber-store/src/db_migrate.rs` | `DbMigrate` helper: `init_or_check()`, `migrate()`, `check()` |
| `crates/fiber-store/build.rs` | Scans `../../migrate/src/migrations/` → extracts `LATEST_DB_VERSION` |
| `crates/fiber-lib/src/store/store_impl/mod.rs:132` | `open_store()` → calls `check_migrate()` → `DbMigrate::init_or_check()` |
| `crates/fiber-bin/src/main.rs:102` | `open_store(store_path)` — no confirm callback currently |
| `crates/fiber-wasm/src/lib.rs:126` | `open_store(store_path)` — no confirm callback currently |
| `Makefile:28` | `cd migrate && cargo check --locked` |

### Key Constants

- `INIT_DB_VERSION = "20241116135521"` (in `migration.rs:17`)
- `LATEST_DB_VERSION` = auto-generated, currently `"20260302100001"` (from `mig_20260302_channel_open_record.rs`)
- `MIGRATION_VERSION_KEY = b"db-version"` (in `migration.rs:16`)

### Current Migration Trait Signature

```rust
pub trait Migration: Send + Sync {
    fn migrate<'a>(
        &self,
        _db: &'a Store,
        _pb: Arc<dyn Fn(u64) -> ProgressBar + Send + Sync>,
    ) -> Result<&'a Store, StoreError>;
    fn version(&self) -> &str;
    fn is_break_change(&self) -> bool { false }
}
```

The trait takes `&Store` (concrete type) and an `indicatif::ProgressBar` factory. This ties migrations to the concrete store and to the `indicatif` progress UI library.

---

## Phase 1: Archive & Cleanup

### Task 1: Rename `migrate/` to `migrate_archive/`

**Files:**
- Rename: `migrate/` → `migrate_archive/`
- Create: `migrate_archive/README.md`

**Step 1: Rename the directory**

```bash
git mv migrate migrate_archive
```

**Step 2: Create archive README**

Create `migrate_archive/README.md`:

```markdown
# Archived Migrations (v0.7.x and earlier)

This directory contains the **archived** standalone migration tool (`fnn-migrate`)
that was used for database migrations up to v0.7.x.

**This code is NOT compiled by CI and is kept for reference only.**

## Why archived?

The migration system has been unified into `crates/fiber-store/`. All new
migrations are written against the `StorageBackend` trait and work across
all backends (RocksDB, SQLite, IndexedDB).

## Upgrading from old databases

If you have a database older than version `20260302100001`, you must first
upgrade using the v0.7.x `fnn-migrate` binary:

1. Download the v0.7.x release binary from GitHub releases
2. Run: `fnn-migrate -d <data-dir>`
3. Then upgrade to the new fiber version

The new migration system will take over from version `20260302100001` onwards.
```

**Step 3: Commit**

```bash
git add migrate_archive/
git commit -m "chore: archive migrate/ to migrate_archive/ (ref: unified migration system)"
```

---

### Task 2: Update Makefile — Remove old `migrate/` targets

**Files:**
- Modify: `Makefile:28` (the `cd migrate && cargo check --locked` line)

**Step 1: Edit Makefile**

In `Makefile`, remove the line:

```makefile
cd migrate && cargo check --locked
```

from the `check` target (line 28). The `check-migrate` and `update-migrate-check` targets (lines 95-108) remain unchanged — they scan `fiber-lib/src` and `fiber-types/src`, not `migrate/`.

**Step 2: Verify other targets are unaffected**

Run:
```bash
grep -n 'migrate' Makefile
```

Confirm only `check-migrate`, `update-migrate-check`, and `install-migration-check` remain. No references to `cd migrate`.

**Step 3: Commit**

```bash
git add Makefile
git commit -m "chore: remove old migrate/ check from Makefile"
```

---

## Phase 2: Refactor `fiber-store` — Migration Trait & Registry

### Task 3: Refactor `Migration` trait to use `StorageBackend`

**Files:**
- Modify: `crates/fiber-store/src/migration.rs`
- Modify: `crates/fiber-store/Cargo.toml` (remove `indicatif`, `console` deps if no longer needed)

**Step 1: Write the new Migration trait**

Replace the current `Migration` trait and supporting types in `crates/fiber-store/src/migration.rs`:

```rust
use crate::backend::StorageBackend;
use crate::StoreError;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::{error, info};

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
            MigrateError::DatabaseTooOld { db_version, min_version } => {
                write!(
                    f,
                    "Database version {} is too old. Minimum supported version is {}. \
                     Please use fnn-migrate v0.7.x to upgrade first.",
                    db_version, min_version
                )
            }
            MigrateError::DatabaseTooNew { db_version, latest_version } => {
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

    /// Execute migration using only StorageBackend methods.
    fn migrate(&self, store: &dyn StorageBackend<Batch = crate::Batch>) -> Result<(), String>;

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

    pub fn get_db_version(&self, store: &dyn StorageBackend<Batch = crate::Batch>) -> Option<String> {
        store
            .get(MIGRATION_VERSION_KEY)
            .map(|v| String::from_utf8(v).expect("version bytes to utf8"))
    }

    /// Check database version against binary version.
    pub fn check(&self, store: &dyn StorageBackend<Batch = crate::Batch>) -> Ordering {
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
    pub fn init_db_version(&self, store: &dyn StorageBackend<Batch = crate::Batch>) {
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
        store: &dyn StorageBackend<Batch = crate::Batch>,
        confirm_fn: MigrateConfirmFn,
        progress_fn: MigrateProgressFn,
    ) -> Result<(), MigrateError> {
        let db_version = match self.get_db_version(store) {
            None => {
                // New database — stamp with latest version
                self.init_db_version(store);
                return Ok(());
            }
            Some(v) => v,
        };

        // Already up to date
        if db_version.as_str() == LATEST_DB_VERSION {
            info!("Database version {} is current, no migration needed", db_version);
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
            // (shouldn't happen if LATEST is derived correctly)
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
                    " WARNING: Contains breaking changes — backup strongly recommended."
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

            m.migrate(store).map_err(|e| MigrateError::MigrationFailed {
                version: m.version().to_string(),
                error: e,
            })?;

            // Update db-version after each successful migration
            store.put(MIGRATION_VERSION_KEY, m.version());
        }

        info!("Migration complete: {} -> {}", db_version, LATEST_DB_VERSION);
        Ok(())
    }
}
```

**Key changes from current code:**
1. `INIT_DB_VERSION` updated from `"20241116135521"` to `"20260302100001"` (new epoch)
2. `Migration::migrate()` takes `&dyn StorageBackend<Batch = crate::Batch>` instead of `&Store`
3. Removed `indicatif::ProgressBar` from trait — progress is now via callbacks
4. Removed `DefaultMigration` — no longer needed (new DBs get `LATEST_DB_VERSION` directly)
5. Added `MigrationPlan`, `MigrationProgress`, `MigrateError`, callback types
6. `auto_migrate()` replaces both `init_or_check()` and `migrate()` — single entry point

**Step 2: Update `Cargo.toml` to remove unnecessary deps**

In `crates/fiber-store/Cargo.toml`, remove:
- `indicatif = "0.18"` (no longer used in fiber-store)
- Remove `console = "0.16.0"` from `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`

**Step 3: Run `cargo check -p fiber-store`**

Expected: compile errors in `db_migrate.rs` (uses old API). That's fine — we fix it in Task 4.

**Step 4: Commit**

```bash
git add crates/fiber-store/
git commit -m "refactor: migration trait to use StorageBackend with callback-based progress"
```

---

### Task 4: Refactor `DbMigrate` coordinator

**Files:**
- Modify: `crates/fiber-store/src/db_migrate.rs`

**Step 1: Rewrite `db_migrate.rs`**

Replace with a simpler coordinator that delegates to `Migrations::auto_migrate()`:

```rust
use crate::migration::{MigrateConfirmFn, MigrateError, MigrateProgressFn, Migrations, Migration};
use crate::StorageBackend;
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
        store: &dyn StorageBackend<Batch = crate::Batch>,
        confirm_fn: MigrateConfirmFn,
        progress_fn: MigrateProgressFn,
    ) -> Result<(), MigrateError> {
        self.migrations.auto_migrate(store, confirm_fn, progress_fn)
    }

    /// Check database version ordering (for external queries).
    pub fn check(&self, store: &dyn StorageBackend<Batch = crate::Batch>) -> std::cmp::Ordering {
        self.migrations.check(store)
    }
}

impl Default for DbMigrate {
    fn default() -> Self {
        Self::new()
    }
}
```

**Key changes:**
1. `DbMigrate` no longer holds a `&Store` reference — takes store as parameter
2. Removed `init_or_check()` — replaced by `auto_migrate()`
3. Removed lifetime parameter `'a` — simpler ownership model
4. No longer wraps `DefaultMigration` — the Migrations registry handles empty case

**Step 2: Commit**

```bash
git add crates/fiber-store/src/db_migrate.rs
git commit -m "refactor: simplify DbMigrate to delegate to Migrations::auto_migrate()"
```

---

### Task 5: Create `migrations/` directory in fiber-store

**Files:**
- Create: `crates/fiber-store/src/migrations/mod.rs`

**Step 1: Create the directory and mod.rs**

```bash
mkdir -p crates/fiber-store/src/migrations
```

Create `crates/fiber-store/src/migrations/mod.rs`:

```rust
//! Migration implementations.
//!
//! Each migration is a module containing a struct implementing `Migration`.
//! New migrations are auto-registered at build time via `build.rs`.
//!
//! To add a new migration:
//! 1. Create `mig_YYYYMMDD_description.rs` in this directory
//! 2. Define `const MIGRATION_DB_VERSION: &str = "YYYYMMDDHHMMSS";`
//! 3. Implement `pub struct MigrationObj` with `Migration` trait
//! 4. Run `cargo build` — build.rs will auto-register it

use crate::db_migrate::DbMigrate;
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/register_migrations.rs"));
```

**Step 2: Update `lib.rs` to include the module**

Add to `crates/fiber-store/src/lib.rs`:

```rust
#[cfg(any(feature = "rocksdb", feature = "sqlite", target_arch = "wasm32"))]
pub mod migrations;
```

**Step 3: Commit**

```bash
git add crates/fiber-store/src/migrations/
git add crates/fiber-store/src/lib.rs
git commit -m "feat: add empty migrations directory with auto-registration support"
```

---

### Task 6: Update `fiber-store/build.rs`

**Files:**
- Modify: `crates/fiber-store/build.rs`

**Step 1: Rewrite build.rs**

The build script now does two things:
1. Scans `src/migrations/` (instead of `../../migrate/src/migrations/`) for `LATEST_DB_VERSION`
2. Generates `register_migrations.rs` with auto-registration function

```rust
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=src/migrations");

    let out_dir = env::var("OUT_DIR").unwrap();
    let migrations_dir = Path::new("src/migrations");

    let mut latest_db_version = String::new();
    let mut migration_modules = Vec::new();

    if migrations_dir.exists() {
        for entry in fs::read_dir(migrations_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() {
                if let Some(stem) = path.file_stem() {
                    if let Some(stem_str) = stem.to_str() {
                        if stem_str == "mod" {
                            continue;
                        }

                        let source_code = fs::read_to_string(&path).unwrap();
                        if let Some(version_line) = source_code
                            .lines()
                            .find(|line| line.starts_with("const MIGRATION_DB_VERSION"))
                        {
                            let version = version_line
                                .split_whitespace()
                                .last()
                                .unwrap()
                                .replace("\";", "")
                                .replace('"', "");
                            if version > latest_db_version {
                                latest_db_version = version;
                            }
                            migration_modules.push(stem_str.to_string());
                        }
                    }
                }
            }
        }
    }

    // Generate LATEST_DB_VERSION constant
    // If no migrations exist, use INIT_DB_VERSION as the latest
    let version_code = if latest_db_version.is_empty() {
        "    pub const LATEST_DB_VERSION: &str = INIT_DB_VERSION;\n".to_string()
    } else {
        format!(
            "    pub const LATEST_DB_VERSION: &str = \"{}\";\n",
            latest_db_version
        )
    };
    let version_path = Path::new(&out_dir).join("latest_db_version.rs");
    fs::write(version_path, version_code).unwrap();

    // Generate register_migrations function
    let mut reg_code = String::new();
    reg_code.push_str("pub fn register_all_migrations(db_migrate: &mut DbMigrate) {\n");
    for module in &migration_modules {
        reg_code.push_str(&format!(
            "    db_migrate.add_migration(Arc::new({}::MigrationObj::new()));\n",
            module
        ));
    }
    reg_code.push_str("}\n");

    // Also declare the modules
    let mut mod_decls = String::new();
    for module in &migration_modules {
        mod_decls.push_str(&format!("mod {};\n", module));
    }

    let reg_path = Path::new(&out_dir).join("register_migrations.rs");
    fs::write(reg_path, format!("{}\n{}", mod_decls, reg_code)).unwrap();
}
```

**Step 2: Run `cargo check -p fiber-store`**

Expected: Should compile. With no migration files yet, `LATEST_DB_VERSION` falls back to `INIT_DB_VERSION`.

**Step 3: Commit**

```bash
git add crates/fiber-store/build.rs
git commit -m "refactor: update build.rs to scan src/migrations/ and generate registration code"
```

---

## Phase 3: Integration — `fiber-lib`, `fiber-bin`, `fiber-wasm`

### Task 7: Refactor `open_store()` in `fiber-lib`

**Files:**
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs` (lines 131-145)

**Step 1: Update `open_store` signature and implementation**

Change `open_store()` to accept confirm and progress callbacks:

```rust
use fiber_store::migration::{MigrateConfirmFn, MigrateError, MigrateProgressFn};
use fiber_store::db_migrate::DbMigrate;

/// Open a store at `path`, running auto-migration if needed.
pub fn open_store<P: AsRef<Path>>(
    path: P,
    confirm_fn: MigrateConfirmFn,
    progress_fn: MigrateProgressFn,
) -> Result<Store, String> {
    let db = fiber_store::Store::open_db(path.as_ref())?;
    run_auto_migrate(&db, confirm_fn, progress_fn)?;
    Ok(Store {
        inner: db,
        watcher: None,
    })
}

fn run_auto_migrate(
    db: &fiber_store::Store,
    confirm_fn: MigrateConfirmFn,
    progress_fn: MigrateProgressFn,
) -> Result<(), String> {
    let mut migrate = DbMigrate::new();
    fiber_store::migrations::register_all_migrations(&mut migrate);
    migrate
        .auto_migrate(db, confirm_fn, progress_fn)
        .map_err(|e| e.to_string())
}
```

Also update `check_validate()` if it still uses `DbMigrate::init_or_check()` — it may just need a simple version check rather than full migration.

**Step 2: Verify that all callers of `open_store` are updated**

Search for `open_store(` across the codebase. Expected callers:
- `crates/fiber-bin/src/main.rs` — updated in Task 8
- `crates/fiber-wasm/src/lib.rs` — updated in Task 9
- `tests/` — updated in Task 10

**Step 3: Commit**

```bash
git add crates/fiber-lib/src/store/store_impl/mod.rs
git commit -m "refactor: open_store() accepts confirm/progress callbacks for auto-migration"
```

---

### Task 8: Implement CLI confirm callback in `fiber-bin`

**Files:**
- Modify: `crates/fiber-bin/src/main.rs`

**Step 1: Create confirm and progress callbacks**

Add these callback implementations and update the `open_store` call:

```rust
use fnn::store::open_store;
use fiber_store::migration::{MigrationPlan, MigrationProgress};
use std::io::{self, Write};

fn cli_confirm(plan: MigrationPlan) -> bool {
    eprintln!("{}", plan.message);
    if plan.has_break_change {
        eprintln!(
            "WARNING: This migration contains breaking changes. \
             You should shutdown all channels and backup your data."
        );
    }
    eprint!("Continue? [y/N] ");
    io::stderr().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    input.trim().eq_ignore_ascii_case("y") || input.trim().eq_ignore_ascii_case("yes")
}

fn cli_progress(progress: MigrationProgress) {
    eprintln!(
        "[{}/{}] {}",
        progress.current_step, progress.total_steps, progress.message
    );
}
```

Then change line 102 from:
```rust
let raw_store = open_store(store_path).map_err(|err| ExitMessage(err.to_string()))?;
```
to:
```rust
let raw_store = open_store(
    store_path,
    Box::new(cli_confirm),
    Box::new(cli_progress),
).map_err(|err| ExitMessage(err.to_string()))?;
```

**Step 2: Run `cargo check -p fiber-bin`**

Expected: compiles cleanly.

**Step 3: Commit**

```bash
git add crates/fiber-bin/src/main.rs
git commit -m "feat: CLI confirm/progress callbacks for database migration"
```

---

### Task 9: Implement WASM confirm callback in `fiber-wasm`

**Files:**
- Modify: `crates/fiber-wasm/src/lib.rs`

**Step 1: Create WASM-compatible callbacks**

For the WASM target, we have two options:
1. **Auto-confirm** — always proceed (simplest, reasonable for browser where user already opened the app)
2. **SharedArrayBuffer IPC** — send confirmation request to main thread (as described in design doc)

Start with option 1 (auto-confirm) since it's the simplest and most practical:

```rust
use fiber_store::migration::{MigrationPlan, MigrationProgress};

fn wasm_confirm(plan: MigrationPlan) -> bool {
    tracing::info!("{}", plan.message);
    // In WASM/browser context, auto-confirm migrations.
    // The browser user has already chosen to open the app.
    true
}

fn wasm_progress(progress: MigrationProgress) {
    tracing::info!(
        "[{}/{}] {}",
        progress.current_step, progress.total_steps, progress.message
    );
}
```

Update line 126 from:
```rust
let store = open_store(store_path).map_err(|err| exit_to_js(ExitMessage(err.to_string())))?;
```
to:
```rust
let store = open_store(
    store_path,
    Box::new(wasm_confirm),
    Box::new(wasm_progress),
).map_err(|err| exit_to_js(ExitMessage(err.to_string())))?;
```

**Step 2: Run `cargo check -p fiber-wasm --target wasm32-unknown-unknown`**

Expected: compiles cleanly.

**Step 3: Commit**

```bash
git add crates/fiber-wasm/src/lib.rs
git commit -m "feat: WASM auto-confirm callback for database migration"
```

---

### Task 10: Update test infrastructure

**Files:**
- Modify: any test files that call `open_store()` directly

**Step 1: Search for test usages**

```bash
grep -rn 'open_store(' crates/ tests/ --include='*.rs'
```

**Step 2: Update each call site**

For tests, use auto-confirm callbacks:

```rust
let store = open_store(
    path,
    Box::new(|_| true),  // auto-confirm in tests
    Box::new(|_| {}),    // no-op progress in tests
);
```

If there are many call sites, consider adding a convenience function in test helpers:

```rust
#[cfg(test)]
pub fn open_store_for_test<P: AsRef<Path>>(path: P) -> Result<Store, String> {
    open_store(path, Box::new(|_| true), Box::new(|_| {}))
}
```

**Step 3: Run tests**

```bash
cargo nextest run --no-fail-fast -p fnn -p fiber-bin
```

**Step 4: Commit**

```bash
git add -A
git commit -m "test: update open_store() calls with auto-confirm callbacks"
```

---

## Phase 4: Verification & CI

### Task 11: Verify builds for all targets

**Step 1: Check native build (default features)**

```bash
cargo check --locked
```

**Step 2: Check native with sqlite feature**

```bash
cargo check --no-default-features --features sqlite -p fnn -p fiber-bin -p fnn-cli -p fiber-store -p fiber-types -p fiber-json-types
```

**Step 3: Check WASM build**

```bash
cargo check --target wasm32-unknown-unknown -p fiber-wasm -p fiber-wasm-db-worker -p fiber-wasm-db-common
```

**Step 4: Run clippy**

```bash
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -p fnn-cli -p fiber-store -p fiber-types -p fiber-json-types -- -D warnings
cargo clippy -p fiber-wasm -p fiber-wasm-db-worker -p fiber-wasm-db-common --target wasm32-unknown-unknown -- -D warnings
```

**Step 5: Run fmt**

```bash
cargo fmt --all
```

**Step 6: Run migration schema check**

```bash
make check-migrate
```

**Step 7: Run tests**

```bash
cargo nextest run --no-fail-fast -p fnn -p fiber-bin
```

---

### Task 12: Verify new DB creation stamps correct version

**Step 1: Write a unit test in `fiber-store`**

Add to `crates/fiber-store/src/migration.rs` (or a new test file):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageBackend;

    // Uses the in-memory/test backend
    #[test]
    fn test_new_db_gets_latest_version() {
        let store = /* create temp store */;
        let migrations = Migrations::default();

        let result = migrations.auto_migrate(
            &store,
            Box::new(|_| true),
            Box::new(|_| {}),
        );
        assert!(result.is_ok());

        let version = store.get(MIGRATION_VERSION_KEY).unwrap();
        let version_str = String::from_utf8(version).unwrap();
        assert_eq!(version_str, LATEST_DB_VERSION);
    }

    #[test]
    fn test_old_db_returns_error() {
        let store = /* create temp store */;
        store.put(MIGRATION_VERSION_KEY, "20240101000000");

        let migrations = Migrations::default();
        let result = migrations.auto_migrate(
            &store,
            Box::new(|_| true),
            Box::new(|_| {}),
        );

        assert!(matches!(result, Err(MigrateError::DatabaseTooOld { .. })));
    }

    #[test]
    fn test_newer_db_returns_error() {
        let store = /* create temp store */;
        store.put(MIGRATION_VERSION_KEY, "99991231235959");

        let migrations = Migrations::default();
        let result = migrations.auto_migrate(
            &store,
            Box::new(|_| true),
            Box::new(|_| {}),
        );

        assert!(matches!(result, Err(MigrateError::DatabaseTooNew { .. })));
    }

    #[test]
    fn test_user_cancel_returns_error() {
        let store = /* create temp store */;
        // Set to a version that needs migration (between INIT and LATEST)
        // This requires at least one migration to exist
        // Skip if no migrations registered
        store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

        let mut migrations = Migrations::default();
        // Would need a test migration here...

        let result = migrations.auto_migrate(
            &store,
            Box::new(|_| false), // user declines
            Box::new(|_| {}),
        );

        // If no pending migrations, this would succeed (no-op)
        // If pending, this would return UserCancelled
    }
}
```

Note: The exact test implementation depends on whether a concrete `Store` can be created in tests. RocksDB requires a temp directory, SQLite requires a temp file, etc. Use `#[cfg(feature = "rocksdb")]` or `#[cfg(feature = "sqlite")]` to gate tests appropriately.

---

## Summary of Changes

| Phase | Task | Files Changed | Description |
|-------|------|--------------|-------------|
| 1 | 1 | `migrate/` → `migrate_archive/` | Archive old migrations |
| 1 | 2 | `Makefile` | Remove `cd migrate && cargo check` |
| 2 | 3 | `fiber-store/src/migration.rs`, `Cargo.toml` | New Migration trait + callbacks |
| 2 | 4 | `fiber-store/src/db_migrate.rs` | Simplified DbMigrate coordinator |
| 2 | 5 | `fiber-store/src/migrations/mod.rs`, `lib.rs` | New migrations directory |
| 2 | 6 | `fiber-store/build.rs` | Scan src/migrations/ + generate registration |
| 3 | 7 | `fiber-lib/src/store/store_impl/mod.rs` | `open_store()` with callbacks |
| 3 | 8 | `fiber-bin/src/main.rs` | CLI confirm/progress |
| 3 | 9 | `fiber-wasm/src/lib.rs` | WASM auto-confirm |
| 3 | 10 | test files | Update test call sites |
| 4 | 11 | — | Build/CI verification |
| 4 | 12 | test files | Unit tests for migration logic |

## Design Decisions & Trade-offs

### 1. `&dyn StorageBackend<Batch = crate::Batch>` vs `&impl StorageBackend`

The design doc specifies `&impl StorageBackend`, but that makes the trait non-object-safe (can't use `dyn Migration` in a `BTreeMap`). We use `&dyn StorageBackend<Batch = crate::Batch>` instead, which is object-safe because `Batch` is a concrete associated type per platform. This works because `crate::Batch` is always the right type for the active backend (RocksDB `Batch`, SQLite `Batch`, or Browser `Batch`).

### 2. Auto-confirm in WASM

The design doc describes a `SharedArrayBuffer + Atomics` IPC pattern for WASM confirmation. We start with auto-confirm (always proceed) because:
- Browser users have already chosen to open the app
- The IPC pattern adds complexity and can be added later
- Progress is already logged via `tracing::info!`

### 3. No `fnn migrate` subcommand (deferred)

The design doc mentions an optional `fnn migrate` subcommand for explicit migration. This is deferred to a follow-up task since auto-migration at startup covers the primary use case.

### 4. `indicatif` removal

The `indicatif` progress bar library is removed from `fiber-store` since progress is now reported via platform-specific callbacks. If desired, `fiber-bin` could add `indicatif` as a direct dependency for fancy CLI progress bars in its callback implementation, but simple `eprintln!` is sufficient for now.
