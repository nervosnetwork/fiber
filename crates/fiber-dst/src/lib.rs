//! Deterministic simulation testing for the Fiber Network.

#![cfg(not(target_arch = "wasm32"))]

mod harness;
mod host;
mod scenario;
mod tower;

pub use scenario::{run, RandomFaultPolicy, Scenario};
