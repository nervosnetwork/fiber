//! Channel state persistence with an optional read cache in front of it.
//!
//! `Store` owns the durable records; `ChannelStates` is the one place a single channel record is
//! read, written and cached, so `ChannelActorStateStore` methods only pick which operation to call.
//! Bulk reads (`get_all_channel_states`, index scans) decode the prefix themselves and do not use
//! the cache.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ckb_types::prelude::Entity;
use fiber_store::backend::{BatchWriter, StorageBackend};
use fiber_types::schema::{
    channel_payment_hashes_key, payment_hash_channel_id_key, CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX,
    CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX, PUBKEY_CHANNEL_ID_PREFIX,
};
use fiber_types::{ChannelOpeningStatus, ChannelState, Hash256};

use crate::fiber::channel::{ChannelActorState, ChannelOpenRecordStore};

use super::store_impl::{
    channel_immutable_key, channel_mutable_key, deserialize_channel_state_parts, deserialize_from,
    serialize_to_vec, stage_channel_actor_state, stage_move_channel_open_records, Store,
};
use super::NodeNamespace;

/// Striped locks keyed by channel id. A channel state write reads the payment hashes indexed for
/// the channel and commits the new ones with the state, so two writes of one channel must not
/// interleave. The cache slot already serializes them when the cache is on; these also cover a
/// store without a cache. One set per process, shared by every store and namespace, and kept out
/// of `Store` so its size does not change.
struct ChannelIndexLocks([parking_lot::Mutex<()>; 64]);

static CHANNEL_INDEX_LOCKS: ChannelIndexLocks =
    ChannelIndexLocks([const { parking_lot::const_mutex(()) }; 64]);

impl ChannelIndexLocks {
    fn stripe(&self, channel_id: &Hash256) -> usize {
        channel_id.as_ref()[0] as usize % self.0.len()
    }

    fn lock(&self, channel_id: &Hash256) -> parking_lot::MutexGuard<'_, ()> {
        self.0[self.stripe(channel_id)].lock()
    }

    /// Lock the stripes of two channels in a fixed order.
    fn lock_pair(
        &self,
        a: &Hash256,
        b: &Hash256,
    ) -> (
        parking_lot::MutexGuard<'_, ()>,
        Option<parking_lot::MutexGuard<'_, ()>>,
    ) {
        let (a, b) = (self.stripe(a), self.stripe(b));
        if a == b {
            return (self.0[a].lock(), None);
        }
        let (first, second) = if a < b { (a, b) } else { (b, a) };
        let first = self.0[first].lock();
        (first, Some(self.0[second].lock()))
    }
}

/// Key in the shared channel cache: physical stores can host multiple logical node namespaces
/// (host + hosted tenants) that share identical channel IDs.
type ChannelCacheKey = (Option<NodeNamespace>, Hash256);

/// The channel records of one store.
pub(crate) struct ChannelStates<'a> {
    store: &'a Store,
    cache: Option<&'a Arc<ChannelStateCache>>,
}

impl<'a> ChannelStates<'a> {
    pub(super) fn new(store: &'a Store, cache: Option<&'a Arc<ChannelStateCache>>) -> Self {
        Self { store, cache }
    }

    fn cache_key(&self, id: &Hash256) -> ChannelCacheKey {
        (self.store.namespace().cloned(), *id)
    }

    /// Decode a channel state from its two stored records, bypassing the cache.
    fn load(&self, id: &Hash256) -> Option<ChannelActorState> {
        // Both records are written and deleted in one batch, so they have to be read from one
        // snapshot: a delete or a move between two reads would otherwise hand back half a channel.
        let [mutable, immutable] = self
            .store
            .multi_get(&[channel_mutable_key(id), channel_immutable_key(id)])
            .try_into()
            .expect("two keys were requested");
        match (mutable, immutable) {
            (Some(mutable), Some(immutable)) => {
                Some(deserialize_channel_state_parts(id, &mutable, &immutable))
            }
            (None, None) => None,
            (Some(_), None) => panic!(
                "channel {id}: mutable record without its fixed record; run the database migration first"
            ),
            (None, Some(_)) => panic!("channel {id}: fixed record without its mutable record"),
        }
    }

    /// Stage the payment hash index rows that change a channel's indexed hashes to `new`.
    /// The caller holds the channel's index lock until the batch commits.
    fn stage_payment_hash_index(
        &self,
        batch: &mut impl BatchWriter,
        channel_id: &Hash256,
        new: &BTreeSet<Hash256>,
    ) {
        let key = channel_payment_hashes_key(channel_id);
        let old: Vec<Hash256> = self
            .store
            .get(&key)
            .map(|value| deserialize_from(value.as_ref(), "ChannelPaymentHashes"))
            .unwrap_or_default();
        if old.len() == new.len() && old.iter().eq(new.iter()) {
            return;
        }
        for hash in old.iter().filter(|hash| !new.contains(hash)) {
            batch.delete(payment_hash_channel_id_key(hash, channel_id));
        }
        for hash in new.iter().filter(|hash| old.binary_search(hash).is_err()) {
            batch.put(payment_hash_channel_id_key(hash, channel_id), []);
        }
        if new.is_empty() {
            batch.delete(key);
        } else {
            let hashes: Vec<Hash256> = new.iter().copied().collect();
            batch.put(key, serialize_to_vec(&hashes, "ChannelPaymentHashes"));
        }
    }

    /// Write the channel records without touching the cache.
    fn write(&self, state: ChannelActorState, write_immutable: bool) {
        let _index_guard = CHANNEL_INDEX_LOCKS.lock(&state.id);
        let mut batch = self.store.batch();
        self.stage_payment_hash_index(&mut batch, &state.id, &state.pending_payment_hashes());
        stage_channel_actor_state(self.store, &mut batch, state, write_immutable);
        batch.commit();
    }

    /// Delete the channel record and its indexes without touching the cache.
    fn remove(&self, id: &Hash256, existing: Option<&ChannelActorState>) {
        let Some(existing) = existing else {
            return;
        };
        let _index_guard = CHANNEL_INDEX_LOCKS.lock(id);
        let mut batch = self.store.batch();
        self.stage_payment_hash_index(&mut batch, id, &BTreeSet::new());
        batch.delete(channel_mutable_key(id));
        batch.delete(channel_immutable_key(id));
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
        let key = self.cache_key(id);
        self.cache
            .and_then(|cache| cache.peek(&key))
            .map(|hit| (*hit).clone())
            .or_else(|| self.load(id))
    }

    /// Persist a channel state.
    ///
    /// The fixed record is written only while the channel is opening, so the caller has to arrive
    /// with one already there: a channel whose first write is already established (a fixture, or a
    /// caller that lost the opening write) would leave a record that cannot be read back.
    pub(super) fn insert(&self, state: ChannelActorState) {
        self.insert_with(state, false)
    }

    /// Test-only write that always refreshes the fixed record, in the same batch as the mutable
    /// one, so a fixture may inject a fixed field of an established channel.
    #[cfg(any(test, feature = "sample", feature = "bench"))]
    pub(super) fn insert_for_test(&self, state: ChannelActorState) {
        self.insert_with(state, true)
    }

    fn insert_with(&self, mut state: ChannelActorState, force_immutable: bool) {
        state.clear_local_signer_material();
        let write_immutable = force_immutable || !state.state.is_established();
        let Some(cache) = self.cache else {
            return self.write(state, write_immutable);
        };
        // Only a ready channel is cached: its id is final (a channel id changes once, during the
        // opening handshake) and its record is what the message path reads back.
        let cacheable = state.state == ChannelState::ChannelReady;
        // The commit and the cache update happen under the channel's slot, so a write of a ready
        // state and a transition out of it cannot disagree about which one is current: whichever
        // commits last also decides the snapshot.
        let key = self.cache_key(&state.id);
        {
            let entry = cache.slot(key.clone());
            let mut slot = entry.write();
            if cacheable {
                // Clone the snapshot only when it can be published: a full cache drops it, and the
                // clone would be paid for nothing on every write of an uncached channel.
                let cached = cache
                    .has_room(&slot)
                    .then(|| channel_cache_snapshot(&state));
                self.write(state, write_immutable);
                if let Some(cached) = cached {
                    cache.install(&mut slot, cached);
                }
            } else {
                cache.unpublish(&mut slot);
                self.write(state, write_immutable);
            }
        }
        cache.discard_if_empty(&key);
    }

    /// Move a channel to a new id.
    ///
    /// Only the opening handshake does this, when the id derived from the funding keys replaces
    /// the temporary one, so the channel is never ready here and no snapshot is published for
    /// either id. Both slots are taken in id order, so two concurrent moves cannot deadlock, and
    /// the rewrite happens under them so no concurrent write can interleave with it. The old
    /// record and its indexes go away in the same batch as the new ones, so an interrupted move
    /// leaves the channel untouched.
    pub(super) fn move_to(&self, old_id: &Hash256, mut state: ChannelActorState) {
        state.clear_local_signer_material();
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
        let ns = self.store.namespace().cloned();
        let first_key = (ns.clone(), first_id);
        let second_key = (ns, second_id);
        let first = cache.slot(first_key.clone());
        let second = cache.slot(second_key.clone());
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
        cache.discard_if_empty(&first_key);
        cache.discard_if_empty(&second_key);
    }

    /// Rewrite the channel records under a new id, in one batch.
    fn rewrite_records(
        &self,
        old_id: &Hash256,
        old_state: Option<&ChannelActorState>,
        state: ChannelActorState,
    ) {
        let _index_guards = CHANNEL_INDEX_LOCKS.lock_pair(old_id, &state.id);
        let mut batch = self.store.batch();
        self.stage_payment_hash_index(&mut batch, old_id, &BTreeSet::new());
        self.stage_payment_hash_index(&mut batch, &state.id, &state.pending_payment_hashes());
        if let Some(old_state) = old_state {
            batch.delete(channel_mutable_key(old_id));
            batch.delete(channel_immutable_key(old_id));
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
        // `new_id` did not exist before this batch, so it needs both records whatever the state is.
        let new_id = state.id;
        stage_channel_actor_state(self.store, &mut batch, state, true);
        // Atomically re-key any persisted opening records alongside the channel state so a
        // restart between ChannelActor's final-ID write and NetworkActor's event handling
        // cannot leave channel state at `new_id` while open records remain stranded at `old_id`.
        let open_record = self
            .store
            .get_channel_open_record(old_id)
            .map(|mut record| {
                record.channel_id = new_id;
                record.update_status(ChannelOpeningStatus::FundingTxBuilding);
                record
            });
        let tenant_opening = self
            .store
            .get_tenant_channel_opening(old_id)
            .map(|mut opening| {
                opening.channel_id = new_id;
                opening
            });
        stage_move_channel_open_records(&mut batch, old_id, open_record, tenant_opening);
        batch.commit();
    }

    pub(super) fn delete(&self, id: &Hash256) {
        let Some(cache) = self.cache else {
            let existing = self.load(id);
            return self.remove(id, existing.as_ref());
        };
        let key = self.cache_key(id);
        {
            let entry = cache.slot(key.clone());
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
        cache.discard_if_empty(&key);
    }

    #[cfg(test)]
    pub(super) fn cache_len(&self) -> usize {
        self.cache.map_or(0, |cache| {
            let ns = self.store.namespace();
            cache
                .entries
                .iter()
                .filter(|entry| entry.key().0.as_ref() == ns)
                .count()
        })
    }

    /// The cached snapshot together with the stored record, both read while the slot's read lock
    /// is held, so a concurrent write cannot change the record between the two reads (tests only).
    #[cfg(test)]
    pub(super) fn cache_and_record(
        &self,
        id: &Hash256,
    ) -> Option<(ChannelActorState, Option<ChannelActorState>)> {
        let cache = self.cache?;
        let key = self.cache_key(id);
        let entry = cache.entries.get(&key)?;
        let slot = entry.try_read()?;
        let cached = (**slot.as_ref()?).clone();
        let record = self.load(id);
        Some((cached, record))
    }

    /// The cached snapshot only, without falling back to the record.
    #[cfg(any(test, all(feature = "simulation", not(target_arch = "wasm32"))))]
    pub(super) fn cache_peek(&self, id: &Hash256) -> Option<ChannelActorState> {
        let key = self.cache_key(id);
        self.cache
            .and_then(|cache| cache.peek(&key))
            .map(|hit| (*hit).clone())
    }
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
    entries: dashmap::DashMap<ChannelCacheKey, ChannelCacheEntry>,
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
    fn peek(&self, key: &ChannelCacheKey) -> Option<Arc<ChannelActorState>> {
        let entry = self.entries.get(key)?;
        let slot = entry.try_read()?;
        slot.as_ref().map(Arc::clone)
    }

    /// The channel's slot, created on first use.
    ///
    /// Every write takes it, even when the cache is full or the state is not cacheable, so all
    /// writes of one channel are serialized by the same lock. The returned handle keeps the slot
    /// alive for as long as the writer uses it; an empty slot is dropped from the map afterwards
    /// (see `discard_if_empty`), so slots never outlive the write that created them.
    fn slot(&self, key: ChannelCacheKey) -> ChannelCacheEntry {
        match self.entries.get(&key) {
            Some(entry) => Arc::clone(&entry),
            None => self
                .entries
                .entry(key)
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
    fn discard_if_empty(&self, key: &ChannelCacheKey) {
        self.entries.remove_if(key, |_, entry| {
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
