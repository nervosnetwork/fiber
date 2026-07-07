#![allow(dead_code)]

use ckb_types::packed::OutPoint;
use serde::{Deserialize, Deserializer, Serialize};

use std::collections::HashMap;
use tracing::warn;

use crate::fiber::Pubkey;

pub(crate) const DEFAULT_GOSSIP_BAN_THRESHOLD: u32 = 100;
pub(crate) const DEFAULT_GOSSIP_BAN_DURATION_MS: u64 = 10 * 60 * 1000;
pub(crate) const DEFAULT_GOSSIP_BAN_MAX_TRACKED_PEERS: usize = 50_000;
pub(crate) const DEFAULT_GOSSIP_GLOBAL_MSG_RATE_BYTES: u64 = 1_024_000;
pub(crate) const DEFAULT_GOSSIP_GLOBAL_MSG_BURST_BYTES: u64 = 2_048_000;
pub(crate) const DEFAULT_GOSSIP_PEER_MSG_RATE_BYTES: u64 = 51_200;
pub(crate) const DEFAULT_GOSSIP_PEER_MSG_BURST_BYTES: u64 = 102_400;
pub(crate) const DEFAULT_GOSSIP_OUTBOUND_DELAY_QUEUE_CAPACITY: usize = 1_024;
pub(crate) const DEFAULT_GOSSIP_CHANNEL_UPDATE_INTERVAL_MS: u64 = 60_000;
pub(crate) const DEFAULT_GOSSIP_CHANNEL_UPDATE_BURST: u32 = 10;
pub(crate) const DEFAULT_GOSSIP_CHANNEL_UPDATE_MAX_TRACKED_KEYS: usize = 50_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct GossipPolicyConfig {
    pub ban: GossipBanConfig,
    pub outbound_global: ByteRateLimitConfig,
    pub outbound_peer: ByteRateLimitConfig,
    // Maximum number of delayed outbound gossip payloads queued per peer.
    pub outbound_delay_queue_capacity: usize,
    pub inbound_channel_update: ChannelUpdateRateLimitConfig,
}

impl Default for GossipPolicyConfig {
    fn default() -> Self {
        Self {
            ban: GossipBanConfig::default(),
            outbound_global: default_global_outbound_config(),
            outbound_peer: default_peer_outbound_config(),
            outbound_delay_queue_capacity: DEFAULT_GOSSIP_OUTBOUND_DELAY_QUEUE_CAPACITY,
            inbound_channel_update: ChannelUpdateRateLimitConfig {
                interval_ms: DEFAULT_GOSSIP_CHANNEL_UPDATE_INTERVAL_MS,
                burst: DEFAULT_GOSSIP_CHANNEL_UPDATE_BURST,
            },
        }
    }
}

impl<'de> Deserialize<'de> for GossipPolicyConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct PartialGossipPolicyConfig {
            ban: PartialGossipBanConfig,
            outbound_global: PartialByteRateLimitConfig,
            outbound_peer: PartialByteRateLimitConfig,
            outbound_delay_queue_capacity: Option<usize>,
            inbound_channel_update: PartialChannelUpdateRateLimitConfig,
        }

        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct PartialGossipBanConfig {
            threshold: Option<u32>,
            duration_ms: Option<u64>,
        }

        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct PartialByteRateLimitConfig {
            rate_bytes_per_sec: Option<u64>,
            burst_bytes: Option<u64>,
        }

        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct PartialChannelUpdateRateLimitConfig {
            interval_ms: Option<u64>,
            burst: Option<u32>,
        }

        let partial = PartialGossipPolicyConfig::deserialize(deserializer)?;
        let defaults = Self::default();

        Ok(Self {
            ban: GossipBanConfig {
                threshold: partial.ban.threshold.unwrap_or(defaults.ban.threshold),
                duration_ms: partial.ban.duration_ms.unwrap_or(defaults.ban.duration_ms),
            },
            outbound_global: ByteRateLimitConfig {
                rate_bytes_per_sec: partial
                    .outbound_global
                    .rate_bytes_per_sec
                    .unwrap_or(defaults.outbound_global.rate_bytes_per_sec),
                burst_bytes: partial
                    .outbound_global
                    .burst_bytes
                    .unwrap_or(defaults.outbound_global.burst_bytes),
            },
            outbound_peer: ByteRateLimitConfig {
                rate_bytes_per_sec: partial
                    .outbound_peer
                    .rate_bytes_per_sec
                    .unwrap_or(defaults.outbound_peer.rate_bytes_per_sec),
                burst_bytes: partial
                    .outbound_peer
                    .burst_bytes
                    .unwrap_or(defaults.outbound_peer.burst_bytes),
            },
            outbound_delay_queue_capacity: partial
                .outbound_delay_queue_capacity
                .unwrap_or(defaults.outbound_delay_queue_capacity),
            inbound_channel_update: ChannelUpdateRateLimitConfig {
                interval_ms: partial
                    .inbound_channel_update
                    .interval_ms
                    .unwrap_or(defaults.inbound_channel_update.interval_ms),
                burst: partial
                    .inbound_channel_update
                    .burst
                    .unwrap_or(defaults.inbound_channel_update.burst),
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct GossipBanConfig {
    pub threshold: u32,
    pub duration_ms: u64,
}

impl Default for GossipBanConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_GOSSIP_BAN_THRESHOLD,
            duration_ms: DEFAULT_GOSSIP_BAN_DURATION_MS,
        }
    }
}

impl GossipBanConfig {
    pub(crate) fn build_tracker(&self) -> GossipBanTracker {
        GossipBanTracker::new(self.clone())
    }

    pub(crate) fn build_tracker_with_max_entries(&self, max_entries: usize) -> GossipBanTracker {
        GossipBanTracker::with_max_entries(self.clone(), max_entries)
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.threshold == 0 || self.duration_ms == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ByteRateLimitConfig {
    pub rate_bytes_per_sec: u64,
    pub burst_bytes: u64,
}

impl ByteRateLimitConfig {
    pub(crate) fn is_disabled(&self) -> bool {
        self.rate_bytes_per_sec == 0 || self.burst_bytes == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ChannelUpdateRateLimitConfig {
    pub interval_ms: u64,
    pub burst: u32,
}

impl Default for ChannelUpdateRateLimitConfig {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_GOSSIP_CHANNEL_UPDATE_INTERVAL_MS,
            burst: DEFAULT_GOSSIP_CHANNEL_UPDATE_BURST,
        }
    }
}

impl ChannelUpdateRateLimitConfig {
    pub(crate) fn build_limiter(&self) -> ChannelUpdateLimiter {
        ChannelUpdateLimiter::new(self.clone())
    }

    #[cfg(test)]
    pub(crate) fn build_limiter_with_max_entries(
        &self,
        max_entries: usize,
    ) -> ChannelUpdateLimiter {
        ChannelUpdateLimiter::with_max_entries(self.clone(), max_entries)
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.interval_ms == 0 || self.burst == 0
    }
}

fn default_global_outbound_config() -> ByteRateLimitConfig {
    ByteRateLimitConfig {
        rate_bytes_per_sec: DEFAULT_GOSSIP_GLOBAL_MSG_RATE_BYTES,
        burst_bytes: DEFAULT_GOSSIP_GLOBAL_MSG_BURST_BYTES,
    }
}

fn default_peer_outbound_config() -> ByteRateLimitConfig {
    ByteRateLimitConfig {
        rate_bytes_per_sec: DEFAULT_GOSSIP_PEER_MSG_RATE_BYTES,
        burst_bytes: DEFAULT_GOSSIP_PEER_MSG_BURST_BYTES,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum GossipViolation {
    InvalidSyncResponse,
    InvalidBroadcastMessage,
    PolicyRejectedMessage,
}

impl GossipViolation {
    fn score(self) -> u32 {
        match self {
            GossipViolation::InvalidSyncResponse => 100,
            GossipViolation::InvalidBroadcastMessage => 100,
            GossipViolation::PolicyRejectedMessage => 25,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct GossipBanEntry {
    score: u32,
    banned_until_ms: Option<u64>,
    last_updated_ms: u64,
}

impl GossipBanEntry {
    fn expires_at_ms(&self, duration_ms: u64) -> u64 {
        self.last_updated_ms.saturating_add(duration_ms)
    }

    fn is_expired(&self, duration_ms: u64, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms(duration_ms)
    }

    fn is_currently_banned(&self, now_ms: u64) -> bool {
        self.banned_until_ms
            .is_some_and(|until_ms| now_ms < until_ms)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GossipBanTracker {
    config: GossipBanConfig,
    max_entries: usize,
    peers: HashMap<Pubkey, GossipBanEntry>,
}

impl GossipBanTracker {
    pub(crate) fn new(config: GossipBanConfig) -> Self {
        Self::with_max_entries(config, DEFAULT_GOSSIP_BAN_MAX_TRACKED_PEERS)
    }

    pub(crate) fn with_max_entries(config: GossipBanConfig, max_entries: usize) -> Self {
        Self {
            config,
            max_entries: max_entries.max(1),
            peers: HashMap::new(),
        }
    }

    fn maybe_expire(&mut self, peer: &Pubkey, now_ms: u64) {
        let is_expired = self
            .peers
            .get(peer)
            .is_some_and(|entry| entry.is_expired(self.config.duration_ms, now_ms));
        if is_expired {
            self.peers.remove(peer);
        }
    }

    pub(crate) fn prune_expired(&mut self, now_ms: u64) {
        let duration_ms = self.config.duration_ms;
        self.peers
            .retain(|_, entry| !entry.is_expired(duration_ms, now_ms));
    }

    fn maybe_make_room_for_new_entry(&mut self, peer: &Pubkey, now_ms: u64) {
        if self.peers.contains_key(peer) || self.peers.len() < self.max_entries {
            return;
        }

        self.prune_expired(now_ms);
        while !self.peers.contains_key(peer) && self.peers.len() >= self.max_entries {
            let Some(evicted_peer) = self
                .peers
                .iter()
                .min_by_key(|(_, entry)| {
                    (
                        entry.is_currently_banned(now_ms),
                        entry.score,
                        entry.last_updated_ms,
                    )
                })
                .map(|(peer, _)| *peer)
            else {
                break;
            };

            self.peers.remove(&evicted_peer);
            warn!(
                evicted_peer = format!("{evicted_peer:?}"),
                max_entries = self.max_entries,
                "Evicting gossip ban entry due to capacity limit"
            );
        }
    }

    pub(crate) fn record_violation(
        &mut self,
        peer: &Pubkey,
        violation: GossipViolation,
        now_ms: u64,
    ) -> bool {
        if self.config.is_disabled() {
            return false;
        }

        self.maybe_expire(peer, now_ms);
        self.maybe_make_room_for_new_entry(peer, now_ms);

        let entry = self.peers.entry(*peer).or_default();
        entry.score = entry.score.saturating_add(violation.score());
        entry.last_updated_ms = now_ms;
        if entry.score >= self.config.threshold {
            entry.banned_until_ms = Some(now_ms.saturating_add(self.config.duration_ms));
            return true;
        }
        false
    }

    pub(crate) fn is_banned(&mut self, peer: &Pubkey, now_ms: u64) -> bool {
        if self.config.is_disabled() {
            return false;
        }
        self.maybe_expire(peer, now_ms);
        self.peers
            .get(peer)
            .and_then(|entry| entry.banned_until_ms)
            .is_some_and(|until_ms| now_ms < until_ms)
    }

    pub(crate) fn score(&mut self, peer: &Pubkey, now_ms: u64) -> u32 {
        self.maybe_expire(peer, now_ms);
        self.peers.get(peer).map(|entry| entry.score).unwrap_or(0)
    }

    pub(crate) fn tracked_peers(&self) -> usize {
        self.peers.len()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ByteTokenBucket {
    config: ByteRateLimitConfig,
    available_bytes: i128,
    last_refill_ms: u64,
}

impl ByteTokenBucket {
    pub(crate) fn new(config: ByteRateLimitConfig) -> Self {
        Self {
            available_bytes: i128::from(config.burst_bytes),
            config,
            last_refill_ms: 0,
        }
    }

    fn refill(&mut self, now_ms: u64) {
        if self.config.is_disabled() {
            return;
        }
        if now_ms <= self.last_refill_ms {
            return;
        }

        let elapsed_ms = now_ms - self.last_refill_ms;
        let refill_bytes =
            (u128::from(elapsed_ms) * u128::from(self.config.rate_bytes_per_sec) / 1_000) as i128;
        self.available_bytes = self
            .available_bytes
            .saturating_add(refill_bytes)
            .min(i128::from(self.config.burst_bytes));
        self.last_refill_ms = now_ms;
    }

    pub(crate) fn try_consume(&mut self, bytes: u64, now_ms: u64) -> bool {
        if self.config.is_disabled() {
            return true;
        }

        self.refill(now_ms);
        let bytes = i128::from(bytes);
        if bytes > self.available_bytes {
            return false;
        }
        self.available_bytes -= bytes;
        true
    }

    pub(crate) fn reserve(&mut self, bytes: u64, now_ms: u64) -> u64 {
        if self.config.is_disabled() {
            return 0;
        }

        self.refill(now_ms);
        let bytes = i128::from(bytes);
        let deficit_bytes = (bytes.saturating_sub(self.available_bytes)).max(0) as u128;
        let delay_ms = if deficit_bytes == 0 {
            0
        } else {
            deficit_bytes
                .saturating_mul(1_000)
                .div_ceil(u128::from(self.config.rate_bytes_per_sec)) as u64
        };
        self.available_bytes = self.available_bytes.saturating_sub(bytes);
        delay_ms
    }

    pub(crate) fn refund(&mut self, bytes: u64, now_ms: u64) {
        if self.config.is_disabled() {
            return;
        }

        self.refill(now_ms);
        self.available_bytes = self
            .available_bytes
            .saturating_add(i128::from(bytes))
            .min(i128::from(self.config.burst_bytes));
    }

    pub(crate) fn has_debt(&self, now_ms: u64) -> bool {
        if self.config.is_disabled() {
            return false;
        }

        let mut cloned = self.clone();
        cloned.refill(now_ms);
        cloned.available_bytes < 0
    }

    pub(crate) fn burst_bytes(&self) -> u64 {
        self.config.burst_bytes
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.config.is_disabled()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscreteTokenBucket {
    interval_ms: u64,
    capacity: u32,
    available: u32,
    last_refill_ms: u64,
}

impl DiscreteTokenBucket {
    fn new(interval_ms: u64, capacity: u32) -> Self {
        Self {
            interval_ms,
            capacity,
            available: capacity,
            last_refill_ms: 0,
        }
    }

    fn refill(&mut self, now_ms: u64) {
        if self.interval_ms == 0 || self.capacity == 0 || now_ms <= self.last_refill_ms {
            return;
        }

        let elapsed_ms = now_ms - self.last_refill_ms;
        let refill = (elapsed_ms / self.interval_ms) as u32;
        if refill == 0 {
            return;
        }

        self.available = self.available.saturating_add(refill).min(self.capacity);
        self.last_refill_ms = self
            .last_refill_ms
            .saturating_add(u64::from(refill) * self.interval_ms);
    }

    fn try_consume(&mut self, now_ms: u64) -> bool {
        self.refill(now_ms);
        if self.available == 0 {
            return false;
        }
        self.available -= 1;
        true
    }

    fn is_full(&self) -> bool {
        self.available == self.capacity
    }
}

#[derive(Clone, Debug)]
struct ChannelUpdateLimiterEntry {
    bucket: DiscreteTokenBucket,
    last_used_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelUpdateLimiter {
    config: ChannelUpdateRateLimitConfig,
    max_entries: usize,
    buckets: HashMap<ChannelUpdateLimiterKey, ChannelUpdateLimiterEntry>,
}

impl ChannelUpdateLimiter {
    pub(crate) fn new(config: ChannelUpdateRateLimitConfig) -> Self {
        Self::with_max_entries(config, DEFAULT_GOSSIP_CHANNEL_UPDATE_MAX_TRACKED_KEYS)
    }

    pub(crate) fn with_max_entries(
        config: ChannelUpdateRateLimitConfig,
        max_entries: usize,
    ) -> Self {
        Self {
            config,
            max_entries: max_entries.max(1),
            buckets: HashMap::new(),
        }
    }

    pub(crate) fn try_acquire(&mut self, key: &ChannelUpdateLimiterKey, now_ms: u64) -> bool {
        if self.config.is_disabled() {
            return true;
        }

        if let Some(entry) = self.buckets.get_mut(key) {
            entry.last_used_ms = now_ms;
            return entry.bucket.try_consume(now_ms);
        }

        if self.buckets.len() >= self.max_entries {
            self.prune_idle_entries(now_ms);
        }

        if self.buckets.len() >= self.max_entries {
            warn!(
                max_entries = self.max_entries,
                "Rejecting channel update rate-limit key due to capacity limit"
            );
            return false;
        }

        let mut entry = ChannelUpdateLimiterEntry {
            bucket: DiscreteTokenBucket::new(self.config.interval_ms, self.config.burst),
            last_used_ms: now_ms,
        };
        let allowed = entry.bucket.try_consume(now_ms);
        self.buckets.insert(key.clone(), entry);
        allowed
    }

    fn prune_idle_entries(&mut self, now_ms: u64) {
        self.buckets.retain(|_, entry| {
            entry.bucket.refill(now_ms);
            !entry.bucket.is_full()
        });
    }

    #[cfg(test)]
    pub(crate) fn tracked_keys(&self) -> usize {
        self.buckets.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ChannelUpdateLimiterKey {
    pub peer: Pubkey,
    pub channel_outpoint: OutPoint,
    pub is_node1: bool,
}

impl ChannelUpdateLimiterKey {
    pub(crate) fn new(peer: Pubkey, channel_outpoint: OutPoint, is_node1: bool) -> Self {
        Self {
            peer,
            channel_outpoint,
            is_node1,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct GossipPolicyState {
    pub ban_tracker: GossipBanTracker,
    pub global_outbound: ByteTokenBucket,
    pub peer_outbound: HashMap<Pubkey, ByteTokenBucket>,
    outbound_delay_queue_capacity: usize,
    peer_outbound_config: ByteRateLimitConfig,
}

impl GossipPolicyState {
    pub(crate) fn new(config: GossipPolicyConfig) -> Self {
        Self {
            ban_tracker: config.ban.build_tracker(),
            global_outbound: ByteTokenBucket::new(config.outbound_global.clone()),
            peer_outbound: HashMap::new(),
            outbound_delay_queue_capacity: config.outbound_delay_queue_capacity,
            peer_outbound_config: config.outbound_peer,
        }
    }

    pub(crate) fn record_violation(
        &mut self,
        peer: &Pubkey,
        violation: GossipViolation,
        now_ms: u64,
    ) -> bool {
        self.ban_tracker.record_violation(peer, violation, now_ms)
    }

    pub(crate) fn is_banned(&mut self, peer: &Pubkey, now_ms: u64) -> bool {
        self.ban_tracker.is_banned(peer, now_ms)
    }

    pub(crate) fn prune_bans(&mut self, now_ms: u64) {
        self.ban_tracker.prune_expired(now_ms);
    }

    pub(crate) fn allow_outbound_message(
        &mut self,
        peer: &Pubkey,
        bytes: u64,
        now_ms: u64,
    ) -> bool {
        let mut global_bucket = self.global_outbound.clone();
        if !global_bucket.try_consume(bytes, now_ms) {
            return false;
        }

        let mut peer_bucket = self
            .peer_outbound
            .get(peer)
            .cloned()
            .unwrap_or_else(|| ByteTokenBucket::new(self.peer_outbound_config.clone()));
        if !peer_bucket.try_consume(bytes, now_ms) {
            return false;
        }

        self.global_outbound = global_bucket;
        self.peer_outbound.insert(*peer, peer_bucket);
        true
    }

    pub(crate) fn estimate_outbound_message_delay(
        &self,
        peer: &Pubkey,
        bytes: u64,
        now_ms: u64,
    ) -> u64 {
        let mut cloned = self.clone();
        cloned.reserve_outbound_message(peer, bytes, now_ms)
    }

    pub(crate) fn reserve_outbound_message(
        &mut self,
        peer: &Pubkey,
        bytes: u64,
        now_ms: u64,
    ) -> u64 {
        let global_delay_ms = self.global_outbound.reserve(bytes, now_ms);
        let peer_bucket = self
            .peer_outbound
            .entry(*peer)
            .or_insert_with(|| ByteTokenBucket::new(self.peer_outbound_config.clone()));
        let peer_delay_ms = peer_bucket.reserve(bytes, now_ms);
        global_delay_ms.max(peer_delay_ms)
    }

    pub(crate) fn refund_outbound_message(&mut self, peer: &Pubkey, bytes: u64, now_ms: u64) {
        self.global_outbound.refund(bytes, now_ms);
        if self.peer_outbound_config.is_disabled() {
            return;
        }

        self.peer_outbound
            .entry(*peer)
            .or_insert_with(|| ByteTokenBucket::new(self.peer_outbound_config.clone()))
            .refund(bytes, now_ms);
    }

    pub(crate) fn should_bypass_oversized_outbound_message(
        &self,
        peer: &Pubkey,
        bytes: u64,
        now_ms: u64,
    ) -> bool {
        if !self.outbound_message_exceeds_single_message_capacity(bytes) {
            return false;
        }

        if self.global_outbound.has_debt(now_ms) {
            return false;
        }

        self.peer_outbound
            .get(peer)
            .is_none_or(|bucket| !bucket.has_debt(now_ms))
    }

    pub(crate) fn outbound_message_exceeds_single_message_capacity(&self, bytes: u64) -> bool {
        (!self.global_outbound.is_disabled() && bytes > self.global_outbound.burst_bytes())
            || (!self.peer_outbound_config.is_disabled()
                && bytes > self.peer_outbound_config.burst_bytes)
    }

    pub(crate) fn outbound_delay_queue_capacity(&self) -> usize {
        self.outbound_delay_queue_capacity
    }

    pub(crate) fn outbound_global_delayed_byte_capacity(&self) -> Option<u64> {
        if self.global_outbound.is_disabled() {
            None
        } else {
            Some(self.global_outbound.burst_bytes())
        }
    }

    pub(crate) fn outbound_peer_delayed_byte_capacity(&self) -> Option<u64> {
        if self.peer_outbound_config.is_disabled() {
            None
        } else {
            Some(self.peer_outbound_config.burst_bytes)
        }
    }
}

impl Default for GossipPolicyState {
    fn default() -> Self {
        Self::new(GossipPolicyConfig::default())
    }
}
