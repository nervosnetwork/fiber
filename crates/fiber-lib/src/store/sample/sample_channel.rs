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
        vec![Self::sample_minimal(seed)]
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
            channel_signer: crate::fiber::channel_signer::ChannelSigner::restore(
                core.signer.as_ref(),
                &core.local_channel_public_keys,
            )
            .unwrap(),
            core,
            waiting_peer_response: None,
            reestablish_timer: None,
            recovery_peer_v2: None,
            network: None,
            scheduled_channel_update_handle: None,
            pending_notify_settle_tlcs: vec![],
            defer_peer_tlc_updates: false,
            deferred_peer_tlc_updates: VecDeque::new(),
            ephemeral_config: Default::default(),
            funding_abort_detail: None,
            private_key: None,
            needs_backup: false,
            pending_messages: Default::default(),
            musig2_agg_context_local_first: Default::default(),
            musig2_agg_context_remote_first: Default::default(),
            pending_remove_effects_v2: 0,
            pending_payment_notifications: Default::default(),
            replayed_tlc_updates_v2: 0,
            resigning_outgoing_v2: false,
        }
    }
}

#[cfg(test)]
mod tests;
