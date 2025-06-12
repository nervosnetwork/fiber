#[cfg(not(target_arch = "wasm32"))]
pub mod lnd_test_utils;

#[cfg(all(not(target_arch = "wasm32"), test))]
mod lnd_tests;
#[cfg(test)]
mod payment;
