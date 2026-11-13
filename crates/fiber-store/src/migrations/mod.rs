//! Migration implementations.
//!
//! Each migration is a module containing a struct implementing `Migration`.
//! New migrations are auto-registered at build time via `build.rs`.
//!
//! To add a new migration:
//! 1. Create `mig_YYYYMMDD_description.rs` in this directory
//! 2. Define `const MIGRATION_DB_VERSION: &str = "YYYYMMDDHHMMSS";`
//! 3. Implement `pub struct MigrationObj` with `Migration` trait
//! 4. Run `cargo build` -- build.rs will auto-register it

#[allow(unused_imports)]
use crate::db_migrate::DbMigrate;
#[allow(unused_imports)]
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/register_migrations.rs"));
