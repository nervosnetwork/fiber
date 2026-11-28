/// StoreSample implementation for `PersistentNetworkActorState`.
///
/// Since `PersistentNetworkActorState` has private fields, we construct
/// populated instances via `serde_json` deserialization.
use crate::network::PersistentNetworkActorState;
use crate::schema::PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX;

use super::{deterministic_pubkey, StoreSample};

impl StoreSample for PersistentNetworkActorState {
    const STORE_PREFIX: u8 = PUBLIC_KEY_NETWORK_ACTOR_STATE_PREFIX;
    const TYPE_NAME: &'static str = "PersistentNetworkActorState";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_minimal(), sample_full(seed)]
    }
}

/// Minimal state: empty HashMaps (the Default).
fn sample_minimal() -> PersistentNetworkActorState {
    PersistentNetworkActorState::new()
}

/// Full state: populated saved_peer_addresses.
///
/// Because the struct fields are private, we construct it via JSON
/// deserialization which has access to all fields through serde.
///
/// We use a single entry per HashMap to ensure deterministic serialization
/// (HashMap with one entry has a stable iteration order).
fn sample_full(seed: u64) -> PersistentNetworkActorState {
    let pubkey1 = deterministic_pubkey(seed, 0);

    // Build JSON for HashMap<Pubkey, Vec<Multiaddr>>.
    // Use a single entry to guarantee deterministic HashMap serialization.
    let json = serde_json::json!({
        "saved_peer_addresses": {
            (hex::encode(pubkey1.serialize())): ["/ip4/127.0.0.1/tcp/8000", "/ip4/10.0.0.1/tcp/7000"],
        },
    });

    serde_json::from_value(json).expect("PersistentNetworkActorState JSON deserialization")
}

#[cfg(test)]
mod tests;
