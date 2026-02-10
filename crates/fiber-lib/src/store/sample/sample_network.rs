/// StoreSample implementation for `PersistentNetworkActorState`.
///
/// Since `PersistentNetworkActorState` has private fields, we construct
/// populated instances via `serde_json` deserialization.
use crate::fiber::network::PersistentNetworkActorState;
use crate::store::schema::PEER_ID_NETWORK_ACTOR_STATE_PREFIX;

use super::{deterministic_pubkey, StoreSample};

impl StoreSample for PersistentNetworkActorState {
    const STORE_PREFIX: u8 = PEER_ID_NETWORK_ACTOR_STATE_PREFIX;
    const TYPE_NAME: &'static str = "PersistentNetworkActorState";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_minimal(), sample_full(seed)]
    }
}

/// Minimal state: empty HashMaps (the Default).
fn sample_minimal() -> PersistentNetworkActorState {
    PersistentNetworkActorState::new()
}

/// Full state: populated peer_pubkey_map and saved_peer_addresses.
///
/// Because the struct fields are private, we construct it via JSON
/// deserialization which has access to all fields through serde.
///
/// We use a single entry per HashMap to ensure deterministic serialization
/// (HashMap with one entry has a stable iteration order).
fn sample_full(seed: u64) -> PersistentNetworkActorState {
    let pubkey1 = deterministic_pubkey(seed, 0);
    // Derive PeerId from raw secp256k1 public key bytes via tentacle.
    let pk1_bytes = pubkey1.serialize();
    let tentacle_pk1 = tentacle::secio::PublicKey::from_raw_key(pk1_bytes.to_vec());
    let peer_id1 = tentacle::secio::PeerId::from_public_key(&tentacle_pk1);

    // Build JSON matching the serde_as format:
    // peer_pubkey_map and saved_peer_addresses are serialized as Vec<(String, _)>
    // because of #[serde_as(as = "Vec<(DisplayFromStr, _)>")].
    // Use single entries to guarantee deterministic HashMap serialization.
    let json = serde_json::json!({
        "peer_pubkey_map": [
            [peer_id1.to_base58(), pubkey1],
        ],
        "saved_peer_addresses": [
            [peer_id1.to_base58(), ["/ip4/127.0.0.1/tcp/8000", "/ip4/10.0.0.1/tcp/7000"]],
        ],
    });

    serde_json::from_value(json).expect("PersistentNetworkActorState JSON deserialization")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persistent_network_actor_state_samples_roundtrip() {
        PersistentNetworkActorState::verify_samples_roundtrip(42);
    }

    #[test]
    fn test_persistent_network_actor_state_samples_deterministic() {
        let bytes_a = PersistentNetworkActorState::sample_bytes(42);
        let bytes_b = PersistentNetworkActorState::sample_bytes(42);
        assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
    }

    #[test]
    fn test_persistent_network_actor_state_different_seeds() {
        let bytes_42 = PersistentNetworkActorState::sample_bytes(42);
        let bytes_99 = PersistentNetworkActorState::sample_bytes(99);
        assert_ne!(
            bytes_42, bytes_99,
            "Different seeds should produce different bytes"
        );
    }

    #[test]
    fn test_persistent_network_actor_state_sample_count() {
        let samples = PersistentNetworkActorState::samples(42);
        assert_eq!(samples.len(), 2, "Should produce 2 sample variants");
    }
}
