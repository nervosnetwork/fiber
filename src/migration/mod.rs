pub const MIGRATION_VERSION_KEY: &[u8] = b"db-version";

pub mod db_migrate;
pub(crate) mod migration;
pub(crate) mod migrations;
#[cfg(test)]
mod tests;
