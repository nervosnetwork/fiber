pub const MIGRATION_VERSION_KEY: &[u8] = b"db-version";

pub mod migrate;
pub mod migration;
pub mod migrations;
#[cfg(test)]
mod tests;
