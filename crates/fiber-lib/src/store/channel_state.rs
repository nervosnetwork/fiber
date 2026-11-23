//! Channel state persistence with an optional read cache in front of it.
//!
//! `Store` owns the durable records; `ChannelStates` is the one place a single channel record is
//! read, written and cached, so `ChannelActorStateStore` methods only pick which operation to call.
//! Bulk reads (`get_all_channel_states`, index scans) decode the prefix themselves and do not use
//! the cache.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ckb_types::prelude::Entity;
use fiber_store::backend::{BatchWriter, StorageBackend};
use fiber_types::schema::{
    CHANNEL_ACTOR_STATE_PREFIX, CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX,
    CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX, PUBKEY_CHANNEL_ID_PREFIX,
};
use fiber_types::{ChannelState, Hash256};

use crate::fiber::channel::ChannelActorState;

use super::store_impl::{KeyValue, Store, StoreKeyValue};

/// The channel records of one store.
pub(crate) struct ChannelStates<'a> {
    store: &'a Store,
    cache: Option<&'a Arc<ChannelStateCache>>,
}

impl<'a> ChannelStates<'a> {
    pub(super) fn new(store: &'a Store, cache: Option<&'a Arc<ChannelStateCache>>) -> Self {
        Self { store, cache }
    }

    /// Decode a channel state from the stored record, bypassing the cache.
    fn load(&self, id: &Hash256) -> Option<ChannelActorState> {
        let key = [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat();
        self.store
            .get(key)
            .map(|v| deserialize_channel_state(v.as_ref()))
    }

    /// Write the channel record without touching the cache.
    fn write(&self, state: ChannelActorState) {
        let mut batch = self.store.batch();

        let kv = KeyValue::PubkeyChannelId((state.get_remote_pubkey(), state.id), state.state);
        batch.put(kv.key(), kv.value());
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            let kv = KeyValue::OutPointChannelId(outpoint, state.id);
            batch.put(kv.key(), kv.value());
        }
        let kv = KeyValue::ChannelActorState(state.id, state);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    /// Delete the channel record and its indexes without touching the cache.
    fn remove(&self, id: &Hash256, existing: Option<&ChannelActorState>) {
        let Some(existing) = existing else {
            return;
        };
        let mut batch = self.store.batch();
        batch.delete([&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat());
        let remote_pubkey_bytes = existing.get_remote_pubkey().serialize();
        batch.delete(
            [
                &[PUBKEY_CHANNEL_ID_PREFIX][..],
                &remote_pubkey_bytes[..],
                id.as_ref(),
            ]
            .concat(),
        );
        if let Some(outpoint) = existing.get_funding_transaction_outpoint() {
            batch.delete([&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat());
        }
        batch.delete([&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX], id.as_ref()].concat());
        batch.commit();
    }

    pub(super) fn get(&self, id: &Hash256) -> Option<ChannelActorState> {
        // Only writes fill the cache: a reader that decoded a snapshot could otherwise publish it
        // after a concurrent write invalidated the entry, resurrecting a state that no longer
        // matches the record.
        self.cache
            .and_then(|cache| cache.peek(id))
            .map(|hit| (*hit).clone())
            .or_else(|| self.load(id))
    }

    pub(super) fn insert(&self, state: ChannelActorState) {
        let Some(cache) = self.cache else {
            return self.write(state);
        };
        // Only a ready channel is cached: its id is final (a channel id changes once, during the
        // opening handshake) and its record is what the message path reads back.
        let cacheable = state.state == ChannelState::ChannelReady;
        // The commit and the cache update happen under the channel's slot, so a write of a ready
        // state and a transition out of it cannot disagree about which one is current: whichever
        // commits last also decides the snapshot.
        let id = state.id;
        {
            let entry = cache.slot(id);
            let mut slot = entry.write();
            if cacheable {
                // Clone the snapshot only when it can be published: a full cache drops it, and the
                // clone would be paid for nothing on every write of an uncached channel.
                let cached = cache
                    .has_room(&slot)
                    .then(|| channel_cache_snapshot(&state));
                self.write(state);
                if let Some(cached) = cached {
                    cache.install(&mut slot, cached);
                }
            } else {
                cache.unpublish(&mut slot);
                self.write(state);
            }
        }
        cache.discard_if_empty(&id);
    }

    /// Move a channel to a new id.
    ///
    /// Only the opening handshake does this, when the id derived from the funding keys replaces
    /// the temporary one, so the channel is never ready here and no snapshot is published for
    /// either id. Both slots are taken in id order, so two concurrent moves cannot deadlock, and
    /// the rewrite happens under them so no concurrent write can interleave with it. The old
    /// record and its indexes go away in the same batch as the new ones, so an interrupted move
    /// leaves the channel untouched.
    pub(super) fn move_to(&self, old_id: &Hash256, state: ChannelActorState) {
        if old_id == &state.id {
            self.insert(state);
            return;
        }
        let Some(cache) = self.cache else {
            let previous = self.load(old_id);
            return self.rewrite_records(old_id, previous.as_ref(), state);
        };
        let (first_id, second_id) = if old_id.as_ref() < state.id.as_ref() {
            (*old_id, state.id)
        } else {
            (state.id, *old_id)
        };
        let first = cache.slot(first_id);
        let second = cache.slot(second_id);
        {
            let mut first_slot = first.write();
            let mut second_slot = second.write();
            // Read under the slots, like `delete` does, so the indexes we delete are the ones the
            // record we replace actually has.
            let previous = self.load(old_id);
            cache.unpublish(&mut first_slot);
            cache.unpublish(&mut second_slot);
            self.rewrite_records(old_id, previous.as_ref(), state);
        }
        drop((first, second));
        cache.discard_if_empty(&first_id);
        cache.discard_if_empty(&second_id);
    }

    /// Rewrite the channel records under a new id, in one batch.
    fn rewrite_records(
        &self,
        old_id: &Hash256,
        old_state: Option<&ChannelActorState>,
        state: ChannelActorState,
    ) {
        let mut batch = self.store.batch();
        if let Some(old_state) = old_state {
            batch.delete([&[CHANNEL_ACTOR_STATE_PREFIX], old_id.as_ref()].concat());
            let remote_pubkey_bytes = old_state.get_remote_pubkey().serialize();
            batch.delete(
                [
                    &[PUBKEY_CHANNEL_ID_PREFIX][..],
                    &remote_pubkey_bytes[..],
                    old_id.as_ref(),
                ]
                .concat(),
            );
            if let Some(outpoint) = old_state.get_funding_transaction_outpoint() {
                batch.delete([&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat());
            }
        }
        let kv = KeyValue::PubkeyChannelId((state.get_remote_pubkey(), state.id), state.state);
        batch.put(kv.key(), kv.value());
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            let kv = KeyValue::OutPointChannelId(outpoint, state.id);
            batch.put(kv.key(), kv.value());
        }
        let kv = KeyValue::ChannelActorState(state.id, state);
        batch.put(kv.key(), kv.value());
        batch.commit();
    }

    pub(super) fn delete(&self, id: &Hash256) {
        let Some(cache) = self.cache else {
            let existing = self.load(id);
            return self.remove(id, existing.as_ref());
        };
        {
            let entry = cache.slot(*id);
            let mut slot = entry.write();
            // The snapshot carries the indexes to delete; cloning the state itself just for them
            // would copy it for nothing.
            let cached = slot.clone();
            cache.unpublish(&mut slot);
            let existing = cached.or_else(|| self.load(id).map(Arc::new));
            // Under the slot, so a concurrent write cannot make a snapshot current again after
            // the record is gone.
            self.remove(id, existing.as_deref());
        }
        cache.discard_if_empty(id);
    }

    #[cfg(test)]
    pub(super) fn cache_len(&self) -> usize {
        self.cache.map_or(0, |cache| cache.entries.len())
    }

    /// The cached snapshot together with the stored record, both read while the slot's read lock
    /// is held, so a concurrent write cannot change the record between the two reads (tests only).
    #[cfg(test)]
    pub(super) fn cache_and_record(
        &self,
        id: &Hash256,
    ) -> Option<(ChannelActorState, Option<ChannelActorState>)> {
        let cache = self.cache?;
        let entry = cache.entries.get(id)?;
        let slot = entry.try_read()?;
        let cached = (**slot.as_ref()?).clone();
        let record = self.load(id);
        Some((cached, record))
    }

    /// The cached snapshot only, without falling back to the record (tests only).
    #[cfg(test)]
    pub(super) fn cache_peek(&self, id: &Hash256) -> Option<ChannelActorState> {
        self.cache
            .and_then(|cache| cache.peek(id))
            .map(|hit| (*hit).clone())
    }
}

pub(super) fn deserialize_channel_state(bytes: &[u8]) -> ChannelActorState {
    fiber_types::channel_v2_validation::decode_channel_actor_data(bytes)
        .unwrap_or_else(|e| panic!("deserialization of ChannelActorState failed: {e}"))
        .into()
}

/// Bounded read cache of channel state snapshots with per-channel locking.
///
/// Every persisted version is immutable, so the cache only ever replaces a snapshot, and only the
/// write path publishes: holding a channel's slot across "commit + publish" (or "remove +
/// unpublish") is what keeps the snapshot equal to the last committed record.
///
/// The cache never evicts. It publishes at most `max_size` snapshots; past that, writes still go
/// through the slot (so they stay serialized with every other write of that channel) but the
/// snapshot is dropped. Deleting a channel frees its capacity again.
pub(super) struct ChannelStateCache {
    max_size: usize,
    /// Number of published snapshots, taken atomically so concurrent writers cannot exceed
    /// `max_size`. Counting snapshots rather than slots is what lets every write take a slot
    /// without the map growing with channels that are not cached.
    snapshots: AtomicUsize,
    entries: dashmap::DashMap<Hash256, ChannelCacheEntry>,
}

type ChannelCacheEntry = Arc<parking_lot::RwLock<Option<Arc<ChannelActorState>>>>;

impl ChannelStateCache {
    fn new(max_size: usize) -> Self {
        Self {
            max_size,
            snapshots: AtomicUsize::new(0),
            entries: Default::default(),
        }
    }

    /// The cached snapshot, cloned while the slot is briefly read-locked. A slot a writer is
    /// holding is treated as a miss, so a read never waits for a commit, and two readers can read
    /// at the same time.
    fn peek(&self, id: &Hash256) -> Option<Arc<ChannelActorState>> {
        let entry = self.entries.get(id)?;
        let slot = entry.try_read()?;
        slot.as_ref().map(Arc::clone)
    }

    /// The channel's slot, created on first use.
    ///
    /// Every write takes it, even when the cache is full or the state is not cacheable, so all
    /// writes of one channel are serialized by the same lock. The returned handle keeps the slot
    /// alive for as long as the writer uses it; an empty slot is dropped from the map afterwards
    /// (see `discard_if_empty`), so slots never outlive the write that created them.
    fn slot(&self, id: Hash256) -> ChannelCacheEntry {
        match self.entries.get(&id) {
            Some(entry) => Arc::clone(&entry),
            None => self
                .entries
                .entry(id)
                .or_insert_with(|| Arc::new(parking_lot::RwLock::new(None)))
                .clone(),
        }
    }

    /// Whether a snapshot would be published right now: the channel is cached already, or there is
    /// capacity left. A hint only — a concurrent writer can take the last slot — so `install` still
    /// decides atomically.
    fn has_room(&self, slot: &Option<Arc<ChannelActorState>>) -> bool {
        slot.is_some() || self.snapshots.load(Ordering::Relaxed) < self.max_size
    }

    /// Publish a snapshot under the caller's slot lock, unless the cache filled up meanwhile. A
    /// channel that is already cached keeps being updated (its capacity is already taken).
    fn install(&self, slot: &mut Option<Arc<ChannelActorState>>, cached: Arc<ChannelActorState>) {
        if slot.is_some() || self.reserve() {
            *slot = Some(cached);
        }
    }

    /// Drop the snapshot under the caller's slot lock, freeing its capacity.
    fn unpublish(&self, slot: &mut Option<Arc<ChannelActorState>>) {
        if slot.take().is_some() {
            self.snapshots.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Take one capacity slot, atomically, so concurrent writers cannot exceed `max_size`.
    fn reserve(&self) -> bool {
        self.snapshots
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |snapshots| {
                (snapshots < self.max_size).then_some(snapshots + 1)
            })
            .is_ok()
    }

    /// Remove an empty slot nobody holds, freeing the map. The predicate runs under the shard
    /// lock, so a writer cannot take the slot between the check and the removal: unlinking a live
    /// slot would let a later writer create a second lock for the same channel.
    ///
    /// A concurrent reader can keep an empty slot alive for one more round; it then stays until
    /// the next write or delete of that channel, which costs a map entry, never correctness.
    fn discard_if_empty(&self, id: &Hash256) {
        self.entries.remove_if(id, |_, entry| {
            // A slot we cannot read-lock is kept: only a reader can hold it here, and removing
            // what someone is reading is the direction this check exists to avoid.
            Arc::strong_count(entry) == 1 && entry.try_read().is_some_and(|slot| slot.is_none())
        });
    }

    /// Drop every snapshot and reset the capacity counter.
    ///
    /// Not serialized with writers: a writer that already holds a slot would afterwards publish
    /// into an unlinked slot (dropping its capacity back into a counter that is already zero) and
    /// the next writer for that channel would get a second lock. Both callers are quiescent — a
    /// database restore and the test harness before a node starts — so keep it that way.
    pub(super) fn clear(&self) {
        self.entries.clear();
        self.snapshots.store(0, Ordering::Relaxed);
    }
}

pub(super) fn new_channel_cache(capacity: usize) -> Option<Arc<ChannelStateCache>> {
    (capacity > 0).then(|| Arc::new(ChannelStateCache::new(capacity)))
}

/// Snapshot to cache: exactly what decoding the stored record produces, so cached reads cannot
/// leak runtime-only fields that were never persisted.
fn channel_cache_snapshot(state: &ChannelActorState) -> Arc<ChannelActorState> {
    Arc::new(ChannelActorState::from(state.core.clone()))
}
