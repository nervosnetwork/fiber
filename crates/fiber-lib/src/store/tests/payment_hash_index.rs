//! The payment hash index must give the same answer as reading every stored channel.
use std::collections::BTreeSet;

use fiber_store::backend::StorageBackend;
use fiber_store::migration::MIGRATION_VERSION_KEY;
use fiber_types::schema::{CHANNEL_PAYMENT_HASHES_PREFIX, PAYMENT_HASH_CHANNEL_ID_PREFIX};
use fiber_types::{
    AppliedFlags, CommitmentNumbers, Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus,
    TLCId, TlcInfo, TlcStatus,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::fiber::channel::{
    has_pending_tlc_for_payment_hash, ChannelActorState, ChannelActorStateStore,
};
use crate::gen_rand_sha256_hash;
use crate::store::sample::StoreSample;
use crate::store::{open_store, open_store_with_migration, NodeNamespace, Store};
use crate::tests::test_utils::TempDir;

/// The database version before the payment hash index existed.
const VERSION_WITHOUT_INDEX: &str = "20261113000001";

fn new_channel() -> ChannelActorState {
    let mut state = ChannelActorState::samples(1)
        .into_iter()
        .next()
        .expect("channel sample");
    state.core.id = gen_rand_sha256_hash();
    state.core.tlc_state = Default::default();
    state.core.session.remove_effects.clear();
    state
}

/// Write a channel state: its first write stores the fixed record too, as the opening handshake
/// does for a real channel; every later write is the normal one.
fn save(store: &Store, state: ChannelActorState) {
    if store.get_channel_actor_state(&state.id).is_none() {
        store.insert_channel_actor_state_for_test(state);
    } else {
        store.insert_channel_actor_state(state);
    }
}

/// Open a store with the given channel cache capacity (0 = off).
fn open_with_cache(path: &TempDir, capacity: usize) -> Store {
    open_store_with_migration(
        path.as_ref(),
        false,
        capacity,
        Box::new(|_| true),
        Box::new(|_| {}),
    )
    .unwrap()
}

fn tlc(tlc_id: TLCId, payment_hash: Hash256) -> TlcInfo {
    TlcInfo {
        upstream_tlc_key: None,
        status: match tlc_id {
            TLCId::Offered(_) => TlcStatus::Outbound(OutboundTlcStatus::Committed),
            TLCId::Received(_) => TlcStatus::Inbound(InboundTlcStatus::Committed),
        },
        tlc_id,
        amount: 1000,
        payment_hash,
        total_amount: None,
        payment_secret: None,
        attempt_id: None,
        expiry: u64::MAX / 2,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [42u8; 32],
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::default(),
        removed_reason: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::empty(),
        forwarding_tlc: None,
    }
}

fn add_tlc(state: &mut ChannelActorState, tlc_id: TLCId, payment_hash: Hash256) {
    let tlc = tlc(tlc_id, payment_hash);
    match tlc_id {
        TLCId::Offered(_) => state.core.tlc_state.offered_tlcs.tlcs.push(tlc),
        TLCId::Received(_) => state.core.tlc_state.received_tlcs.tlcs.push(tlc),
    }
}

fn add_remove_effect(state: &mut ChannelActorState, tlc_id: TLCId, payment_hash: Hash256) {
    state
        .core
        .session
        .remove_effects
        .push((tlc(tlc_id, payment_hash), None));
}

fn hash_pair(key: &[u8], offset: usize) -> (Hash256, Hash256) {
    let first: [u8; 32] = key[offset..offset + 32].try_into().unwrap();
    let second: [u8; 32] = key[offset + 32..offset + 64].try_into().unwrap();
    (first.into(), second.into())
}

/// (payment_hash, channel_id) rows of the index.
fn index_rows(store: &Store) -> BTreeSet<(Hash256, Hash256)> {
    store
        .prefix_iterator(vec![PAYMENT_HASH_CHANNEL_ID_PREFIX])
        .map(|(key, _)| {
            assert_eq!(key.len(), 65, "index key length");
            hash_pair(&key, 1)
        })
        .collect()
}

/// (payment_hash, channel_id) pairs recorded by the per-channel hash lists.
fn channel_hash_rows(store: &Store) -> BTreeSet<(Hash256, Hash256)> {
    let mut rows = BTreeSet::new();
    for (key, value) in store.prefix_iterator(vec![CHANNEL_PAYMENT_HASHES_PREFIX]) {
        let channel_id: [u8; 32] = key[1..].try_into().unwrap();
        let hashes: Vec<Hash256> = bincode::deserialize(&value).unwrap();
        assert!(!hashes.is_empty(), "empty hash lists are deleted");
        assert!(hashes.windows(2).all(|pair| pair[0] < pair[1]), "sorted");
        rows.extend(hashes.into_iter().map(|hash| (hash, channel_id.into())));
    }
    rows
}

/// What the scan sees: every channel listed by `get_channel_states(None)`.
fn scan_rows(store: &Store) -> BTreeSet<(Hash256, Hash256)> {
    let mut rows = BTreeSet::new();
    for (_, channel_id, _) in store.get_channel_states(None) {
        let state = store.get_channel_actor_state(&channel_id).unwrap();
        rows.extend(
            state
                .pending_payment_hashes()
                .into_iter()
                .map(|hash| (hash, channel_id)),
        );
    }
    rows
}

/// The full scan that `has_pending_tlc_for_payment_hash` did before the index.
fn scan_other_channel(store: &Store, payment_hash: &Hash256, channel_id: &Hash256) -> bool {
    store
        .get_channel_states(None)
        .into_iter()
        .filter(|(_, id, _)| id != channel_id)
        .filter_map(|(_, id, _)| store.get_channel_actor_state(&id))
        .any(|state| state.pending_payment_hashes().contains(payment_hash))
}

fn assert_index_matches_scan(store: &Store, hashes: &[Hash256], channels: &[Hash256]) {
    let scan = scan_rows(store);
    assert_eq!(index_rows(store), scan, "index rows differ from the scan");
    assert_eq!(channel_hash_rows(store), scan, "channel hash lists differ");
    let absent = gen_rand_sha256_hash();
    for hash in hashes.iter().chain([&absent]) {
        for channel in channels.iter().chain([&absent]) {
            assert_eq!(
                store.other_channel_has_pending_payment_hash(hash, channel),
                scan_other_channel(store, hash, channel),
                "lookup differs from the scan for hash {hash} excluding {channel}"
            );
        }
    }
}

#[test]
fn test_payment_hash_index_follows_tlc_add_and_remove() {
    let path = TempDir::new("payment_hash_index_tlc");
    let store = open_store(path.as_ref()).unwrap();
    let (h1, h2) = (gen_rand_sha256_hash(), gen_rand_sha256_hash());
    let mut a = new_channel();
    let mut b = new_channel();
    save(&store, a.clone());
    save(&store, b.clone());
    let ids = [a.id, b.id];
    assert!(index_rows(&store).is_empty());

    add_tlc(&mut a, TLCId::Received(0), h1);
    save(&store, a.clone());
    add_tlc(&mut b, TLCId::Offered(0), h1);
    add_tlc(&mut b, TLCId::Offered(1), h2);
    save(&store, b.clone());
    assert!(store.other_channel_has_pending_payment_hash(&h1, &a.id));
    assert!(store.other_channel_has_pending_payment_hash(&h2, &a.id));
    assert!(!store.other_channel_has_pending_payment_hash(&h2, &b.id));
    assert_index_matches_scan(&store, &[h1, h2], &ids);

    b.core.tlc_state.apply_remove_tlc(TLCId::Offered(0));
    save(&store, b.clone());
    assert!(!store.other_channel_has_pending_payment_hash(&h1, &a.id));
    assert!(store.other_channel_has_pending_payment_hash(&h1, &b.id));
    assert_index_matches_scan(&store, &[h1, h2], &ids);

    a.core.tlc_state.apply_remove_tlc(TLCId::Received(0));
    b.core.tlc_state.apply_remove_tlc(TLCId::Offered(1));
    save(&store, a);
    save(&store, b);
    assert!(index_rows(&store).is_empty());
    assert_index_matches_scan(&store, &[h1, h2], &ids);
}

#[test]
fn test_payment_hash_index_follows_remove_effects() {
    let path = TempDir::new("payment_hash_index_remove_effects");
    let store = open_store(path.as_ref()).unwrap();
    let hash = gen_rand_sha256_hash();
    let current = new_channel();
    let mut other = new_channel();
    save(&store, current.clone());
    add_remove_effect(&mut other, TLCId::Offered(3), hash);
    save(&store, other.clone());
    let ids = [current.id, other.id];
    assert!(store.other_channel_has_pending_payment_hash(&hash, &current.id));
    assert!(has_pending_tlc_for_payment_hash(&store, &current, hash));
    assert_index_matches_scan(&store, &[hash], &ids);

    // A TLC and a remove effect with the same hash: clearing one keeps the row.
    add_tlc(&mut other, TLCId::Offered(3), hash);
    save(&store, other.clone());
    other.core.session.remove_effects.clear();
    save(&store, other.clone());
    assert!(store.other_channel_has_pending_payment_hash(&hash, &current.id));
    assert_index_matches_scan(&store, &[hash], &ids);

    other.core.tlc_state.apply_remove_tlc(TLCId::Offered(3));
    save(&store, other);
    assert!(!store.other_channel_has_pending_payment_hash(&hash, &current.id));
    assert!(!has_pending_tlc_for_payment_hash(&store, &current, hash));
    assert_index_matches_scan(&store, &[hash], &ids);
}

#[test]
fn test_payment_hash_index_follows_channel_delete() {
    let path = TempDir::new("payment_hash_index_delete");
    let store = open_store(path.as_ref()).unwrap();
    let hash = gen_rand_sha256_hash();
    let current = new_channel();
    let mut other = new_channel();
    add_tlc(&mut other, TLCId::Received(0), hash);
    add_remove_effect(&mut other, TLCId::Received(1), gen_rand_sha256_hash());
    save(&store, current.clone());
    save(&store, other.clone());
    assert!(store.other_channel_has_pending_payment_hash(&hash, &current.id));

    store.delete_channel_actor_state(&other.id);
    assert!(index_rows(&store).is_empty());
    assert!(channel_hash_rows(&store).is_empty());
    assert!(!store.other_channel_has_pending_payment_hash(&hash, &current.id));
    assert_index_matches_scan(&store, &[hash], &[current.id, other.id]);
}

#[test]
fn test_payment_hash_index_follows_channel_move() {
    let path = TempDir::new("payment_hash_index_move");
    let store = open_store(path.as_ref()).unwrap();
    let hash = gen_rand_sha256_hash();
    let current = new_channel();
    let mut other = new_channel();
    add_tlc(&mut other, TLCId::Offered(0), hash);
    save(&store, current.clone());
    save(&store, other.clone());
    let old_id = other.id;

    other.core.id = gen_rand_sha256_hash();
    store.move_channel_actor_state(&old_id, other.clone());
    assert_eq!(index_rows(&store), BTreeSet::from([(hash, other.id)]));
    assert!(store.other_channel_has_pending_payment_hash(&hash, &current.id));
    assert!(store.other_channel_has_pending_payment_hash(&hash, &old_id));
    assert!(!store.other_channel_has_pending_payment_hash(&hash, &other.id));
    assert_index_matches_scan(&store, &[hash], &[current.id, old_id, other.id]);

    // A move onto the same id is a plain insert.
    other.core.tlc_state.apply_remove_tlc(TLCId::Offered(0));
    store.move_channel_actor_state(&other.id.clone(), other.clone());
    assert!(index_rows(&store).is_empty());
    assert_index_matches_scan(&store, &[hash], &[current.id, old_id, other.id]);
}

#[test]
fn test_payment_hash_index_is_scoped_to_namespace() {
    scoped_to_namespace(crate::fiber::config::DEFAULT_STORE_CHANNEL_CACHE_CAPACITY);
}

#[test]
fn test_payment_hash_index_is_scoped_to_namespace_without_cache() {
    scoped_to_namespace(0);
}

fn scoped_to_namespace(cache_capacity: usize) {
    let path = TempDir::new("payment_hash_index_namespace");
    let root = open_with_cache(&path, cache_capacity);
    let tenant_a = root.namespaced(NodeNamespace::hosted_tenant("a"));
    let tenant_b = root.namespaced(NodeNamespace::hosted_tenant("b"));
    tenant_a.ensure_current_schema().unwrap();
    tenant_b.ensure_current_schema().unwrap();
    let hash = gen_rand_sha256_hash();
    let probe = new_channel();
    let mut in_a = new_channel();
    add_tlc(&mut in_a, TLCId::Received(0), hash);
    save(&tenant_a, in_a.clone());
    save(&tenant_b, new_channel());
    save(&root, new_channel());

    assert!(tenant_a.other_channel_has_pending_payment_hash(&hash, &probe.id));
    assert!(!tenant_b.other_channel_has_pending_payment_hash(&hash, &probe.id));
    assert!(!root.other_channel_has_pending_payment_hash(&hash, &probe.id));
    assert!(index_rows(&root).is_empty());
    assert!(index_rows(&tenant_b).is_empty());
    for store in [&root, &tenant_a, &tenant_b] {
        assert_index_matches_scan(store, &[hash], &[probe.id, in_a.id]);
    }

    tenant_a.delete_channel_actor_state(&in_a.id);
    assert!(!tenant_a.other_channel_has_pending_payment_hash(&hash, &probe.id));
    assert_index_matches_scan(&tenant_a, &[hash], &[probe.id, in_a.id]);
}

/// Remove every index row and rewind the version, as a database written before the index.
fn drop_index(store: &Store) {
    for prefix in [
        PAYMENT_HASH_CHANNEL_ID_PREFIX,
        CHANNEL_PAYMENT_HASHES_PREFIX,
    ] {
        for (key, _) in store.prefix_iterator(vec![prefix]) {
            store.delete(key);
        }
    }
    store.put(MIGRATION_VERSION_KEY, VERSION_WITHOUT_INDEX);
}

#[test]
fn test_payment_hash_index_is_built_for_database_without_it() {
    let path = TempDir::new("payment_hash_index_rebuild");
    let hashes: Vec<Hash256> = (0..3).map(|_| gen_rand_sha256_hash()).collect();
    let mut ids = Vec::new();
    {
        let root = open_store(path.as_ref()).unwrap();
        let tenant = root.namespaced(NodeNamespace::hosted_tenant("t"));
        tenant.ensure_current_schema().unwrap();
        for (index, store) in [&root, &tenant, &root].into_iter().enumerate() {
            let mut state = new_channel();
            add_tlc(&mut state, TLCId::Offered(0), hashes[index]);
            add_remove_effect(&mut state, TLCId::Received(0), hashes[(index + 1) % 3]);
            ids.push(state.id);
            save(store, state);
        }
        // A channel with nothing pending stays out of the index.
        save(&root, new_channel());
        drop_index(&root);
        drop_index(&tenant);
        // Precondition: the scan finds what the missing index cannot.
        for store in [&root, &tenant] {
            assert!(!scan_rows(store).is_empty());
            assert!(index_rows(store).is_empty());
            assert!(!store.other_channel_has_pending_payment_hash(&hashes[0], &Hash256::default()));
        }
    }

    let root = open_store(path.as_ref()).unwrap();
    assert_eq!(
        root.get(MIGRATION_VERSION_KEY).unwrap(),
        fiber_store::migration::LATEST_DB_VERSION.as_bytes()
    );
    let tenant = root.namespaced(NodeNamespace::hosted_tenant("t"));
    tenant.ensure_current_schema().unwrap();
    for store in [&root, &tenant] {
        assert!(!index_rows(store).is_empty());
        assert_index_matches_scan(store, &hashes, &ids);
    }
    assert_eq!(index_rows(&root).len(), 4);
    assert_eq!(index_rows(&tenant).len(), 2);
}

#[test]
fn test_payment_hash_index_migration_replaces_stale_rows() {
    let path = TempDir::new("payment_hash_index_stale");
    let hash = gen_rand_sha256_hash();
    let stale = gen_rand_sha256_hash();
    let channel_id;
    {
        let store = open_store(path.as_ref()).unwrap();
        let mut state = new_channel();
        add_tlc(&mut state, TLCId::Offered(0), hash);
        channel_id = state.id;
        save(&store, state);
        drop_index(&store);
        store.put(
            fiber_types::schema::payment_hash_channel_id_key(&stale, &channel_id),
            [],
        );
    }
    let store = open_store(path.as_ref()).unwrap();
    assert_eq!(index_rows(&store), BTreeSet::from([(hash, channel_id)]));
    assert_index_matches_scan(&store, &[hash, stale], &[channel_id]);
}

#[test]
fn test_payment_hash_index_matches_scan_under_random_operations() {
    random_operations(crate::fiber::config::DEFAULT_STORE_CHANNEL_CACHE_CAPACITY);
}

#[test]
fn test_payment_hash_index_matches_scan_under_random_operations_without_cache() {
    random_operations(0);
}

fn random_operations(cache_capacity: usize) {
    let path = TempDir::new("payment_hash_index_random");
    let store = open_with_cache(&path, cache_capacity);
    let mut rng = StdRng::seed_from_u64(1720);
    let hashes: Vec<Hash256> = (0..5).map(|_| gen_rand_sha256_hash()).collect();
    let mut channels: Vec<ChannelActorState> = Vec::new();
    let mut known_ids: Vec<Hash256> = Vec::new();
    let mut next_tlc_id = 0u64;
    for step in 0..400 {
        let op = if channels.is_empty() {
            0
        } else {
            rng.random_range(0..8)
        };
        let pick = if channels.is_empty() {
            0
        } else {
            rng.random_range(0..channels.len())
        };
        let hash = hashes[rng.random_range(0..hashes.len())];
        next_tlc_id += 1;
        match op {
            0 => {
                let state = new_channel();
                known_ids.push(state.id);
                save(&store, state.clone());
                channels.push(state);
            }
            1 | 2 => {
                let tlc_id = if op == 1 {
                    TLCId::Offered(next_tlc_id)
                } else {
                    TLCId::Received(next_tlc_id)
                };
                add_tlc(&mut channels[pick], tlc_id, hash);
                save(&store, channels[pick].clone());
            }
            3 => {
                let tlc_ids: Vec<TLCId> = channels[pick]
                    .tlc_state
                    .all_tlcs()
                    .map(|tlc| tlc.tlc_id)
                    .collect();
                if let Some(tlc_id) = tlc_ids.get(rng.random_range(0..tlc_ids.len().max(1))) {
                    channels[pick].core.tlc_state.apply_remove_tlc(*tlc_id);
                }
                save(&store, channels[pick].clone());
            }
            4 => {
                add_remove_effect(&mut channels[pick], TLCId::Offered(next_tlc_id), hash);
                save(&store, channels[pick].clone());
            }
            5 => {
                let keep = rng.random_bool(0.5);
                channels[pick]
                    .core
                    .session
                    .remove_effects
                    .retain(|(tlc, _)| keep && tlc.payment_hash != hash);
                save(&store, channels[pick].clone());
            }
            6 => {
                let state = channels.swap_remove(pick);
                store.delete_channel_actor_state(&state.id);
            }
            _ => {
                let old_id = channels[pick].id;
                channels[pick].core.id = gen_rand_sha256_hash();
                known_ids.push(channels[pick].id);
                store.move_channel_actor_state(&old_id, channels[pick].clone());
            }
        }
        if step % 4 == 0 {
            assert_index_matches_scan(&store, &hashes, &known_ids);
        }
    }
    assert_index_matches_scan(&store, &hashes, &known_ids);
    assert!(
        !index_rows(&store).is_empty()
            || channels
                .iter()
                .all(|c| c.pending_payment_hashes().is_empty())
    );
}

#[test]
fn test_pending_tlc_check_uses_memory_for_the_current_channel() {
    let path = TempDir::new("payment_hash_index_current");
    let store = open_store(path.as_ref()).unwrap();
    let hash = gen_rand_sha256_hash();
    let mut current = new_channel();
    add_tlc(&mut current, TLCId::Received(0), hash);
    save(&store, current.clone());
    assert!(has_pending_tlc_for_payment_hash(&store, &current, hash));
    // The stored state still has the TLC; the in-memory state has removed it.
    current.core.tlc_state.apply_remove_tlc(TLCId::Received(0));
    assert!(!has_pending_tlc_for_payment_hash(&store, &current, hash));
}
