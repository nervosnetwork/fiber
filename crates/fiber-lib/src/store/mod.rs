#[cfg(not(target_arch = "wasm32"))]
pub mod audit;
#[cfg(not(target_arch = "wasm32"))]
pub mod restore;
#[cfg(any(test, feature = "sample"))]
pub mod sample;
pub mod store_impl;
pub mod store_trait;

pub use store_impl::Store;
pub use store_impl::{check_validate, open_store};
pub use store_trait::FiberStore;

#[cfg(test)]
mod tests;
