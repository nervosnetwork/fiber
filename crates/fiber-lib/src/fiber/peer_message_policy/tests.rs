use fiber_types::Privkey;

use crate::fiber::{
    network::CHANNEL_REESTABLISH_INTERVAL,
    types::{FiberMessage, ReestablishChannelV2},
};

use super::*;

fn expect_allowed(policy: &mut PeerMessagePolicy, peer: &Pubkey, bytes: u64, now_ms: u64) {
    assert_eq!(
        policy.admit(peer, bytes, now_ms),
        PeerMessageAdmission::Allow
    );
    policy.release_ingress(bytes);
}

#[test]
fn large_reconnect_overflow_disconnects_without_temp_ban() {
    let peer = Privkey::from_slice(&[1u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(
        8,
        PEER_MESSAGE_BURST.saturating_add(1),
        PEER_MESSAGE_BURST_BYTES.saturating_add(1),
    );

    for _ in 0..PEER_MESSAGE_BURST {
        expect_allowed(&mut policy, &peer, 1, 0);
    }
    assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Disconnect);
    policy.on_disconnected(&peer, 0);
    assert!(!policy.is_banned(&peer, 0));
    expect_allowed(&mut policy, &peer, 1, PEER_MESSAGE_INTERVAL_MS);
}

#[test]
fn clean_reconnect_preserves_depleted_message_bucket() {
    let peer = Privkey::from_slice(&[19u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(
        8,
        PEER_MESSAGE_BURST.saturating_add(1),
        PEER_MESSAGE_BURST_BYTES.saturating_add(1),
    );

    for _ in 0..PEER_MESSAGE_BURST {
        expect_allowed(&mut policy, &peer, 1, 0);
    }
    policy.on_disconnected(&peer, 0);

    assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Disconnect);
}

#[test]
fn clean_reconnect_preserves_depleted_byte_bucket() {
    let peer = Privkey::from_slice(&[20u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(
        8,
        PEER_MESSAGE_BURST.saturating_add(1),
        PEER_MESSAGE_BURST_BYTES.saturating_add(1),
    );

    expect_allowed(&mut policy, &peer, PEER_MESSAGE_BURST_BYTES, 0);
    policy.on_disconnected(&peer, 0);

    assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Disconnect);
}

#[test]
fn bans_only_after_repeated_overflow_disconnects() {
    let peer = Privkey::from_slice(&[2u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(
        8,
        PEER_MESSAGE_BURST.saturating_add(1),
        PEER_MESSAGE_BURST_BYTES.saturating_add(1),
    );

    for _ in 0..PEER_MESSAGE_BURST {
        expect_allowed(&mut policy, &peer, 1, 0);
    }
    for _ in 1..PEER_MESSAGE_VIOLATION_BAN_THRESHOLD {
        assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Disconnect);
        policy.on_disconnected(&peer, 0);
    }
    assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Ban);

    expect_allowed(&mut policy, &peer, 1, PEER_MESSAGE_BAN_DURATION_MS);
}

#[test]
fn paced_reestablishment_allows_more_channels_than_peer_burst() {
    const PERSISTED_CHANNELS: u32 = 540;
    let peer = Privkey::from_slice(&[21u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig::default());
    let frame_bytes = FiberMessage::reestablish_channel_v2(ReestablishChannelV2 {
        channel_id: Default::default(),
        next_ack_number: 0,
        next_commitment_number: 0,
        next_local_commitment_nonce: musig2::SecNonce::build([21u8; 32]).build().public_nonce(),
    })
    .to_molecule_bytes()
    .len() as u64;
    assert_eq!(frame_bytes, 118);
    for index in 0..PERSISTED_CHANNELS {
        expect_allowed(
            &mut policy,
            &peer,
            frame_bytes,
            u64::from(index).saturating_mul(CHANNEL_REESTABLISH_INTERVAL.as_millis() as u64),
        );
    }
    assert!(!policy.is_banned(
        &peer,
        u64::from(PERSISTED_CHANNELS)
            .saturating_mul(CHANNEL_REESTABLISH_INTERVAL.as_millis() as u64)
    ));
}

#[test]
fn temp_bans_repeated_invalid_messages_across_disconnect() {
    let peer = Privkey::from_slice(&[3u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig::default());

    for _ in 1..PEER_MESSAGE_VIOLATION_BAN_THRESHOLD {
        assert!(!policy.record_invalid(&peer, 1_000));
    }
    policy.on_disconnected(&peer, 1_001);
    assert!(policy.record_invalid(&peer, 1_002));
    assert!(policy.is_banned(&peer, 1_002));

    assert!(!policy.is_banned(
        &peer,
        1_002_u64.saturating_add(PEER_MESSAGE_BAN_DURATION_MS)
    ));
}

#[test]
fn invalid_score_uses_fixed_window() {
    let peer = Privkey::from_slice(&[4u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig::default());

    for index in 0..PEER_MESSAGE_VIOLATION_BAN_THRESHOLD {
        let now_ms =
            u64::from(index).saturating_mul(PEER_MESSAGE_BAN_DURATION_MS.saturating_sub(1));
        assert!(!policy.record_invalid(&peer, now_ms));
    }
    assert!(!policy.is_banned(
        &peer,
        u64::from(PEER_MESSAGE_VIOLATION_BAN_THRESHOLD)
            .saturating_mul(PEER_MESSAGE_BAN_DURATION_MS)
    ));
}

#[test]
fn global_message_budget_cannot_be_bypassed_by_many_peers() {
    let peer1 = Privkey::from_slice(&[5u8; 32]).pubkey();
    let peer2 = Privkey::from_slice(&[6u8; 32]).pubkey();
    let peer3 = Privkey::from_slice(&[7u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(8, 2, 100);

    assert_eq!(policy.admit(&peer1, 10, 0), PeerMessageAdmission::Allow);
    assert_eq!(policy.admit(&peer2, 10, 0), PeerMessageAdmission::Allow);
    assert_eq!(policy.in_flight(), (2, 20));
    assert_eq!(policy.admit(&peer3, 1, 0), PeerMessageAdmission::Disconnect);

    policy.release_ingress(10);
    assert_eq!(policy.admit(&peer3, 1, 0), PeerMessageAdmission::Allow);
    assert_eq!(policy.in_flight(), (2, 11));
    policy.release_ingress(10);
    policy.release_ingress(1);
    assert_eq!(policy.in_flight(), (0, 0));
}

#[test]
fn global_byte_budget_releases_on_drop() {
    let peer1 = Privkey::from_slice(&[8u8; 32]).pubkey();
    let peer2 = Privkey::from_slice(&[9u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(8, 10, 5);

    assert_eq!(policy.admit(&peer1, 4, 0), PeerMessageAdmission::Allow);
    assert_eq!(policy.admit(&peer2, 2, 0), PeerMessageAdmission::Disconnect);
    policy.release_ingress(4);
    expect_allowed(&mut policy, &peer2, 2, 0);
    assert_eq!(policy.in_flight(), (0, 0));
}

#[test]
fn per_peer_byte_overflow_disconnects_without_ban() {
    let peer = Privkey::from_slice(&[10u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(8, 10, u64::MAX);

    assert_eq!(
        policy.admit(&peer, PEER_MESSAGE_BURST_BYTES.saturating_add(1), 0),
        PeerMessageAdmission::Disconnect
    );
    assert!(!policy.is_banned(&peer, 0));
}

#[test]
fn capacity_prefers_retaining_active_bans() {
    let banned_peer = Privkey::from_slice(&[12u8; 32]).pubkey();
    let ordinary_peer = Privkey::from_slice(&[13u8; 32]).pubkey();
    let new_peer = Privkey::from_slice(&[14u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(2, 10, 100);

    for _ in 0..PEER_MESSAGE_VIOLATION_BAN_THRESHOLD {
        policy.record_invalid(&banned_peer, 0);
    }
    expect_allowed(&mut policy, &ordinary_peer, 1, 1);
    expect_allowed(&mut policy, &new_peer, 1, 2);

    assert_eq!(policy.peers.len(), 2);
    assert!(policy.peers.contains_key(&banned_peer));
    assert!(!policy.peers.contains_key(&ordinary_peer));
    assert!(policy.peers.contains_key(&new_peer));
}

#[test]
fn capacity_evicts_expired_bans_before_ordinary_entries() {
    let expired_banned_peer = Privkey::from_slice(&[16u8; 32]).pubkey();
    let ordinary_peer = Privkey::from_slice(&[17u8; 32]).pubkey();
    let new_peer = Privkey::from_slice(&[18u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::with_limits(2, 10, 100);

    for _ in 0..PEER_MESSAGE_VIOLATION_BAN_THRESHOLD {
        policy.record_invalid(&expired_banned_peer, 0);
    }
    expect_allowed(
        &mut policy,
        &ordinary_peer,
        1,
        PEER_MESSAGE_BAN_DURATION_MS + 1,
    );
    expect_allowed(&mut policy, &new_peer, 1, PEER_MESSAGE_BAN_DURATION_MS + 2);

    assert_eq!(policy.peers.len(), 2);
    assert!(!policy.peers.contains_key(&expired_banned_peer));
    assert!(policy.peers.contains_key(&ordinary_peer));
    assert!(policy.peers.contains_key(&new_peer));
}

#[test]
fn fiber_config_default_matches_policy_default() {
    let config = crate::fiber::config::FiberConfig::default();
    assert_eq!(
        config.peer_message_policy,
        PeerMessagePolicyConfig::default()
    );
}

#[test]
fn config_deserializes_partial_overrides() {
    let config: PeerMessagePolicyConfig =
        serde_json::from_str(r#"{"peer_message_interval_ms":1,"max_in_flight_messages":0}"#)
            .expect("deserialize peer message policy config");

    assert_eq!(config.peer_message_interval_ms, 1);
    assert_eq!(config.peer_message_burst, PEER_MESSAGE_BURST);
    assert_eq!(
        config.peer_message_rate_bytes_per_sec,
        PEER_MESSAGE_RATE_BYTES_PER_SEC
    );
    assert_eq!(config.peer_message_burst_bytes, PEER_MESSAGE_BURST_BYTES);
    assert_eq!(
        config.violation_ban_threshold,
        PEER_MESSAGE_VIOLATION_BAN_THRESHOLD
    );
    assert_eq!(config.ban_duration_ms, PEER_MESSAGE_BAN_DURATION_MS);
    assert_eq!(config.max_tracked_peers, PEER_MESSAGE_MAX_TRACKED_PEERS);
    assert_eq!(config.max_in_flight_messages, 0);
    assert_eq!(
        config.max_in_flight_bytes,
        FIBER_INGRESS_MAX_IN_FLIGHT_BYTES
    );
}

#[test]
fn disabled_message_limit_allows_unbounded_burst() {
    let peer = Privkey::from_slice(&[30u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig {
        peer_message_interval_ms: 0,
        peer_message_burst: 0,
        ..PeerMessagePolicyConfig::default()
    });

    // Far beyond the default 400-message burst in a single instant.
    for _ in 0..10_000 {
        expect_allowed(&mut policy, &peer, 1, 0);
    }
    assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Allow);
    policy.release_ingress(1);
}

#[test]
fn disabled_byte_limit_allows_large_frames() {
    let peer = Privkey::from_slice(&[31u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig {
        peer_message_rate_bytes_per_sec: 0,
        peer_message_burst_bytes: 0,
        // Unlimited ingress bytes, otherwise the huge frame fails the ingress budget.
        max_in_flight_bytes: 0,
        ..PeerMessagePolicyConfig::default()
    });

    assert_eq!(
        policy.admit(&peer, 1024 * 1024 * 1024, 0),
        PeerMessageAdmission::Allow
    );
    policy.release_ingress(1024 * 1024 * 1024);
}

#[test]
fn unlimited_ingress_budget_never_disconnects_on_overflow() {
    let peer = Privkey::from_slice(&[32u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig {
        // Disable the per-peer message limit too, otherwise the default 400-message
        // burst rejects messages before the ingress budget is exercised.
        peer_message_interval_ms: 0,
        peer_message_burst: 0,
        max_in_flight_messages: 0,
        max_in_flight_bytes: 0,
        ..PeerMessagePolicyConfig::default()
    });

    // Far beyond the default 4096-message in-flight budget: the budget is unlimited,
    // so every message is admitted even without releasing any capacity.
    for _ in 0..20_000 {
        assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Allow);
    }
    assert_eq!(policy.in_flight(), (20_000, 20_000));
}

#[test]
fn zero_ban_threshold_never_bans() {
    let peer = Privkey::from_slice(&[33u8; 32]).pubkey();
    let mut policy = PeerMessagePolicy::new(PeerMessagePolicyConfig {
        violation_ban_threshold: 0,
        ..PeerMessagePolicyConfig::default()
    });

    // Exhaust the message bucket, then keep overflowing: repeated violations only
    // disconnect, never ban.
    for _ in 0..PEER_MESSAGE_BURST {
        expect_allowed(&mut policy, &peer, 1, 0);
    }
    for _ in 0..PEER_MESSAGE_VIOLATION_BAN_THRESHOLD.saturating_mul(2) {
        assert_eq!(policy.admit(&peer, 1, 0), PeerMessageAdmission::Disconnect);
        policy.on_disconnected(&peer, 0);
    }
    assert!(!policy.is_banned(&peer, 0));
}

impl PeerMessagePolicy {
    pub(crate) fn in_flight(&self) -> (u32, u64) {
        (
            self.ingress.in_flight_messages,
            self.ingress.in_flight_bytes,
        )
    }
}

impl PeerMessagePolicy {
    pub(crate) fn with_limits(
        max_entries: usize,
        max_in_flight_messages: u32,
        max_in_flight_bytes: u64,
    ) -> Self {
        Self::new(PeerMessagePolicyConfig {
            max_tracked_peers: max_entries.max(1),
            max_in_flight_messages,
            max_in_flight_bytes,
            ..PeerMessagePolicyConfig::default()
        })
    }
}
