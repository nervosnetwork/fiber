pub mod channel;
pub mod config;
pub mod gen;
pub mod graph;
pub mod hash_algorithm;
pub mod history;
pub mod network;
pub mod serde_utils;
pub mod types;

pub(crate) mod gossip;

mod fee;
mod in_flight_ckb_tx_actor;
mod key;
mod path;

pub use config::FiberConfig;
pub use in_flight_ckb_tx_actor::{
    InFlightCkbTxActor, InFlightCkbTxActorArguments, InFlightCkbTxActorMessage, InFlightCkbTxKind,
};
pub use key::KeyPair;
pub use network::{
    start_network, NetworkActor, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    NetworkServiceEvent, PaymentCustomRecords,
};

pub(crate) const ASSUME_NETWORK_ACTOR_ALIVE: &str = "network actor must be alive";

#[cfg(test)]
pub mod tests;
