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
}

impl Default for PeerMessagePolicy {
    fn default() -> Self {
        Self::new(PeerMessagePolicyConfig::default())
    }
}

#[cfg(test)]
pub(crate) mod tests;
