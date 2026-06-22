use crate::fiber::gossip_policy::{
    ByteRateLimitConfig, ByteTokenBucket, ChannelUpdateLimiterKey, ChannelUpdateRateLimitConfig,
    GossipBanConfig, GossipPolicyConfig, GossipPolicyState, GossipViolation,
};
use crate::fiber::{config::FiberConfig, gossip::GossipConfig};
use crate::tests::gen_utils::{gen_rand_channel_outpoint, gen_rand_fiber_public_key};

#[test]
fn test_ban_tracker_marks_peer_banned_after_threshold() {
    let peer = gen_rand_fiber_public_key();
    let now_ms = 1_000;
    let ban_config = GossipBanConfig {
        threshold: 100,
        duration_ms: 600_000,
    };
    let mut tracker = ban_config.build_tracker();

    assert_eq!(tracker.score(&peer, now_ms), 0);
    assert!(!tracker.record_violation(&peer, GossipViolation::PolicyRejectedMessage, now_ms));
    assert_eq!(tracker.score(&peer, now_ms), 25);
    assert!(tracker.record_violation(&peer, GossipViolation::InvalidBroadcastMessage, now_ms));
    assert!(tracker.is_banned(&peer, now_ms));
    assert_eq!(tracker.score(&peer, now_ms), 125);
}

#[test]
fn test_byte_token_bucket_honors_burst_then_refills() {
    let mut bucket = ByteTokenBucket::new(ByteRateLimitConfig {
        rate_bytes_per_sec: 100,
        burst_bytes: 200,
    });

    assert!(bucket.try_consume(150, 0));
    assert!(!bucket.try_consume(60, 0));
    assert!(bucket.try_consume(50, 500));
    assert!(bucket.try_consume(100, 1_500));
    assert!(!bucket.try_consume(101, 1_500));
}

#[test]
fn test_channel_update_limiter_keys_node1_and_node2_separately() {
    let outpoint = gen_rand_channel_outpoint();
    let peer = gen_rand_fiber_public_key();
    let mut limiter = ChannelUpdateRateLimitConfig {
        interval_ms: 60_000,
        burst: 1,
    }
    .build_limiter();

    let node1_key = ChannelUpdateLimiterKey::new(peer, outpoint.clone(), true);
    let node2_key = ChannelUpdateLimiterKey::new(peer, outpoint, false);

    assert!(limiter.try_acquire(&node1_key, 0));
    assert!(!limiter.try_acquire(&node1_key, 0));
    assert!(limiter.try_acquire(&node2_key, 0));
}

#[test]
fn test_channel_update_limiter_rejects_new_keys_when_full() {
    let peer = gen_rand_fiber_public_key();
    let mut limiter = ChannelUpdateRateLimitConfig {
        interval_ms: 60_000,
        burst: 1,
    }
    .build_limiter_with_max_entries(2);

    let key1 = ChannelUpdateLimiterKey::new(peer, gen_rand_channel_outpoint(), true);
    let key2 = ChannelUpdateLimiterKey::new(peer, gen_rand_channel_outpoint(), true);
    let key3 = ChannelUpdateLimiterKey::new(peer, gen_rand_channel_outpoint(), true);

    assert!(limiter.try_acquire(&key1, 1));
    assert!(limiter.try_acquire(&key2, 2));
    assert_eq!(limiter.tracked_keys(), 2);

    assert!(!limiter.try_acquire(&key3, 3));
    assert_eq!(limiter.tracked_keys(), 2);

    assert!(!limiter.try_acquire(&key1, 4));
    assert!(limiter.try_acquire(&key1, 60_001));
    assert_eq!(limiter.tracked_keys(), 2);
}

#[test]
fn test_channel_update_limiter_prunes_idle_keys_before_accepting_new_key() {
    let peer = gen_rand_fiber_public_key();
    let mut limiter = ChannelUpdateRateLimitConfig {
        interval_ms: 60_000,
        burst: 1,
    }
    .build_limiter_with_max_entries(2);

    let key1 = ChannelUpdateLimiterKey::new(peer, gen_rand_channel_outpoint(), true);
    let key2 = ChannelUpdateLimiterKey::new(peer, gen_rand_channel_outpoint(), true);
    let key3 = ChannelUpdateLimiterKey::new(peer, gen_rand_channel_outpoint(), true);

    assert!(limiter.try_acquire(&key1, 0));
    assert!(limiter.try_acquire(&key2, 1));
    assert_eq!(limiter.tracked_keys(), 2);

    assert!(limiter.try_acquire(&key3, 60_001));
    assert_eq!(limiter.tracked_keys(), 1);
    assert!(!limiter.try_acquire(&key3, 60_001));
}

#[test]
fn test_gossip_policy_config_default_contains_recommended_values() {
    let config = GossipPolicyConfig::default();

    assert_eq!(config.ban.threshold, 100);
    assert_eq!(config.ban.duration_ms, 600_000);
    assert_eq!(config.outbound_global.rate_bytes_per_sec, 1_024_000);
    assert_eq!(config.outbound_global.burst_bytes, 2_048_000);
    assert_eq!(config.outbound_peer.rate_bytes_per_sec, 51_200);
    assert_eq!(config.outbound_peer.burst_bytes, 102_400);
    assert_eq!(config.outbound_delay_queue_capacity, 1_024);
    assert_eq!(config.inbound_channel_update.interval_ms, 60_000);
    assert_eq!(config.inbound_channel_update.burst, 10);
}

#[test]
fn test_gossip_config_uses_default_policy_config() {
    let config = FiberConfig::default();
    let gossip_config = GossipConfig::from(&config);

    assert_eq!(gossip_config.policy, GossipPolicyConfig::default());
}

#[test]
fn test_zero_values_disable_corresponding_limiters() {
    let config = FiberConfig {
        gossip_policy: GossipPolicyConfig {
            ban: GossipBanConfig {
                threshold: 0,
                duration_ms: 0,
            },
            outbound_global: ByteRateLimitConfig {
                rate_bytes_per_sec: 0,
                burst_bytes: 0,
            },
            outbound_peer: ByteRateLimitConfig {
                rate_bytes_per_sec: 0,
                burst_bytes: 0,
            },
            outbound_delay_queue_capacity: 0,
            inbound_channel_update: ChannelUpdateRateLimitConfig {
                interval_ms: 0,
                burst: 0,
            },
        },
        ..FiberConfig::default()
    };

    let gossip_config = GossipConfig::from(&config);

    assert!(gossip_config.policy.ban.is_disabled());
    assert!(gossip_config.policy.outbound_global.is_disabled());
    assert!(gossip_config.policy.outbound_peer.is_disabled());
    assert!(gossip_config.policy.inbound_channel_update.is_disabled());
}

#[test]
fn test_gossip_policy_config_deserializes_partial_defaults() {
    let config: GossipPolicyConfig =
        serde_json::from_str(r#"{"ban":{"threshold":0},"outbound_peer":{"burst_bytes":2048}}"#)
            .expect("deserialize gossip policy config");

    assert_eq!(config.ban.threshold, 0);
    assert_eq!(config.ban.duration_ms, 600_000);
    assert_eq!(config.outbound_global.rate_bytes_per_sec, 1_024_000);
    assert_eq!(config.outbound_global.burst_bytes, 2_048_000);
    assert_eq!(config.outbound_peer.rate_bytes_per_sec, 51_200);
    assert_eq!(config.outbound_peer.burst_bytes, 2_048);
    assert_eq!(config.outbound_delay_queue_capacity, 1_024);
    assert_eq!(config.inbound_channel_update.interval_ms, 60_000);
    assert_eq!(config.inbound_channel_update.burst, 10);
}

#[test]
fn test_outbound_single_message_capacity_ignores_disabled_limiters() {
    let global_disabled = GossipPolicyState::new(GossipPolicyConfig {
        ban: GossipBanConfig::default(),
        outbound_global: ByteRateLimitConfig {
            rate_bytes_per_sec: 0,
            burst_bytes: 0,
        },
        outbound_peer: ByteRateLimitConfig {
            rate_bytes_per_sec: 100,
            burst_bytes: 100,
        },
        outbound_delay_queue_capacity: 16,
        inbound_channel_update: ChannelUpdateRateLimitConfig {
            interval_ms: 60_000,
            burst: 10,
        },
    });
    assert!(!global_disabled.outbound_message_exceeds_single_message_capacity(100));
    assert!(global_disabled.outbound_message_exceeds_single_message_capacity(101));

    let peer_disabled = GossipPolicyState::new(GossipPolicyConfig {
        ban: GossipBanConfig::default(),
        outbound_global: ByteRateLimitConfig {
            rate_bytes_per_sec: 200,
            burst_bytes: 200,
        },
        outbound_peer: ByteRateLimitConfig {
            rate_bytes_per_sec: 0,
            burst_bytes: 0,
        },
        outbound_delay_queue_capacity: 16,
        inbound_channel_update: ChannelUpdateRateLimitConfig {
            interval_ms: 60_000,
            burst: 10,
        },
    });
    assert!(!peer_disabled.outbound_message_exceeds_single_message_capacity(200));
    assert!(peer_disabled.outbound_message_exceeds_single_message_capacity(201));
}

#[test]
fn test_ban_tracker_expires_and_resets_state() {
    let peer = gen_rand_fiber_public_key();
    let mut tracker = GossipBanConfig {
        threshold: 100,
        duration_ms: 100,
    }
    .build_tracker();

    assert!(tracker.record_violation(&peer, GossipViolation::InvalidBroadcastMessage, 1_000));
    assert!(tracker.is_banned(&peer, 1_050));
    assert_eq!(tracker.score(&peer, 1_050), 100);
    assert!(!tracker.is_banned(&peer, 1_100));
    assert_eq!(tracker.score(&peer, 1_100), 0);
}

#[test]
fn test_ban_tracker_prunes_expired_entries_without_reaccess() {
    let peer1 = gen_rand_fiber_public_key();
    let peer2 = gen_rand_fiber_public_key();
    let peer3 = gen_rand_fiber_public_key();
    let mut tracker = GossipBanConfig {
        threshold: 100,
        duration_ms: 100,
    }
    .build_tracker_with_max_entries(10);

    assert!(tracker.record_violation(&peer1, GossipViolation::InvalidBroadcastMessage, 1_000));
    assert!(tracker.record_violation(&peer2, GossipViolation::InvalidBroadcastMessage, 1_010));
    assert!(!tracker.record_violation(&peer3, GossipViolation::PolicyRejectedMessage, 1_020));
    assert_eq!(tracker.tracked_peers(), 3);

    tracker.prune_expired(1_200);

    assert_eq!(tracker.tracked_peers(), 0);
}

#[test]
fn test_ban_tracker_cap_evicts_oldest_low_priority_entry_first() {
    let peer1 = gen_rand_fiber_public_key();
    let peer2 = gen_rand_fiber_public_key();
    let peer3 = gen_rand_fiber_public_key();
    let mut tracker = GossipBanConfig {
        threshold: 100,
        duration_ms: 1_000,
    }
    .build_tracker_with_max_entries(2);

    assert!(!tracker.record_violation(&peer1, GossipViolation::PolicyRejectedMessage, 1_000));
    assert!(tracker.record_violation(&peer2, GossipViolation::InvalidBroadcastMessage, 1_010));
    assert_eq!(tracker.tracked_peers(), 2);

    assert!(tracker.record_violation(&peer3, GossipViolation::InvalidBroadcastMessage, 1_020));

    assert_eq!(tracker.tracked_peers(), 2);
    assert_eq!(tracker.score(&peer1, 1_020), 0);
    assert!(tracker.is_banned(&peer2, 1_020));
    assert!(tracker.is_banned(&peer3, 1_020));
}

#[test]
fn test_channel_update_limiter_refills_one_token_per_interval() {
    let key = ChannelUpdateLimiterKey::new(
        gen_rand_fiber_public_key(),
        gen_rand_channel_outpoint(),
        true,
    );
    let mut limiter = ChannelUpdateRateLimitConfig {
        interval_ms: 60_000,
        burst: 1,
    }
    .build_limiter();

    assert!(limiter.try_acquire(&key, 0));
    assert!(!limiter.try_acquire(&key, 1));
    assert!(limiter.try_acquire(&key, 60_000));
}

#[test]
fn test_gossip_policy_state_applies_global_and_peer_outbound_limits() {
    let mut state = GossipPolicyState::new(GossipPolicyConfig {
        ban: GossipBanConfig::default(),
        outbound_global: ByteRateLimitConfig {
            rate_bytes_per_sec: 100,
            burst_bytes: 100,
        },
        outbound_peer: ByteRateLimitConfig {
            rate_bytes_per_sec: 50,
            burst_bytes: 50,
        },
        outbound_delay_queue_capacity: 16,
        inbound_channel_update: ChannelUpdateRateLimitConfig {
            interval_ms: 60_000,
            burst: 10,
        },
    });
    let peer = gen_rand_fiber_public_key();

    assert!(state.allow_outbound_message(&peer, 40, 0));
    assert!(!state.allow_outbound_message(&peer, 20, 0));
    assert!(state.allow_outbound_message(&peer, 20, 1_000));
}

#[test]
fn test_gossip_policy_state_reserves_outbound_delay_from_current_tokens() {
    let mut state = GossipPolicyState::new(GossipPolicyConfig {
        ban: GossipBanConfig::default(),
        outbound_global: ByteRateLimitConfig {
            rate_bytes_per_sec: 1_000,
            burst_bytes: 1_000,
        },
        outbound_peer: ByteRateLimitConfig {
            rate_bytes_per_sec: 50,
            burst_bytes: 50,
        },
        outbound_delay_queue_capacity: 16,
        inbound_channel_update: ChannelUpdateRateLimitConfig {
            interval_ms: 60_000,
            burst: 10,
        },
    });
    let peer = gen_rand_fiber_public_key();

    assert_eq!(state.reserve_outbound_message(&peer, 40, 0), 0);
    assert_eq!(state.reserve_outbound_message(&peer, 20, 0), 200);
    assert_eq!(state.reserve_outbound_message(&peer, 20, 0), 600);
}

#[test]
fn test_channel_update_limiter_isolated_per_peer_for_same_channel_side() {
    let outpoint = gen_rand_channel_outpoint();
    let peer1 = gen_rand_fiber_public_key();
    let peer2 = gen_rand_fiber_public_key();
    let mut limiter = ChannelUpdateRateLimitConfig {
        interval_ms: 60_000,
        burst: 1,
    }
    .build_limiter();

    let key_peer1 = ChannelUpdateLimiterKey::new(peer1, outpoint.clone(), true);
    let key_peer2 = ChannelUpdateLimiterKey::new(peer2, outpoint, true);

    assert!(limiter.try_acquire(&key_peer1, 0));
    assert!(!limiter.try_acquire(&key_peer1, 0));
    assert!(limiter.try_acquire(&key_peer2, 0));
}

#[test]
fn test_outbound_single_message_capacity_detection() {
    let state = GossipPolicyState::new(GossipPolicyConfig {
        ban: GossipBanConfig::default(),
        outbound_global: ByteRateLimitConfig {
            rate_bytes_per_sec: 1_024,
            burst_bytes: 2_048,
        },
        outbound_peer: ByteRateLimitConfig {
            rate_bytes_per_sec: 100,
            burst_bytes: 100,
        },
        outbound_delay_queue_capacity: 16,
        inbound_channel_update: ChannelUpdateRateLimitConfig {
            interval_ms: 60_000,
            burst: 10,
        },
    });

    assert!(state.outbound_message_exceeds_single_message_capacity(101));
    assert!(!state.outbound_message_exceeds_single_message_capacity(100));
}
