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

/// Number of superseded same-number remote settlement snapshots the watchtower retains.
/// The channel re-sign budget follows this so every signature the node produces stays selectable.
pub(crate) const MAX_SUPERSEDED: usize = 32;

#[cfg(test)]
mod tests;
