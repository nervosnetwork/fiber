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
