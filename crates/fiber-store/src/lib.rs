pub mod db_migrate;
mod error;
pub mod migration;

pub use error::StoreError;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{Batch, DbDirection, IteratorMode, Store};

#[cfg(all(target_arch = "wasm32", not(feature = "browser-test")))]
mod browser;
#[cfg(all(target_arch = "wasm32", not(feature = "browser-test")))]
pub use browser::{Batch, DbDirection, IteratorMode, Store};

#[cfg(all(target_arch = "wasm32", feature = "browser-test"))]
mod browser_test;
#[cfg(all(target_arch = "wasm32", feature = "browser-test"))]
pub use browser_test::{Batch, DbDirection, IteratorMode, Store};
