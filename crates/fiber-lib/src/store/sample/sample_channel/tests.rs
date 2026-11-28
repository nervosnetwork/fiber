use super::*;

#[test]
fn test_channel_actor_state_samples_roundtrip() {
    ChannelActorState::verify_samples_roundtrip(42);
}

#[test]
fn test_channel_actor_state_samples_deterministic() {
    let bytes_a = ChannelActorState::sample_bytes(42);
    let bytes_b = ChannelActorState::sample_bytes(42);
    assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
}

#[test]
fn test_channel_actor_state_different_seeds() {
    let bytes_42 = ChannelActorState::sample_bytes(42);
    let bytes_99 = ChannelActorState::sample_bytes(99);
    assert_ne!(
        bytes_42, bytes_99,
        "Different seeds should produce different bytes"
    );
}

#[test]
fn test_channel_actor_state_sample_count() {
    let samples = ChannelActorState::samples(42);
    assert_eq!(samples.len(), 1, "A genuine V2 bootstrap sample");
    let ids = samples
        .iter()
        .map(|sample| sample.id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        ids.len(),
        samples.len(),
        "Each sample needs a unique store key"
    );
}
