use std::collections::VecDeque;

use fiber_types::schema::CHANNEL_ACTOR_STATE_PREFIX;

use crate::fiber::channel::ChannelActorState;

use super::StoreSample;

/// StoreSample implementation for `ChannelActorState`.
///
/// Provides deterministic sample instances for migration testing.
impl StoreSample for ChannelActorState {
    const STORE_PREFIX: u8 = CHANNEL_ACTOR_STATE_PREFIX;
    const TYPE_NAME: &'static str = "ChannelActorState";

    fn samples(seed: u64) -> Vec<Self> {
        vec![Self::sample_minimal(seed), Self::sample_full(seed)]
    }
}

impl ChannelActorState {
    /// Sample 0: Minimal state — use ChannelActorData sample from fiber-types and add runtime fields.
    fn sample_minimal(seed: u64) -> Self {
        let core = fiber_types::ChannelActorData::samples(seed)
            .into_iter()
            .next()
            .expect("ChannelActorData samples should not be empty");

        ChannelActorState {
            core,
            waiting_peer_response: None,
            network: None,
            scheduled_channel_update_handle: None,
            pending_notify_settle_tlcs: vec![],
            defer_peer_tlc_updates: false,
            deferred_peer_tlc_updates: VecDeque::new(),
            ephemeral_config: Default::default(),
            private_key: None,
        }
    }

    /// Sample 1: Fully populated state — use ChannelActorData::sample_full from fiber-types.
    /// All complex types are already populated in fiber-types sample_full.
    fn sample_full(seed: u64) -> Self {
        let core = fiber_types::ChannelActorData::samples(seed)
            .into_iter()
            .nth(1)
            .expect("ChannelActorData samples should have at least 2 elements");

        ChannelActorState {
            core,
            waiting_peer_response: None,
            network: None,
            scheduled_channel_update_handle: None,
            pending_notify_settle_tlcs: vec![],
            defer_peer_tlc_updates: false,
            deferred_peer_tlc_updates: VecDeque::new(),
            ephemeral_config: Default::default(),
            private_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(samples.len(), 2, "Should produce 2 sample variants");
    }
}
