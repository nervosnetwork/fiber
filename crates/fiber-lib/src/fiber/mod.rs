pub mod channel;
pub mod config;
pub mod features;
pub mod gossip;
pub mod graph;
pub mod hash_algorithm;
pub mod history;
pub mod network;
pub mod payment;
#[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
pub mod profiling;
pub mod serde_utils;
pub mod types;

mod fee;
mod in_flight_ckb_tx_actor;
mod key;
mod path;
mod settle_tlc_set_command;

pub use config::FiberConfig;
pub use fiber_types::*;
pub use in_flight_ckb_tx_actor::{
    InFlightCkbTxActor, InFlightCkbTxActorArguments, InFlightCkbTxActorMessage, InFlightCkbTxKind,
};
pub use key::KeyPair;
pub use network::{
    start_network, NetworkActor, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    NetworkServiceEvent,
};
pub use payment::{HopHint, PaymentCustomRecords};
pub use settle_tlc_set_command::SettleTlcSetCommand;

pub(crate) const ASSUME_NETWORK_ACTOR_ALIVE: &str = "network actor must be alive";

#[cfg(test)]
pub mod tests;
