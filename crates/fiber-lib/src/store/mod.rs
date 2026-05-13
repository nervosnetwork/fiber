#[cfg(any(test, feature = "sample"))]
pub mod sample;
pub mod store_impl;
pub mod store_trait;

pub use store_impl::Store;
pub use store_impl::{check_validate, open_store, open_store_with_migration};
pub use store_trait::FiberStore;

// Re-export migration types for callers of open_store_with_migration
pub use fiber_store::migration::{MigrationPlan, MigrationProgress};

#[cfg(test)]
mod tests;
