#[cfg(not(target_arch = "wasm32"))]
mod migrate;
#[cfg(not(target_arch = "wasm32"))]
mod payment_hash_index;
#[cfg(feature = "watchtower")]
mod store;
