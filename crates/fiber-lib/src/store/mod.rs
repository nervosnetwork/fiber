pub mod actor;
mod channel_state;
pub mod namespace;
pub mod restore;
#[cfg(any(test, feature = "sample"))]
pub mod sample;
pub mod store_impl;
pub mod store_trait;

pub use namespace::NodeNamespace;
pub use store_impl::Store;
pub use store_impl::{check_validate, open_store, open_store_with_migration};
pub use store_trait::FiberStore;

// Re-export migration types for callers of open_store_with_migration
pub use fiber_store::migration::{MigrationPlan, MigrationProgress};
pub use fiber_store::migrations::V1ChannelArchiveSummary;

#[cfg(test)]
mod tests;

/// Archive selected V1 channels before opening the store with the V2-only migration.
///
/// This opens the raw store, so it can archive a channel that the migration
/// preflight would otherwise reject. It does not close the funding transaction or
/// recover funds. The store must still be at the pre-migration database version.
pub fn archive_v1_channels(
    store_path: &std::path::Path,
    channel_ids: &[fiber_types::Hash256],
    apply: bool,
) -> Result<Vec<V1ChannelArchiveSummary>, String> {
    #[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
    let marker = store_path.join("data.sqlite");
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "sqlite")))]
    let marker = store_path.join("CURRENT");
    #[cfg(not(target_arch = "wasm32"))]
    if !marker.is_file() {
        return Err(format!(
            "{} is not an existing Fiber store (missing {}); refusing to create or modify it",
            store_path.display(),
            marker.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let store = fiber_store::Store::open_db(store_path).map_err(|error| error.to_string())?;
    fiber_store::migrations::archive_v1_channels(&store, channel_ids, apply)
}
