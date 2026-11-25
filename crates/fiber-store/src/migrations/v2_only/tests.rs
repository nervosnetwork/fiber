use ckb_types::prelude::Entity;
use fiber_types::channel_v2_validation::decode_and_validate_channel_actor_data_parts;
use fiber_types::sample::StoreSample;
use fiber_types::schema::{
    CHANNEL_IMMUTABLE_STATE_PREFIX, CHANNEL_OPEN_RECORD_PREFIX, CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX,
    CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX, PUBKEY_CHANNEL_ID_PREFIX, WATCHTOWER_CHANNEL_PREFIX,
    WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX,
};
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

#[path = "payment_tests.rs"]
mod payment_tests;
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
    // Unit tests of this migration only: later migrations change the stored layout.
    let mut migrations = DbMigrate::new();
    migrations.add_migration(std::sync::Arc::new(super::MigrationObj::new()));
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

fn local_watchtower_pair(seed: u64) -> (OldChannelActorData, super::OldChannelData) {
    let mut channel = legacy(
        seed,
        OldChannelState::Closed(OldCloseFlags::UNCOOPERATIVE_LOCAL),
    );
    let mut watch = super::OldChannelData::samples(seed).remove(0);
    watch.channel_id = channel.id;
    watch.local_funding_pubkey = channel.signer.funding_key.pubkey();
    watch.commitment_contract_features = fiber_types_0100::CommitmentContractFeatures::LEGACY;
    let mut remote_keys = channel.local_channel_public_keys.clone();
    remote_keys.funding_pubkey = watch.remote_funding_pubkey;
    channel.remote_channel_public_keys = Some(remote_keys);
    assert_ne!(watch.local_funding_pubkey, watch.remote_funding_pubkey);
    (channel, watch)
}

/// Assert the channel is stored as a mutable and an immutable record that merge back to `expected`.
fn assert_split_channel(store: &crate::Store, expected: &ChannelActorData) -> ChannelActorData {
    let mutable = store
        .get(actor_key(&expected.id))
        .expect("mutable channel record");
    let immutable = store
        .get([&[CHANNEL_IMMUTABLE_STATE_PREFIX], expected.id.as_ref()].concat())
        .expect("immutable channel record");
    let restored = decode_and_validate_channel_actor_data_parts(&expected.id, &mutable, &immutable)
        .expect("valid split channel record");
    assert_eq!(
        bincode::serialize(&restored).unwrap(),
        bincode::serialize(expected).unwrap()
    );
    restored
}

fn snapshot(store: &crate::Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    store.prefix_iterator(Vec::new()).collect()
}

/// Snapshot without the version key, which `auto_migrate` stamps once nothing is
/// pending for this migration alone.
fn records(store: &crate::Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    snapshot(store)
        .into_iter()
        .filter(|(key, _)| key.as_slice() != MIGRATION_VERSION_KEY)
        .collect()
}

#[test]
fn test_released_v2_migrates_without_changing_signing_state() {
    let (_dir, store) = database();
    let channel = sample(41);
    let key = actor_key(&channel.id);
    let encoded = bincode::serialize(&released_v2(channel.clone())).unwrap();
    store.put(&key, &encoded);
    migrate(&store).unwrap();
    let restored = assert_split_channel(&store, &channel);
    assert_eq!(
        bincode::serialize(&restored.session).unwrap(),
        bincode::serialize(&channel.session).unwrap()
    );
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        MIGRATION_DB_VERSION.as_bytes()
    );
    // Mutable block, immutable block and the version key.
    assert_eq!(snapshot(&store).len(), 3);
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
    assert_split_channel(&store, &current);
    assert_split_channel(&store, &old);
    assert_eq!(store.get(archive_key(&open_key)).unwrap(), encoded);
    let completed = records(&store);
    migrate(&store).unwrap();
    assert_eq!(records(&store), completed);
}

#[test]
fn test_already_split_records_are_validated_and_skipped() {
    let (_dir, store) = database();
    // One channel is already split (a resumed run), the other still holds a released record.
    let split = sample(20);
    let (immutable, mutable) = split.clone().into_parts();
    store.put(actor_key(&split.id), bincode::serialize(&mutable).unwrap());
    store.put(
        [&[CHANNEL_IMMUTABLE_STATE_PREFIX], split.id.as_ref()].concat(),
        bincode::serialize(&immutable).unwrap(),
    );
    let pending = sample(21);
    store.put(
        actor_key(&pending.id),
        bincode::serialize(&released_v2(pending.clone())).unwrap(),
    );

    migrate(&store).unwrap();
    assert_split_channel(&store, &split);
    assert_split_channel(&store, &pending);
}

#[test]
fn test_released_v2_with_an_unusable_session_is_never_repaired() {
    let (_dir, store) = database();
    let channel = sample(79);
    let id = channel.id;
    // A valid key, just not the one that signed this nonce.
    let foreign_owner = channel.remote_pubkey;
    let (old, session) = released_v2(channel);
    let (marker, mut session) = session.unwrap();
    session.own.owner = foreign_owner;
    store.put(
        actor_key(&id),
        bincode::serialize(&(old, Some((marker, session)))).unwrap(),
    );

    let original = snapshot(&store);
    assert!(migrate(&store).is_err());
    assert_eq!(snapshot(&store), original);
}

#[test]
fn test_released_v2_with_a_corrupted_marker_is_refused() {
    let (_dir, store) = database();
    let channel = sample(80);
    let id = channel.id;
    let (old, session) = released_v2(channel);
    let (marker, session) = session.unwrap();
    store.put(
        actor_key(&id),
        bincode::serialize(&(old, Some((marker ^ 1, session)))).unwrap(),
    );

    let original = snapshot(&store);
    let error = migrate(&store).unwrap_err().to_string();
    // The released layout is recognised, so its own error is reported instead of "trailing bytes".
    assert!(error.contains("session marker"), "{error}");
    assert_eq!(snapshot(&store), original);
}

#[test]
fn test_active_channel_and_its_opening_record_survive_the_split() {
    let (_dir, store) = database();
    let channel = sample(81);
    let id = channel.id;
    store.put(
        actor_key(&id),
        bincode::serialize(&released_v2(channel.clone())).unwrap(),
    );
    // An established channel keeps its opening record, and the migration converts the actor first;
    // reading that record afterwards must not go back to the whole-record layout.
    let opening = OldChannelOpenRecord {
        channel_id: decode_as_new(id).unwrap(),
        pubkey: decode_as_new(channel.remote_pubkey).unwrap(),
        is_acceptor: false,
        status: OldChannelOpeningStatus::ChannelReady,
        funding_amount: 1,
        failure_detail: None,
        created_at: 1,
        last_updated_at: 1,
        // A V2 channel, so the released record carries the V2 feature byte.
        commitment_contract_features:
            fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    };
    let open_key = [&[CHANNEL_OPEN_RECORD_PREFIX], id.as_ref()].concat();
    store.put(open_key.clone(), bincode::serialize(&opening).unwrap());

    migrate(&store).unwrap();

    assert_split_channel(&store, &channel);
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        MIGRATION_DB_VERSION.as_bytes()
    );
    assert!(store.get(&open_key).is_some(), "the opening record is kept");
    // A second run has nothing left to do.
    let completed = snapshot(&store);
    migrate(&store).unwrap();
    assert_eq!(snapshot(&store), completed);
}

#[test]
fn test_truncated_v2_is_never_repaired() {
    let (_dir, store) = database();
    let channel = sample(77);
    let stored = released_v2(channel);
    let key = actor_key(&stored.0.id);
    let mut value = bincode::serialize(&stored).unwrap();
    value.pop();
    store.put(&key, value);
    let original = snapshot(&store);
    assert!(migrate(&store).is_err());
    assert_eq!(snapshot(&store), original);
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
    let (mut last, _): (OldChannelActorData, Option<(u32, super::ReleasedChannelSessionV2Wire)>) =
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
fn test_unfunded_orphan_openings_are_archived_verbatim() {
    for is_acceptor in [false, true] {
        for features in [
            fiber_types_0100::CommitmentContractFeatures::LEGACY,
            fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
        ] {
            let (_dir, store) = database();
            let mut record = OldChannelOpenRecord::samples(66).remove(0);
            record.commitment_contract_features = features;
            record.status = OldChannelOpeningStatus::WaitingForPeer;
            record.is_acceptor = is_acceptor;
            record.funding_amount = 100_000_000_000; // Requested amount, not locked funds.
            let key = [&[CHANNEL_OPEN_RECORD_PREFIX], record.channel_id.as_ref()].concat();
            let encoded = bincode::serialize(&record).unwrap();
            store.put(&key, &encoded);
            migrate(&store).unwrap();
            assert!(store.get(&key).is_none());
            assert_eq!(store.get(archive_key(&key)).unwrap(), encoded);
        }
    }
}

#[test]
fn test_orphan_openings_past_waiting_for_peer_still_block() {
    for status in [
        OldChannelOpeningStatus::FundingTxBuilding,
        OldChannelOpeningStatus::FundingTxBroadcasted,
        OldChannelOpeningStatus::ChannelReady,
    ] {
        let (_dir, store) = database();
        let mut record = OldChannelOpenRecord::samples(66).remove(0);
        record.status = status;
        store.put(
            [&[CHANNEL_OPEN_RECORD_PREFIX], record.channel_id.as_ref()].concat(),
            bincode::serialize(&record).unwrap(),
        );
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
fn test_full_hash_watchtower_record_retains_keys_and_v2_witness() {
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
    let converted: fiber_types::ChannelData = decode_exact(&store.get(&key).unwrap()).unwrap();
    assert_eq!(converted.channel_features, fiber_types::ChannelFeatures::V2);
    assert_eq!(
        converted.local_settlement_key.unwrap().as_ref(),
        record.local_settlement_key.as_ref()
    );
    for (old, new) in record
        .local_settlement_data
        .tlcs
        .iter()
        .zip(&converted.local_settlement_data.tlcs)
    {
        assert_eq!(
            new.local_key.as_ref().unwrap().as_ref(),
            old.local_key.as_ref()
        );
        assert_eq!(
            new.local_key_pubkey.unwrap(),
            new.local_key.as_ref().unwrap().pubkey()
        );
        assert_eq!(new.local_key_commitment_number, None);
        assert_eq!(new.payment_hash.as_ref(), old.payment_hash.as_ref());
    }
    let before = snapshot(&store);
    migrate(&store).unwrap();
    assert_eq!(snapshot(&store), before, "migration must be idempotent");
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

#[test]
fn test_closed_local_v1_watchtower_history_is_archived_and_resumes() {
    for archived_actor in [false, true] {
        let (_dir, store) = database();
        let (channel, watch) = local_watchtower_pair(80);
        let channel_key = actor_key(&channel.id);
        let watch_key = [&[WATCHTOWER_CHANNEL_PREFIX], channel.id.as_ref()].concat();
        let superseded_key = [
            &[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
            channel.id.as_ref(),
        ]
        .concat();
        let channel_bytes = bincode::serialize(&channel).unwrap();
        let watch_bytes = bincode::serialize(&watch).unwrap();
        let superseded =
            bincode::serialize(&vec![watch.pending_remote_settlement_data.clone()]).unwrap();
        store.put(
            if archived_actor {
                archive_key(&channel_key)
            } else {
                channel_key.clone()
            },
            &channel_bytes,
        );
        store.put(&watch_key, &watch_bytes);
        store.put(&superseded_key, &superseded);
        migrate(&store).unwrap();
        for (key, bytes) in [
            (&channel_key, &channel_bytes),
            (&watch_key, &watch_bytes),
            (&superseded_key, &superseded),
        ] {
            assert!(store.get(key).is_none());
            assert_eq!(store.get(archive_key(key)).unwrap(), *bytes);
        }
        let completed = records(&store);
        migrate(&store).unwrap();
        assert_eq!(records(&store), completed);
    }
}

#[test]
fn test_local_watchtower_archive_conflict_preserves_all_records() {
    for prefix in [
        WATCHTOWER_CHANNEL_PREFIX,
        WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX,
    ] {
        let (_dir, store) = database();
        let (channel, watch) = local_watchtower_pair(87);
        store.put(
            actor_key(&channel.id),
            bincode::serialize(&channel).unwrap(),
        );
        store.put(
            [&[WATCHTOWER_CHANNEL_PREFIX], channel.id.as_ref()].concat(),
            bincode::serialize(&watch).unwrap(),
        );
        store.put(
            [
                &[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
                channel.id.as_ref(),
            ]
            .concat(),
            b"retained snapshot",
        );
        let conflicting_key = [&[prefix], channel.id.as_ref()].concat();
        store.put(archive_key(&conflicting_key), b"conflicting archived bytes");
        let before = snapshot(&store);
        assert!(migrate(&store)
            .unwrap_err()
            .to_string()
            .contains("Conflicting V1 archive"));
        assert_eq!(snapshot(&store), before);
    }
}

#[test]
fn test_local_v1_watchtower_requires_final_state_and_matching_funding_keys() {
    for scenario in [
        "waiting",
        "confirmed",
        "delivery",
        "retry",
        "missing actor",
        "local key",
        "remote key",
    ] {
        let (_dir, store) = database();
        let v2 = sample(81);
        store.put(
            actor_key(&v2.id),
            bincode::serialize(&released_v2(v2)).unwrap(),
        );
        let (mut channel, mut watch) = local_watchtower_pair(82);
        match scenario {
            "waiting" => {
                channel.state = OldChannelState::Closed(
                    OldCloseFlags::UNCOOPERATIVE_LOCAL | OldCloseFlags::WAITING_ONCHAIN_SETTLEMENT,
                )
            }
            "confirmed" => {
                channel.state = OldChannelState::Closed(
                    OldCloseFlags::UNCOOPERATIVE_LOCAL
                        | OldCloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
                )
            }
            "delivery" => {
                channel
                    .waiting_forward_tlc_tasks
                    .insert(fiber_types_0100::TLCId::Received(0), [1; 32]);
            }
            "retry" => channel.retryable_tlc_operations.push_back(
                fiber_types_0100::RetryableTlcOperation::RemoveTlc(
                    fiber_types_0100::TLCId::Received(0),
                    fiber_types_0100::RemoveTlcReason::RemoveTlcFulfill(
                        fiber_types_0100::RemoveTlcFulfill {
                            payment_preimage: [1; 32].into(),
                        },
                    ),
                ),
            ),
            "local key" => watch.local_funding_pubkey = watch.remote_funding_pubkey,
            "remote key" => watch.remote_funding_pubkey = watch.local_funding_pubkey,
            "missing actor" => {}
            _ => unreachable!(),
        }
        if scenario != "missing actor" {
            store.put(
                actor_key(&channel.id),
                bincode::serialize(&channel).unwrap(),
            );
        }
        store.put(
            [&[WATCHTOWER_CHANNEL_PREFIX], channel.id.as_ref()].concat(),
            bincode::serialize(&watch).unwrap(),
        );
        let before = snapshot(&store);
        assert!(migrate(&store).is_err(), "{scenario}");
        assert_eq!(snapshot(&store), before, "{scenario}");
    }
}

#[test]
fn test_third_party_legacy_watchtower_cannot_borrow_a_closed_local_state() {
    let (_dir, store) = database();
    let (channel, watch) = local_watchtower_pair(83);
    store.put(
        actor_key(&channel.id),
        bincode::serialize(&channel).unwrap(),
    );
    let client = [7; 33];
    let key = [
        &[WATCHTOWER_CHANNEL_PREFIX],
        client.as_slice(),
        channel.id.as_ref(),
    ]
    .concat();
    store.put(key, bincode::serialize(&watch).unwrap());
    let before = snapshot(&store);
    let error = migrate(&store).unwrap_err().to_string();
    assert!(
        error.contains("Unsupported legacy third-party watchtower"),
        "{error}"
    );
    assert!(error.contains(&channel.id.to_string()), "{error}");
    assert_eq!(snapshot(&store), before);
}

#[test]
fn test_waiting_orphan_with_funding_evidence_is_not_archived() {
    for evidence in [
        "outpoint",
        "snapshot",
        "commit diff",
        "pubkey index",
        "different pubkey index",
        "watchtower",
        "superseded",
    ] {
        let (_dir, store) = database();
        let mut record = OldChannelOpenRecord::samples(84).remove(0);
        record.status = OldChannelOpeningStatus::WaitingForPeer;
        store.put(
            [&[CHANNEL_OPEN_RECORD_PREFIX], record.channel_id.as_ref()].concat(),
            bincode::serialize(&record).unwrap(),
        );
        let (key, value) = match evidence {
            "outpoint" => (
                [
                    &[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX],
                    ckb_types::packed::OutPoint::default().as_slice(),
                ]
                .concat(),
                bincode::serialize(&record.channel_id).unwrap(),
            ),
            "snapshot" => (
                [
                    &[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX],
                    record.channel_id.as_ref(),
                ]
                .concat(),
                b"retained settlement snapshot".to_vec(),
            ),
            "superseded" => (
                [
                    &[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
                    record.channel_id.as_ref(),
                ]
                .concat(),
                b"retained superseded snapshot".to_vec(),
            ),
            "commit diff" => (
                [
                    &[super::PENDING_COMMIT_DIFF_PREFIX],
                    record.channel_id.as_ref(),
                ]
                .concat(),
                b"retained signed replay intent".to_vec(),
            ),
            "pubkey index" | "different pubkey index" => {
                let pubkey = if evidence == "different pubkey index" {
                    let pubkey = super::OldChannelData::samples(85)
                        .remove(0)
                        .local_funding_pubkey;
                    assert_ne!(pubkey, record.pubkey);
                    pubkey
                } else {
                    record.pubkey
                };
                (
                    [
                        &[PUBKEY_CHANNEL_ID_PREFIX],
                        pubkey.serialize().as_slice(),
                        record.channel_id.as_ref(),
                    ]
                    .concat(),
                    bincode::serialize(&OldChannelState::ChannelReady).unwrap(),
                )
            }
            "watchtower" => {
                let mut watch = super::OldChannelData::samples(84).remove(0);
                watch.channel_id = record.channel_id;
                watch.commitment_contract_features =
                    fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH;
                (
                    [&[WATCHTOWER_CHANNEL_PREFIX], record.channel_id.as_ref()].concat(),
                    bincode::serialize(&watch).unwrap(),
                )
            }
            _ => unreachable!(),
        };
        store.put(key, value);
        let before = snapshot(&store);
        assert!(migrate(&store).is_err(), "{evidence}");
        assert_eq!(snapshot(&store), before, "{evidence}");
    }
}

#[test]
fn test_funding_evidence_for_another_channel_does_not_block_orphan_retirement() {
    let (_dir, store) = database();
    let mut record = OldChannelOpenRecord::samples(85).remove(0);
    record.status = OldChannelOpeningStatus::WaitingForPeer;
    let key = [&[CHANNEL_OPEN_RECORD_PREFIX], record.channel_id.as_ref()].concat();
    let encoded = bincode::serialize(&record).unwrap();
    store.put(&key, &encoded);
    let other = sample(86);
    store.put(
        actor_key(&other.id),
        bincode::serialize(&released_v2(other.clone())).unwrap(),
    );
    store.put(
        [
            &[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX],
            ckb_types::packed::OutPoint::default().as_slice(),
        ]
        .concat(),
        bincode::serialize(&other.id).unwrap(),
    );
    migrate(&store).unwrap();
    assert!(store.get(&key).is_none());
    assert_eq!(store.get(archive_key(&key)).unwrap(), encoded);
    assert_split_channel(&store, &other);
}

#[test]
fn test_released_v2_channel_with_unresolved_tlc_and_remove_effects_migrates() {
    let (_dir, store) = database();
    let mut channel = sample(87);

    // Choose a valid secp256k1 private key whose first byte is 0x02 so decoding it as
    // head's `Option<Privkey>` fails with an invalid Option tag if released decoding regresses.
    let raw_key = [0x02u8; 32];
    let local_key = fiber_types::Privkey::from(&raw_key);
    let remote_key = fiber_types::Privkey::from(&[0x03u8; 32]).pubkey();
    let tlc = fiber_types::SettlementTlc {
        tlc_id: fiber_types::TLCId::Offered(7),
        hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
        payment_amount: 50_000,
        payment_hash: fiber_types::Hash256::from([0x11; 32]),
        expiry: 1_700_000_000_000,
        local_key: Some(local_key.clone()),
        local_key_pubkey: Some(local_key.pubkey()),
        local_key_commitment_number: None,
        remote_key,
    };
    let settlement = fiber_types::SettlementData {
        local_amount: 100_000,
        remote_amount: 200_000,
        tlcs: vec![tlc],
    };
    channel.session.bootstrap_settlement = Some(settlement.clone());
    let mut old_tlc = OldChannelActorData::samples(89)
        .remove(1)
        .tlc_state
        .offered_tlcs
        .tlcs
        .remove(0);
    old_tlc.amount = 50_000;
    let current_tlc: fiber_types::TlcInfo = super::convert_named(old_tlc).unwrap();
    assert_eq!(current_tlc.upstream_tlc_key, None);
    channel.session.remove_effects = vec![(current_tlc, Some(1))];

    let key = actor_key(&channel.id);
    let encoded = bincode::serialize(&released_v2(channel.clone())).unwrap();
    // Prove the released bytes cannot be decoded directly as head's ChannelSessionV2.
    assert!(
        decode_exact::<(OldChannelActorData, Option<(u32, ChannelSessionV2)>)>(&encoded).is_err(),
        "released v0.10.1 session with 0x02-prefixed TLC key must differ from head layout"
    );
    store.put(&key, &encoded);

    migrate(&store).unwrap();
    let restored = assert_split_channel(&store, &channel);
    let restored_tlc = &restored
        .session
        .bootstrap_settlement
        .as_ref()
        .unwrap()
        .tlcs[0];
    assert_eq!(restored_tlc.local_key.as_ref(), Some(&local_key));
    assert_eq!(restored_tlc.local_key_pubkey, Some(local_key.pubkey()));
    assert_eq!(restored_tlc.local_key_commitment_number, None);
    assert_eq!(restored.session.remove_effects.len(), 1);
    assert_eq!(restored.session.remove_effects[0].0.upstream_tlc_key, None);

    // Also verify outgoing and incoming commitment snapshots carrying unresolved TLCs round-trip.
    let mut full_session = channel.session.clone();
    full_session.outgoing = Some(fiber_types::OutgoingCommitmentV2 {
        peer_wait_started_at: 123,
        request: Default::default(),
        transaction: Default::default(),
        settlement: settlement.clone(),
        updates: vec![],
        funding: channel.session.own.clone(),
        revocation: None,
        revocation_context: None,
    });
    full_session.incoming = Some(fiber_types::IncomingCommitmentV2 {
        request: Default::default(),
        funding: channel.session.own.clone(),
        revocation: None,
        revocation_context: None,
        transaction: Default::default(),
        settlement: settlement.clone(),
        response: None,
    });
    let mut full_channel = channel.clone();
    full_channel.session = full_session.clone();
    let full_released = released_v2(full_channel);
    let full_encoded = bincode::serialize(&full_released).unwrap();
    assert!(
        decode_exact::<(OldChannelActorData, Option<(u32, ChannelSessionV2)>)>(&full_encoded)
            .is_err()
    );
    let (_, decoded_session) = super::released_channel(&full_encoded).unwrap();
    let decoded_session = decoded_session.expect("decoded V2 session");
    assert_eq!(
        decoded_session.outgoing.as_ref().unwrap().settlement,
        settlement
    );
    assert_eq!(
        decoded_session.incoming.as_ref().unwrap().settlement,
        settlement
    );
}

#[test]
fn test_released_shutdown_settlement_record_migrates_and_resumes() {
    let (_dir, store) = database();
    let remote_key = fiber_types_0100::Privkey::from(&[0x03u8; 32]).pubkey();
    // First byte 0x02 is an invalid Option tag in bincode, so head's ShutdownSettlementRecord
    // cannot decode this released v0.10.1 record unless the migration converts it.
    let raw_key = [0x02u8; 32];
    let old_local_key = fiber_types_0100::Privkey::from(&raw_key);
    let expected_local_key = fiber_types::Privkey::from(&raw_key);
    let old_tlc = fiber_types_0100::SettlementTlc {
        tlc_id: fiber_types_0100::TLCId::Offered(11),
        hash_algorithm: fiber_types_0100::HashAlgorithm::CkbHash,
        payment_amount: 42_000,
        payment_hash: fiber_types_0100::Hash256::from([0x44; 32]),
        expiry: 1_700_000_000_123,
        local_key: old_local_key,
        remote_key,
    };

    // 1. Released v0.10.1 prefix-49 record with a pending TLC (needs conversion).
    let pending_channel = sample(91);
    let pending_key = [
        &[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX],
        pending_channel.id.as_ref(),
    ]
    .concat();
    let old_pending = fiber_types_0100::ShutdownSettlementRecord {
        shutdown_tx_hash: ckb_types::H256::from([0xaa; 32]),
        for_remote: true,
        commitment_number: 7,
        settlement_data: fiber_types_0100::SettlementData {
            local_amount: 100_000,
            remote_amount: 200_000,
            tlcs: vec![old_tlc.clone()],
        },
    };
    let old_pending_bytes = bincode::serialize(&old_pending).unwrap();
    assert!(
        decode_exact::<fiber_types::ShutdownSettlementRecord>(&old_pending_bytes).is_err(),
        "released v0.10.1 ShutdownSettlementRecord with 0x02-prefixed TLC key must not decode as head"
    );
    store.put(
        actor_key(&pending_channel.id),
        bincode::serialize(&released_v2(pending_channel.clone())).unwrap(),
    );
    store.put(&pending_key, &old_pending_bytes);

    // 2. Released v0.10.1 prefix-49 record with empty TLCs (identical binary layout).
    let empty_channel_id = fiber_types::Hash256::from([0x55; 32]);
    let empty_key = [
        &[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX],
        empty_channel_id.as_ref(),
    ]
    .concat();
    let old_empty = fiber_types_0100::ShutdownSettlementRecord {
        shutdown_tx_hash: ckb_types::H256::from([0xbb; 32]),
        for_remote: false,
        commitment_number: 3,
        settlement_data: fiber_types_0100::SettlementData {
            local_amount: 50_000,
            remote_amount: 60_000,
            tlcs: vec![],
        },
    };
    let old_empty_bytes = bincode::serialize(&old_empty).unwrap();
    store.put(&empty_key, &old_empty_bytes);

    // 3. Already-converted prefix-49 record with a pending TLC (simulating interrupted resume).
    let resumed_channel_id = fiber_types::Hash256::from([0x66; 32]);
    let resumed_key = [
        &[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX],
        resumed_channel_id.as_ref(),
    ]
    .concat();
    let already_converted = fiber_types::ShutdownSettlementRecord {
        shutdown_tx_hash: ckb_types::H256::from([0xcc; 32]),
        for_remote: true,
        commitment_number: 9,
        settlement_data: fiber_types::SettlementData {
            local_amount: 70_000,
            remote_amount: 80_000,
            tlcs: vec![fiber_types::SettlementTlc {
                tlc_id: fiber_types::TLCId::Received(5),
                hash_algorithm: fiber_types::HashAlgorithm::Sha256,
                payment_amount: 12_345,
                payment_hash: fiber_types::Hash256::from([0x77; 32]),
                expiry: 1_700_000_000_456,
                local_key: Some(expected_local_key.clone()),
                local_key_pubkey: Some(expected_local_key.pubkey()),
                local_key_commitment_number: None,
                remote_key: decode_as_new(remote_key).unwrap(),
            }],
        },
    };
    let already_converted_bytes = bincode::serialize(&already_converted).unwrap();
    store.put(&resumed_key, &already_converted_bytes);

    // 4. Safely closed V1 channel with a prefix-49 record: archived verbatim, not converted.
    let closed_v1 = legacy(92, OldChannelState::Closed(OldCloseFlags::UNCOOPERATIVE_LOCAL));
    let v1_p49_key = [&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX], closed_v1.id.as_ref()].concat();
    store.put(actor_key(&closed_v1.id), bincode::serialize(&closed_v1).unwrap());
    store.put(&v1_p49_key, &old_pending_bytes);

    migrate(&store).expect("shutdown settlement records must migrate and resume cleanly");
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        MIGRATION_DB_VERSION.as_bytes()
    );

    // Verify converted pending-TLC record.
    let restored_pending: fiber_types::ShutdownSettlementRecord =
        decode_exact(&store.get(&pending_key).unwrap()).unwrap();
    assert_eq!(restored_pending.shutdown_tx_hash, old_pending.shutdown_tx_hash);
    assert_eq!(restored_pending.for_remote, old_pending.for_remote);
    assert_eq!(restored_pending.commitment_number, old_pending.commitment_number);
    assert_eq!(restored_pending.settlement_data.local_amount, 100_000);
    assert_eq!(restored_pending.settlement_data.remote_amount, 200_000);
    assert_eq!(restored_pending.settlement_data.tlcs.len(), 1);
    let restored_tlc = &restored_pending.settlement_data.tlcs[0];
    assert_eq!(restored_tlc.tlc_id, fiber_types::TLCId::Offered(11));
    assert_eq!(restored_tlc.hash_algorithm, fiber_types::HashAlgorithm::CkbHash);
    assert_eq!(restored_tlc.payment_amount, 42_000);
    assert_eq!(restored_tlc.payment_hash, fiber_types::Hash256::from([0x44; 32]));
    assert_eq!(restored_tlc.expiry, 1_700_000_000_123);
    assert_eq!(restored_tlc.local_key.as_ref(), Some(&expected_local_key));
    assert_eq!(restored_tlc.local_key_pubkey, Some(expected_local_key.pubkey()));
    assert_eq!(restored_tlc.local_key_commitment_number, None);
    assert_eq!(restored_tlc.remote_key, decode_as_new(remote_key).unwrap());

    // Verify empty-TLC record.
    let restored_empty: fiber_types::ShutdownSettlementRecord =
        decode_exact(&store.get(&empty_key).unwrap()).unwrap();
    assert_eq!(restored_empty.shutdown_tx_hash, old_empty.shutdown_tx_hash);
    assert!(!restored_empty.for_remote);
    assert_eq!(restored_empty.commitment_number, 3);
    assert_eq!(restored_empty.settlement_data.local_amount, 50_000);
    assert_eq!(restored_empty.settlement_data.remote_amount, 60_000);
    assert!(restored_empty.settlement_data.tlcs.is_empty());

    // Verify already-converted resumed record was preserved verbatim.
    assert_eq!(store.get(&resumed_key).unwrap(), already_converted_bytes);

    // Verify closed V1 channel's prefix-49 record was archived verbatim.
    assert!(store.get(&v1_p49_key).is_none());
    assert_eq!(store.get(archive_key(&v1_p49_key)).unwrap(), old_pending_bytes);

    // Verify idempotency on a second run.
    let completed = snapshot(&store);
    migrate(&store).unwrap();
    assert_eq!(snapshot(&store), completed);
}

#[test]
fn test_corrupted_shutdown_settlement_record_rejects_without_writes() {
    let (_dir, store) = database();
    let channel = sample(93);
    store.put(
        actor_key(&channel.id),
        bincode::serialize(&released_v2(channel.clone())).unwrap(),
    );
    let key = [&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX], channel.id.as_ref()].concat();
    let old = fiber_types_0100::ShutdownSettlementRecord {
        shutdown_tx_hash: ckb_types::H256::from([0xdd; 32]),
        for_remote: false,
        commitment_number: 1,
        settlement_data: fiber_types_0100::SettlementData {
            local_amount: 10,
            remote_amount: 20,
            tlcs: vec![],
        },
    };
    let mut suffixed = bincode::serialize(&old).unwrap();
    suffixed.push(0);
    store.put(&key, &suffixed);

    let before = snapshot(&store);
    assert!(migrate(&store).is_err());
    assert_eq!(snapshot(&store), before);
}
