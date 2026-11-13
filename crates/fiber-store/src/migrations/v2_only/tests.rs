use fiber_types::channel_v2_validation::decode_and_validate_channel_actor_data;
use fiber_types::sample::StoreSample;
use fiber_types::schema::{CHANNEL_OPEN_RECORD_PREFIX, PUBKEY_CHANNEL_ID_PREFIX};
use fiber_types::{ChannelActorData, ChannelSessionV2};
use fiber_types_0100::sample::StoreSample as OldStoreSample;

use super::{
    actor_key, archive_key, decode_as_new, decode_exact, OldChannelActorData, OldChannelOpenRecord,
    OldChannelOpeningStatus, OldChannelState, OldCloseFlags, MIGRATION_DB_VERSION,
    RELEASED_VERSION,
};
use crate::backend::StorageBackend;
use crate::db_migrate::DbMigrate;
use crate::migration::MIGRATION_VERSION_KEY;

#[path = "test_utils.rs"]
mod test_utils;
use test_utils::released_v2;
fn database() -> (tempfile::TempDir, crate::Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::Store::open_db(dir.path()).unwrap();
    store.put(MIGRATION_VERSION_KEY, RELEASED_VERSION);
    (dir, store)
}

fn migrate(store: &crate::Store) -> Result<(), crate::migration::MigrateError> {
    let mut migrations = DbMigrate::new();
    crate::migrations::register_all_migrations(&mut migrations);
    migrations.auto_migrate(store, Box::new(|_| true), Box::new(|_| {}))
}

fn sample(seed: u64) -> ChannelActorData {
    ChannelActorData::samples(seed).remove(0)
}

fn legacy(seed: u64, state: OldChannelState) -> OldChannelActorData {
    let mut record = OldChannelActorData::samples(seed).remove(0);
    record.state = state;
    record
}

fn snapshot(store: &crate::Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    store.prefix_iterator(Vec::new()).collect()
}

#[test]
fn test_released_v2_migrates_without_changing_signing_state() {
    let (_dir, store) = database();
    let channel = sample(41);
    let key = actor_key(&channel.id);
    let encoded = bincode::serialize(&released_v2(channel.clone())).unwrap();
    store.put(&key, &encoded);
    migrate(&store).unwrap();
    let value = store.get(&key).unwrap();
    assert_eq!(value, bincode::serialize(&channel).unwrap());
    let restored = decode_and_validate_channel_actor_data(&value).unwrap();
    assert_eq!(
        bincode::serialize(&restored.session).unwrap(),
        bincode::serialize(&channel.session).unwrap()
    );
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        MIGRATION_DB_VERSION.as_bytes()
    );
    assert_eq!(snapshot(&store).len(), 2);
}

#[test]
fn test_active_or_unsettled_v1_blocks_all_writes() {
    let states = [
        OldChannelState::ChannelReady,
        OldChannelState::Stale,
        OldChannelState::ShuttingDown(
            fiber_types_0100::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
        ),
        OldChannelState::Closed(
            OldCloseFlags::UNCOOPERATIVE_LOCAL | OldCloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        ),
        OldChannelState::Closed(
            OldCloseFlags::UNCOOPERATIVE_REMOTE
                | OldCloseFlags::WAITING_ONCHAIN_SETTLEMENT
                | OldCloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        ),
    ];
    for state in states {
        let (_dir, store) = database();
        let v2 = sample(1);
        store.put(
            actor_key(&v2.id),
            bincode::serialize(&released_v2(v2)).unwrap(),
        );
        let v1 = legacy(99, state);
        store.put(actor_key(&v1.id), bincode::serialize(&v1).unwrap());
        let original = snapshot(&store);
        let error = migrate(&store).unwrap_err().to_string();
        assert!(error.contains(&v1.id.to_string()), "{error}");
        assert!(error.contains("old binary"), "{error}");
        assert_eq!(snapshot(&store), original);
    }
}

#[test]
fn test_closed_v1_and_indexes_are_archived_verbatim() {
    for state in [
        OldCloseFlags::COOPERATIVE,
        OldCloseFlags::UNCOOPERATIVE_LOCAL,
        OldCloseFlags::UNCOOPERATIVE_REMOTE,
        OldCloseFlags::FUNDING_ABORTED,
        OldCloseFlags::ABANDONED,
    ] {
        let (_dir, store) = database();
        let v1 = legacy(101, OldChannelState::Closed(state));
        let key = actor_key(&v1.id);
        let index = [
            &[PUBKEY_CHANNEL_ID_PREFIX],
            v1.remote_pubkey.serialize().as_slice(),
            v1.id.as_ref(),
        ]
        .concat();
        let encoded = bincode::serialize(&v1).unwrap();
        let index_value = bincode::serialize(&v1.state).unwrap();
        store.put(&key, &encoded);
        store.put(&index, &index_value);
        migrate(&store).unwrap();
        assert!(store.get(&key).is_none());
        assert!(store.get(&index).is_none());
        assert_eq!(store.get(archive_key(&key)).unwrap(), encoded);
        assert_eq!(store.get(archive_key(&index)).unwrap(), index_value);
    }
}

#[test]
fn test_closed_v1_with_pending_delivery_is_not_retired() {
    let (_dir, store) = database();
    let mut v1 = legacy(102, OldChannelState::Closed(OldCloseFlags::COOPERATIVE));
    v1.waiting_forward_tlc_tasks
        .insert(fiber_types_0100::TLCId::Received(0), [1; 32]);
    store.put(actor_key(&v1.id), bincode::serialize(&v1).unwrap());
    let original = snapshot(&store);
    assert!(migrate(&store).is_err());
    assert_eq!(snapshot(&store), original);
}

#[test]
fn test_migration_resumes_after_partial_v2_conversion_and_v1_archive() {
    let (_dir, store) = database();
    let current = sample(10);
    store.put(
        actor_key(&current.id),
        bincode::serialize(&current).unwrap(),
    );
    let old = sample(11);
    store.put(
        actor_key(&old.id),
        bincode::serialize(&released_v2(old.clone())).unwrap(),
    );
    let v1 = legacy(12, OldChannelState::Closed(OldCloseFlags::COOPERATIVE));
    store.put(
        archive_key(&actor_key(&v1.id)),
        bincode::serialize(&v1).unwrap(),
    );
    let opening = OldChannelOpenRecord {
        channel_id: v1.id,
        pubkey: v1.remote_pubkey,
        is_acceptor: false,
        status: OldChannelOpeningStatus::ChannelReady,
        funding_amount: 1,
        failure_detail: None,
        created_at: 1,
        last_updated_at: 1,
        commitment_contract_features: fiber_types_0100::CommitmentContractFeatures::LEGACY,
    };
    let open_key = [&[CHANNEL_OPEN_RECORD_PREFIX], v1.id.as_ref()].concat();
    let encoded = bincode::serialize(&opening).unwrap();
    store.put(&open_key, &encoded);
    migrate(&store).unwrap();
    assert_eq!(
        store.get(actor_key(&current.id)).unwrap(),
        bincode::serialize(&current).unwrap()
    );
    assert_eq!(
        store.get(actor_key(&old.id)).unwrap(),
        bincode::serialize(&old).unwrap()
    );
    assert_eq!(store.get(archive_key(&open_key)).unwrap(), encoded);
    let completed = snapshot(&store);
    migrate(&store).unwrap();
    assert_eq!(snapshot(&store), completed);
}

#[test]
fn test_truncated_or_invalid_v2_is_never_repaired() {
    for truncate in [true, false] {
        let (_dir, store) = database();
        let channel = sample(77);
        let mut stored = released_v2(channel);
        if !truncate {
            stored.1.as_mut().unwrap().marker = 0;
        }
        let key = actor_key(&stored.0.id);
        let mut value = bincode::serialize(&stored).unwrap();
        if truncate {
            value.pop();
        }
        store.put(&key, value);
        let original = snapshot(&store);
        assert!(migrate(&store).is_err());
        assert_eq!(snapshot(&store), original);
    }
}

#[test]
fn test_preflight_checks_every_page_before_any_writes() {
    let (_dir, store) = database();
    let mut keys = Vec::new();
    for seed in 0..205 {
        let channel = sample(seed);
        let key = actor_key(&channel.id);
        store.put(&key, bincode::serialize(&released_v2(channel)).unwrap());
        keys.push(key);
    }
    keys.sort();
    let key = keys.last().unwrap();
    let (mut last, _): (OldChannelActorData, Option<ChannelSessionV2>) =
        decode_exact(&store.get(key).unwrap()).unwrap();
    last.state = OldChannelState::ChannelReady;
    store.put(key, bincode::serialize(&last).unwrap());
    let original = snapshot(&store);
    assert!(migrate(&store).is_err());
    assert_eq!(snapshot(&store), original);
}

#[test]
fn test_user_cancellation_preserves_the_old_database() {
    let (_dir, store) = database();
    let channel = sample(42);
    store.put(
        actor_key(&channel.id),
        bincode::serialize(&released_v2(channel)).unwrap(),
    );
    let original = snapshot(&store);
    let mut migrations = DbMigrate::new();
    crate::migrations::register_all_migrations(&mut migrations);
    let result = migrations.auto_migrate(&store, Box::new(|_| false), Box::new(|_| {}));
    assert!(matches!(
        result,
        Err(crate::migration::MigrateError::UserCancelled)
    ));
    assert_eq!(snapshot(&store), original);
}

#[test]
fn test_full_hash_v1_is_not_mistaken_for_v2() {
    for appended_none in [false, true] {
        let (_dir, store) = database();
        let mut channel = legacy(65, OldChannelState::ChannelReady);
        channel.commitment_contract_features =
            fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH;
        let key = actor_key(&channel.id);
        let value = if appended_none {
            bincode::serialize(&(channel, Option::<ChannelSessionV2>::None)).unwrap()
        } else {
            bincode::serialize(&channel).unwrap()
        };
        store.put(key, value);
        let before = snapshot(&store);
        let error = migrate(&store).unwrap_err().to_string();
        assert!(error.contains("old binary"), "{error}");
        assert_eq!(snapshot(&store), before);
    }
}

#[test]
fn test_unfinished_opening_without_a_v2_session_is_rejected() {
    for features in [
        fiber_types_0100::CommitmentContractFeatures::LEGACY,
        fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    ] {
        let (_dir, store) = database();
        let mut record = OldChannelOpenRecord::samples(66).remove(0);
        record.commitment_contract_features = features;
        record.status = OldChannelOpeningStatus::WaitingForPeer;
        let key = [&[CHANNEL_OPEN_RECORD_PREFIX], record.channel_id.as_ref()].concat();
        store.put(key, bincode::serialize(&record).unwrap());
        let before = snapshot(&store);
        assert!(migrate(&store).is_err());
        assert_eq!(snapshot(&store), before);
    }
}

#[test]
fn test_legacy_watchtower_obligation_blocks_all_writes() {
    let (_dir, store) = database();
    let channel = sample(67);
    store.put(
        actor_key(&channel.id),
        bincode::serialize(&released_v2(channel)).unwrap(),
    );
    let mut record = super::OldChannelData::samples(68).remove(0);
    record.commitment_contract_features = fiber_types_0100::CommitmentContractFeatures::LEGACY;
    let key = [
        &[fiber_types::schema::WATCHTOWER_CHANNEL_PREFIX],
        record.channel_id.as_ref(),
    ]
    .concat();
    store.put(key, bincode::serialize(&record).unwrap());
    let before = snapshot(&store);
    let error = migrate(&store).unwrap_err().to_string();
    assert!(error.contains("watchtower"), "{error}");
    assert_eq!(snapshot(&store), before);
}

#[test]
fn test_full_hash_watchtower_record_retains_its_exact_bytes() {
    let (_dir, store) = database();
    let mut record = super::OldChannelData::samples(69).remove(0);
    record.commitment_contract_features =
        fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH;
    let key = [
        &[fiber_types::schema::WATCHTOWER_CHANNEL_PREFIX],
        record.channel_id.as_ref(),
    ]
    .concat();
    let encoded = bincode::serialize(&record).unwrap();
    store.put(&key, &encoded);
    migrate(&store).unwrap();
    assert_eq!(store.get(&key).unwrap(), encoded);
    let _: fiber_types::ChannelData = decode_exact(&encoded).unwrap();
}

#[test]
fn test_archive_conflict_is_rejected_without_writes() {
    let (_dir, store) = database();
    let channel = sample(70);
    store.put(
        actor_key(&channel.id),
        bincode::serialize(&released_v2(channel)).unwrap(),
    );
    let v1 = legacy(71, OldChannelState::Closed(OldCloseFlags::COOPERATIVE));
    let key = actor_key(&v1.id);
    store.put(&key, bincode::serialize(&v1).unwrap());
    store.put(archive_key(&key), b"conflicting original bytes");
    let before = snapshot(&store);
    assert!(migrate(&store).is_err());
    assert_eq!(snapshot(&store), before);
}
