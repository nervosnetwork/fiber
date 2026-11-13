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

/// Convert a field between the published and current crate's data types.
pub fn decode_as_new<TOld: serde::Serialize, TNew: serde::de::DeserializeOwned>(
    value: TOld,
) -> Result<TNew, String> {
    use bincode::Options;
    let bytes = bincode::serialize(&value)
        .map_err(|error| format!("Failed to serialize legacy field: {error}"))?;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .deserialize(&bytes)
        .map_err(|error| format!("Failed to deserialize migrated field: {error}"))
}

include!(concat!(env!("OUT_DIR"), "/register_migrations.rs"));
