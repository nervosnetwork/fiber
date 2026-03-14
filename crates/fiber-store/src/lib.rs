pub mod backend;
pub mod db_migrate;
mod error;
pub mod iterator;
pub mod migration;

pub use backend::{BatchWriter, StorageBackend};
pub use error::StoreError;
pub use iterator::{IteratorDirection, KVPair};

#[cfg(all(not(target_arch = "wasm32"), not(feature = "sqlite")))]
mod native;
#[cfg(all(not(target_arch = "wasm32"), not(feature = "sqlite")))]
pub use native::{Batch, Store};

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
mod sqlite;
#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
pub use sqlite::{Batch, Store};

#[cfg(all(target_arch = "wasm32", not(feature = "browser-test")))]
mod browser;
#[cfg(all(target_arch = "wasm32", not(feature = "browser-test")))]
pub use browser::{Batch, Store};

#[cfg(all(target_arch = "wasm32", feature = "browser-test"))]
mod browser_test;
#[cfg(all(target_arch = "wasm32", feature = "browser-test"))]
pub use browser_test::{Batch, Store};
