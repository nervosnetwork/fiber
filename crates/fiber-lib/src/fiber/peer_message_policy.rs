use std::collections::{BTreeSet, HashMap};

use fiber_types::Pubkey;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::debug;

use super::gossip_policy::{ByteRateLimitConfig, ByteTokenBucket, DiscreteTokenBucket};

// Keep this admission control in the protocol callback, before the NetworkActor's unbounded
// mailbox. The actor separately scores semantically invalid messages, while the per-peer rate
// limits and global in-flight budget bound parsed work before that score is processed.
const PEER_MESSAGE_INTERVAL_MS: u64 = 5;
const PEER_MESSAGE_BURST: u32 = 400;
const PEER_MESSAGE_RATE_BYTES_PER_SEC: u64 = 1024 * 1024;
const PEER_MESSAGE_BURST_BYTES: u64 = 4 * 1024 * 1024;
const PEER_MESSAGE_VIOLATION_BAN_THRESHOLD: u32 = 20;
const PEER_MESSAGE_BAN_DURATION_MS: u64 = 10 * 60 * 1000;
const PEER_MESSAGE_MAX_TRACKED_PEERS: usize = 50_000;
const FIBER_INGRESS_MAX_IN_FLIGHT_MESSAGES: u32 = 4_096;
const FIBER_INGRESS_MAX_IN_FLIGHT_BYTES: u64 = 32 * 1024 * 1024;

/// Tunable parameters for the inbound Fiber peer message admission policy.
///
/// All fields default to the constants defined above. A field set to `0` disables the
/// corresponding limit:
/// - `peer_message_interval_ms == 0 || peer_message_burst == 0` disables the per-peer message limit;
/// - `peer_message_rate_bytes_per_sec == 0 || peer_message_burst_bytes == 0` disables the per-peer byte limit;
/// - `violation_ban_threshold == 0` disables temporary bans;
/// - `max_in_flight_messages == 0 || max_in_flight_bytes == 0` makes the global ingress budget unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PeerMessagePolicyConfig {
    /// Refill interval of the per-peer message token bucket, in milliseconds.
    pub peer_message_interval_ms: u64,
    /// Capacity (burst) of the per-peer message token bucket.
    pub peer_message_burst: u32,
    /// Sustained per-peer byte rate, in bytes per second.
    pub peer_message_rate_bytes_per_sec: u64,
    /// Burst size of the per-peer byte token bucket, in bytes.
    pub peer_message_burst_bytes: u64,
    /// Number of repeated violations before a peer is temporarily banned.
    pub violation_ban_threshold: u32,
    /// Duration of a temporary ban, in milliseconds.
    pub ban_duration_ms: u64,
    /// Maximum number of peers tracked by the policy (LRU eviction).
    pub max_tracked_peers: usize,
    /// Maximum number of inbound Fiber messages in flight globally.
    pub max_in_flight_messages: u32,
    /// Maximum bytes of inbound Fiber messages in flight globally.
    pub max_in_flight_bytes: u64,
}

impl Default for PeerMessagePolicyConfig {
    fn default() -> Self {
        Self {
            peer_message_interval_ms: PEER_MESSAGE_INTERVAL_MS,
            peer_message_burst: PEER_MESSAGE_BURST,
            peer_message_rate_bytes_per_sec: PEER_MESSAGE_RATE_BYTES_PER_SEC,
            peer_message_burst_bytes: PEER_MESSAGE_BURST_BYTES,
            violation_ban_threshold: PEER_MESSAGE_VIOLATION_BAN_THRESHOLD,
            ban_duration_ms: PEER_MESSAGE_BAN_DURATION_MS,
            max_tracked_peers: PEER_MESSAGE_MAX_TRACKED_PEERS,
            max_in_flight_messages: FIBER_INGRESS_MAX_IN_FLIGHT_MESSAGES,
            max_in_flight_bytes: FIBER_INGRESS_MAX_IN_FLIGHT_BYTES,
        }
    }
}

impl PeerMessagePolicyConfig {
    /// Whether the per-peer message token bucket is disabled.
    pub(crate) fn message_limit_disabled(&self) -> bool {
        self.peer_message_interval_ms == 0 || self.peer_message_burst == 0
    }

    /// Whether temporary bans are disabled.
    pub(crate) fn ban_disabled(&self) -> bool {
        self.violation_ban_threshold == 0
    }
}

impl<'de> Deserialize<'de> for PeerMessagePolicyConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct PartialPeerMessagePolicyConfig {
            peer_message_interval_ms: Option<u64>,
            peer_message_burst: Option<u32>,
            peer_message_rate_bytes_per_sec: Option<u64>,
            peer_message_burst_bytes: Option<u64>,
            violation_ban_threshold: Option<u32>,
            ban_duration_ms: Option<u64>,
            max_tracked_peers: Option<usize>,
            max_in_flight_messages: Option<u32>,
            max_in_flight_bytes: Option<u64>,
        }

        let partial = PartialPeerMessagePolicyConfig::deserialize(deserializer)?;
        let defaults = Self::default();
        Ok(Self {
            peer_message_interval_ms: partial
                .peer_message_interval_ms
                .unwrap_or(defaults.peer_message_interval_ms),
            peer_message_burst: partial
                .peer_message_burst
                .unwrap_or(defaults.peer_message_burst),
            peer_message_rate_bytes_per_sec: partial
                .peer_message_rate_bytes_per_sec
                .unwrap_or(defaults.peer_message_rate_bytes_per_sec),
            peer_message_burst_bytes: partial
                .peer_message_burst_bytes
                .unwrap_or(defaults.peer_message_burst_bytes),
            violation_ban_threshold: partial
                .violation_ban_threshold
                .unwrap_or(defaults.violation_ban_threshold),
            ban_duration_ms: partial.ban_duration_ms.unwrap_or(defaults.ban_duration_ms),
            max_tracked_peers: partial
                .max_tracked_peers
                .unwrap_or(defaults.max_tracked_peers),
            max_in_flight_messages: partial
                .max_in_flight_messages
                .unwrap_or(defaults.max_in_flight_messages),
            max_in_flight_bytes: partial
                .max_in_flight_bytes
                .unwrap_or(defaults.max_in_flight_bytes),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerMessageAdmission {
    Allow,
    Disconnect,
    Ban,
}

#[derive(Debug)]
struct ViolationWindow {
    count: u32,
    started_at_ms: u64,
}

impl ViolationWindow {
    fn new(now_ms: u64) -> Self {
        Self {
            count: 0,
            started_at_ms: now_ms,
        }
    }

    fn expire(&mut self, now_ms: u64, window_duration_ms: u64) {
        if self.count > 0 && now_ms.saturating_sub(self.started_at_ms) >= window_duration_ms {
            self.count = 0;
            self.started_at_ms = now_ms;
        }
    }

    fn record(&mut self, now_ms: u64, window_duration_ms: u64) -> u32 {
        self.expire(now_ms, window_duration_ms);
        if self.count == 0 {
            self.started_at_ms = now_ms;
        }
        self.count = self.count.saturating_add(1);
        self.count
    }
}

#[derive(Debug)]
struct PeerMessagePolicyEntry {
    messages: DiscreteTokenBucket,
    bytes: ByteTokenBucket,
    rate_limit_violations: ViolationWindow,
    invalid_messages: ViolationWindow,
    banned_until_ms: Option<u64>,
    last_used_ms: u64,
}

impl PeerMessagePolicyEntry {
    fn new(now_ms: u64, config: &PeerMessagePolicyConfig) -> Self {
        Self {
            messages: DiscreteTokenBucket::new(
                config.peer_message_interval_ms,
                config.peer_message_burst,
            ),
            bytes: ByteTokenBucket::new(ByteRateLimitConfig {
                rate_bytes_per_sec: config.peer_message_rate_bytes_per_sec,
                burst_bytes: config.peer_message_burst_bytes,
            }),
            rate_limit_violations: ViolationWindow::new(now_ms),
            invalid_messages: ViolationWindow::new(now_ms),
            banned_until_ms: None,
            last_used_ms: now_ms,
        }
    }

    fn is_banned(&self, now_ms: u64) -> bool {
        self.banned_until_ms
            .is_some_and(|banned_until_ms| now_ms < banned_until_ms)
    }

    fn ban(&mut self, now_ms: u64, ban_duration_ms: u64) {
        self.banned_until_ms = Some(now_ms.saturating_add(ban_duration_ms));
        self.last_used_ms = now_ms;
    }

    fn expire_violation_windows(&mut self, now_ms: u64, window_duration_ms: u64) {
        self.rate_limit_violations
            .expire(now_ms, window_duration_ms);
        self.invalid_messages.expire(now_ms, window_duration_ms);
    }
}

#[derive(Debug)]
struct FiberIngressBudget {
    max_messages: u32,
    max_bytes: u64,
    in_flight_messages: u32,
    in_flight_bytes: u64,
}

impl FiberIngressBudget {
    fn new(max_messages: u32, max_bytes: u64) -> Self {
        Self {
            max_messages,
            max_bytes,
            in_flight_messages: 0,
            in_flight_bytes: 0,
        }
    }

    fn is_unlimited(&self) -> bool {
        self.max_messages == 0 || self.max_bytes == 0
    }

    fn try_reserve(&mut self, bytes: u64) -> bool {
        if !self.is_unlimited()
            && (self.in_flight_messages >= self.max_messages
                || bytes > self.max_bytes.saturating_sub(self.in_flight_bytes))
        {
            return false;
        }
        self.in_flight_messages += 1;
        self.in_flight_bytes += bytes;
        true
    }

    fn release(&mut self, bytes: u64) {
        debug_assert!(self.in_flight_messages > 0);
        debug_assert!(self.in_flight_bytes >= bytes);
        self.in_flight_messages = self.in_flight_messages.saturating_sub(1);
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(bytes);
    }
}

#[derive(Debug)]
pub(crate) struct PeerMessagePolicy {
    peers: HashMap<Pubkey, PeerMessagePolicyEntry>,
    ordinary_lru: BTreeSet<(u64, Pubkey)>,
    banned_expirations: BTreeSet<(u64, Pubkey)>,
    max_entries: usize,
    ingress: FiberIngressBudget,
    config: PeerMessagePolicyConfig,
}

impl PeerMessagePolicy {
    pub(crate) fn new(config: PeerMessagePolicyConfig) -> Self {
        Self {
            peers: HashMap::new(),
            ordinary_lru: BTreeSet::new(),
            banned_expirations: BTreeSet::new(),
            max_entries: config.max_tracked_peers.max(1),
            ingress: FiberIngressBudget::new(
                config.max_in_flight_messages,
                config.max_in_flight_bytes,
            ),
            config,
        }
    }

    #[cfg(test)]
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

    fn insert_entry(&mut self, peer: Pubkey, entry: PeerMessagePolicyEntry) {
        if let Some(banned_until_ms) = entry.banned_until_ms {
            self.banned_expirations.insert((banned_until_ms, peer));
        } else {
            self.ordinary_lru.insert((entry.last_used_ms, peer));
        }
        self.peers.insert(peer, entry);
    }

    fn take_entry(&mut self, peer: &Pubkey) -> Option<PeerMessagePolicyEntry> {
        let entry = self.peers.remove(peer)?;
        if let Some(banned_until_ms) = entry.banned_until_ms {
            self.banned_expirations.remove(&(banned_until_ms, *peer));
        } else {
            self.ordinary_lru.remove(&(entry.last_used_ms, *peer));
        }
        Some(entry)
    }

    fn prune_expired_bans(&mut self, now_ms: u64) {
        while let Some(&(banned_until_ms, peer)) = self.banned_expirations.first() {
            if now_ms < banned_until_ms {
                break;
            }
            self.banned_expirations.pop_first();
            self.peers.remove(&peer);
        }
    }

    fn make_room(&mut self, peer: &Pubkey, now_ms: u64) {
        if self.peers.contains_key(peer) || self.peers.len() < self.max_entries {
            return;
        }
        self.prune_expired_bans(now_ms);
        if self.peers.len() < self.max_entries {
            return;
        }

        let evicted_peer = self
            .ordinary_lru
            .pop_first()
            .or_else(|| self.banned_expirations.pop_first())
            .map(|(_, peer)| peer);
        if let Some(evicted_peer) = evicted_peer {
            self.peers.remove(&evicted_peer);
            debug!(
                evicted_peer = format!("{evicted_peer:?}"),
                max_entries = self.max_entries,
                "Evicting peer message policy entry due to capacity limit"
            );
        }
    }

    fn entry_for_update(&mut self, peer: &Pubkey, now_ms: u64) -> PeerMessagePolicyEntry {
        self.make_room(peer, now_ms);
        self.take_entry(peer)
            .unwrap_or_else(|| PeerMessagePolicyEntry::new(now_ms, &self.config))
    }

    pub(crate) fn admit(&mut self, peer: &Pubkey, bytes: u64, now_ms: u64) -> PeerMessageAdmission {
        if self.is_banned(peer, now_ms) {
            return PeerMessageAdmission::Ban;
        }

        let mut entry = self.entry_for_update(peer, now_ms);
        entry.expire_violation_windows(now_ms, self.config.ban_duration_ms);
        entry.last_used_ms = now_ms;
        let mut message_bucket = entry.messages.clone();
        let mut byte_bucket = entry.bytes.clone();
        if (self.config.message_limit_disabled() || message_bucket.try_consume(now_ms))
            && byte_bucket.try_consume(bytes, now_ms)
        {
            if !self.ingress.try_reserve(bytes) {
                self.insert_entry(*peer, entry);
                return PeerMessageAdmission::Disconnect;
            }
            entry.messages = message_bucket;
            entry.bytes = byte_bucket;
            self.insert_entry(*peer, entry);
            return PeerMessageAdmission::Allow;
        }

        let violations = entry
            .rate_limit_violations
            .record(now_ms, self.config.ban_duration_ms);
        let decision =
            if !self.config.ban_disabled() && violations >= self.config.violation_ban_threshold {
                entry.ban(now_ms, self.config.ban_duration_ms);
                PeerMessageAdmission::Ban
            } else {
                PeerMessageAdmission::Disconnect
            };
        self.insert_entry(*peer, entry);
        decision
    }

    pub(crate) fn record_invalid(&mut self, peer: &Pubkey, now_ms: u64) -> bool {
        if self.is_banned(peer, now_ms) {
            return true;
        }

        let mut entry = self.entry_for_update(peer, now_ms);
        entry.expire_violation_windows(now_ms, self.config.ban_duration_ms);
        entry.last_used_ms = now_ms;
        let violations = entry
            .invalid_messages
            .record(now_ms, self.config.ban_duration_ms);
        let banned =
            if !self.config.ban_disabled() && violations >= self.config.violation_ban_threshold {
                entry.ban(now_ms, self.config.ban_duration_ms);
                true
            } else {
                false
            };
        self.insert_entry(*peer, entry);
        banned
    }

    pub(crate) fn is_banned(&mut self, peer: &Pubkey, now_ms: u64) -> bool {
        let Some(entry) = self.take_entry(peer) else {
            return false;
        };
        if entry.banned_until_ms.is_some() && !entry.is_banned(now_ms) {
            return false;
        }
        let banned = entry.is_banned(now_ms);
        self.insert_entry(*peer, entry);
        banned
    }

    pub(crate) fn on_disconnected(&mut self, peer: &Pubkey, now_ms: u64) {
        if self.is_banned(peer, now_ms) {
            return;
        }
        let Some(mut entry) = self.take_entry(peer) else {
            return;
        };
        entry.expire_violation_windows(now_ms, self.config.ban_duration_ms);
        // Preserve depleted buckets across clean reconnects. Retained entries remain bounded by
        // max_entries and are evicted through ordinary_lru when capacity is needed.
        self.insert_entry(*peer, entry);
    }

    pub(crate) fn release_ingress(&mut self, bytes: u64) {
        self.ingress.release(bytes);
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> (u32, u64) {
        (
            self.ingress.in_flight_messages,
            self.ingress.in_flight_bytes,
        )
    }
}

impl Default for PeerMessagePolicy {
    fn default() -> Self {
        Self::new(PeerMessagePolicyConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use fiber_types::Privkey;

    use crate::fiber::{
        network::CHANNEL_REESTABLISH_INTERVAL,
        types::{FiberMessage, ReestablishChannel},
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
        let frame_bytes = FiberMessage::reestablish_channel(ReestablishChannel {
            channel_id: Default::default(),
            local_commitment_number: 0,
            remote_commitment_number: 0,
        })
        .to_molecule_bytes()
        .len() as u64;
        assert_eq!(frame_bytes, 68);
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
}
