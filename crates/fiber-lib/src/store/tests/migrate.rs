use fiber_store::backend::StorageBackend;
use fiber_store::db_migrate::DbMigrate;
use fiber_store::migration::{
    MigrateError, Migration, Migrations, INIT_DB_VERSION, LATEST_DB_VERSION, MIGRATION_VERSION_KEY,
};
use fiber_store::migrations::decode_as_new;
use fiber_types_0100::ChannelActorData as OldChannelActorData;
use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

fn gen_path() -> std::path::PathBuf {
    let tmp_dir = tempfile::Builder::new()
        .prefix("test-store")
        .tempdir()
        .unwrap();
    tmp_dir.as_ref().to_path_buf()
}

fn gen_store() -> fiber_store::Store {
    let path = gen_path();
    fiber_store::Store::open_db(&path).unwrap()
}

// This real-channel fixture has no unsettled TLCs in its signed snapshots. The
// store migration's own tests cover the released wire layout with pending TLCs.
fn released_v2_without_pending_tlcs(channel: fiber_types::ChannelActorData) -> Vec<u8> {
    const RELEASED_SESSION_MARKER: u32 = 0x56320001;
    let session = channel.session.clone();
    assert!(session
        .outgoing
        .as_ref()
        .is_none_or(|outgoing| outgoing.settlement.tlcs.is_empty()));
    assert!(session
        .incoming
        .as_ref()
        .is_none_or(|incoming| incoming.settlement.tlcs.is_empty()));
    assert!(session
        .pending_incoming
        .as_ref()
        .is_none_or(|incoming| incoming.settlement.tlcs.is_empty()));
    assert!(session
        .revocation_effect
        .as_ref()
        .is_none_or(|(_, settlement)| settlement.tlcs.is_empty()));
    assert!(session
        .bootstrap_settlement
        .as_ref()
        .is_none_or(|settlement| settlement.tlcs.is_empty()));
    let latest_commitment_transaction = channel.latest_commitment_transaction();
    let old = OldChannelActorData {
        state: decode_as_new(channel.state).unwrap(),
        public_channel_info: decode_as_new(channel.public_channel_info).unwrap(),
        local_tlc_info: decode_as_new(channel.local_tlc_info).unwrap(),
        remote_tlc_info: decode_as_new(channel.remote_tlc_info).unwrap(),
        local_pubkey: decode_as_new(channel.local_pubkey).unwrap(),
        remote_pubkey: decode_as_new(channel.remote_pubkey).unwrap(),
        id: decode_as_new(channel.id).unwrap(),
        funding_tx: channel.funding_tx,
        funding_tx_confirmed_at: channel.funding_tx_confirmed_at,
        funding_udt_type_script: channel.funding_udt_type_script,
        latest_commitment_transaction,
        is_acceptor: channel.is_acceptor,
        is_one_way: channel.is_one_way,
        to_local_amount: channel.to_local_amount,
        to_remote_amount: channel.to_remote_amount,
        local_reserved_ckb_amount: channel.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: channel.remote_reserved_ckb_amount,
        commitment_fee_rate: channel.commitment_fee_rate,
        commitment_delay_epoch: channel.commitment_delay_epoch,
        funding_fee_rate: channel.funding_fee_rate,
        signer: decode_as_new(channel.signer.expect("released channel has a local signer"))
            .unwrap(),
        local_channel_public_keys: decode_as_new(channel.local_channel_public_keys).unwrap(),
        commitment_numbers: decode_as_new(channel.commitment_numbers).unwrap(),
        local_constraints: decode_as_new(channel.local_constraints).unwrap(),
        remote_constraints: decode_as_new(channel.remote_constraints).unwrap(),
        tlc_state: serde_json::from_value(serde_json::to_value(channel.tlc_state).unwrap())
            .unwrap(),
        retryable_tlc_operations: serde_json::from_value(
            serde_json::to_value(channel.retryable_tlc_operations).unwrap(),
        )
        .unwrap(),
        waiting_forward_tlc_tasks: decode_as_new(channel.waiting_forward_tlc_tasks).unwrap(),
        remote_shutdown_script: channel.remote_shutdown_script,
        local_shutdown_script: channel.local_shutdown_script,
        remote_commitment_points: decode_as_new(channel.remote_commitment_points).unwrap(),
        remote_channel_public_keys: decode_as_new(channel.remote_channel_public_keys).unwrap(),
        local_shutdown_info: decode_as_new(channel.local_shutdown_info).unwrap(),
        remote_shutdown_info: decode_as_new(channel.remote_shutdown_info).unwrap(),
        shutdown_transaction_hash: channel.shutdown_transaction_hash,
        reestablishing: channel.reestablishing,
        created_at: channel.created_at,
        pending_replay_updates: decode_as_new(channel.pending_replay_updates).unwrap(),
        last_was_revoke: channel.last_was_revoke,
        connectivity_state: decode_as_new(channel.connectivity_state).unwrap(),
        external_funding: decode_as_new(channel.external_funding).unwrap(),
        last_committed_remote_nonce: None,
        remote_revocation_nonce_for_verify: None,
        remote_revocation_nonce_for_send: None,
        remote_revocation_nonce_for_next: None,
        last_revoke_ack_msg: None,
        commitment_contract_features:
            fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    };
    // The released wire ends at closing, before the appended settlement history.
    assert!(session.superseded_settlements.is_empty());
    let suffix = bincode::serialize(&session.superseded_settlements).unwrap();
    let bytes = bincode::serialize(&(old, Some((RELEASED_SESSION_MARKER, session)))).unwrap();
    bytes.strip_suffix(suffix.as_slice()).unwrap().to_vec()
}

#[test]
fn test_new_db_gets_latest_version() {
    let store = gen_store();
    let migrations = Migrations::default();

    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_ok());

    let version = store.get(MIGRATION_VERSION_KEY).unwrap();
    let version_str = String::from_utf8(version).unwrap();
    assert_eq!(version_str, LATEST_DB_VERSION);
}

#[test]
fn test_current_db_no_migration_needed() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);

    let migrations = Migrations::default();
    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_ok());
}

#[test]
fn test_old_db_returns_error() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, "20240101000000");

    let migrations = Migrations::default();
    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));

    assert!(matches!(result, Err(MigrateError::DatabaseTooOld { .. })));
}

#[test]
fn test_newer_db_returns_error() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, "99991231235959");

    let migrations = Migrations::default();
    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));

    assert!(matches!(result, Err(MigrateError::DatabaseTooNew { .. })));
}

pub struct DummyMigration {
    version: String,
    run_count: Arc<RwLock<usize>>,
}

impl DummyMigration {
    pub fn new(version: &str, run_count: Arc<RwLock<usize>>) -> Self {
        Self {
            version: version.to_string(),
            run_count,
        }
    }
}

impl Migration for DummyMigration {
    fn migrate(&self, _store: &dyn fiber_store::migration::MigrationStore) -> Result<(), String> {
        eprintln!("DummyMigration::migrate {} ... ", self.version);
        let mut count = self.run_count.write().unwrap();
        *count += 1;
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}

pub struct BreakChangeMigration {
    version: String,
}

impl BreakChangeMigration {
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
        }
    }
}

impl Migration for BreakChangeMigration {
    fn migrate(&self, _store: &dyn fiber_store::migration::MigrationStore) -> Result<(), String> {
        eprintln!("BreakChangeMigration::migrate {} ... ", self.version);
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn is_break_change(&self) -> bool {
        true
    }
}

#[test]
fn test_run_migration() {
    let run_count = Arc::new(RwLock::new(0));
    let store = gen_store();

    // Initialize with INIT_DB_VERSION
    store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

    let mut migrations = Migrations::default();
    // Add migrations after INIT_DB_VERSION
    let v1 = "20261114000001";
    let v2 = "20261114000002";
    migrations.add_migration(Arc::new(DummyMigration::new(v1, run_count.clone())));
    migrations.add_migration(Arc::new(DummyMigration::new(v2, run_count.clone())));

    let result = migrations.auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_ok());
    assert_eq!(*run_count.read().unwrap(), 2);

    // Verify version was updated to the last migration
    let version = store.get(MIGRATION_VERSION_KEY).unwrap();
    let version_str = String::from_utf8(version).unwrap();
    assert_eq!(version_str, v2);
}

#[test]
fn test_user_cancel_returns_error() {
    let run_count = Arc::new(RwLock::new(0));
    let store = gen_store();

    store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(DummyMigration::new(
        "20261114000001",
        run_count.clone(),
    )));

    // User declines
    let result = migrations.auto_migrate(&store, Box::new(|_| false), Box::new(|_| {}));

    assert!(matches!(result, Err(MigrateError::UserCancelled)));
    // Migration should NOT have run
    assert_eq!(*run_count.read().unwrap(), 0);
}

#[test]
fn test_break_change_migration() {
    let store = gen_store();
    store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);

    let mut migrations = Migrations::default();
    migrations.add_migration(Arc::new(BreakChangeMigration::new("20261114000001")));

    // auto_migrate should present a plan with has_break_change=true
    let result = migrations.auto_migrate(
        &store,
        Box::new(|plan| {
            assert!(plan.has_break_change);
            true // confirm anyway
        }),
        Box::new(|_| {}),
    );
    assert!(result.is_ok());
}

#[test]
fn test_db_migrate_check() {
    let store = gen_store();

    let migrate = DbMigrate::new();
    // No version set yet
    assert_eq!(migrate.check(&store), Ordering::Less);

    store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
    assert_eq!(migrate.check(&store), Ordering::Equal);

    store.put(MIGRATION_VERSION_KEY, "99991231235959");
    assert_eq!(migrate.check(&store), Ordering::Greater);
}

#[test]
fn test_invalid_previous_epochs_are_rejected_without_changing_records() {
    for version in [b"20260925120000", b"20261113000000"] {
        let path = gen_path();
        {
            let store = fiber_store::Store::open_db(&path).unwrap();
            store.put(MIGRATION_VERSION_KEY, version);
            store.put([0, 1], b"old channel bytes");
        }
        let Err(error) = crate::store::open_store(&path) else {
            panic!("old database must be rejected");
        };
        assert!(
            error.contains("Invalid channel record")
                || error.contains("Unsupported database layout"),
            "{error}"
        );
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(store.get(MIGRATION_VERSION_KEY).unwrap(), version);
        assert_eq!(store.get([0, 1]).unwrap(), b"old channel bytes");
    }
}

#[test]
fn test_nonempty_unversioned_database_is_not_stamped() {
    let store = gen_store();
    store.put([0, 1], b"unversioned channel bytes");
    let result = Migrations::default().auto_migrate(&store, Box::new(|_| true), Box::new(|_| {}));
    assert!(result.is_err());
    assert!(store.get(MIGRATION_VERSION_KEY).is_none());
    assert_eq!(store.get([0, 1]).unwrap(), b"unversioned channel bytes");
}

/// Write the two stored records of a current-epoch channel.
fn put_split_channel(store: &fiber_store::Store, channel: &fiber_types::ChannelActorData) {
    let (immutable, mutable) = channel.clone().into_parts();
    store.put(
        [&[1u8], channel.id.as_ref()].concat(),
        bincode::serialize(&immutable).unwrap(),
    );
    store.put(
        [&[0u8], channel.id.as_ref()].concat(),
        bincode::serialize(&mutable).unwrap(),
    );
}

#[test]
#[cfg(feature = "watchtower")]
fn test_upgrade_rejects_other_channel_record_suffixes_without_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::schema::{CHANNEL_OPEN_RECORD_PREFIX, WATCHTOWER_CHANNEL_PREFIX};

    fn to_old_settlement(s: fiber_types::SettlementData) -> fiber_types_0100::SettlementData {
        fiber_types_0100::SettlementData {
            local_amount: s.local_amount,
            remote_amount: s.remote_amount,
            tlcs: s
                .tlcs
                .into_iter()
                .map(|tlc| fiber_types_0100::SettlementTlc {
                    tlc_id: decode_as_new(tlc.tlc_id).unwrap(),
                    hash_algorithm: decode_as_new(tlc.hash_algorithm).unwrap(),
                    payment_amount: tlc.payment_amount,
                    payment_hash: decode_as_new(tlc.payment_hash).unwrap(),
                    expiry: tlc.expiry,
                    local_key: decode_as_new(tlc.local_key.expect("local key")).unwrap(),
                    remote_key: decode_as_new(tlc.remote_key).unwrap(),
                })
                .collect(),
        }
    }

    // The keys carry the record's own channel id, so a rejection can only come from the appended
    // byte rather than from a malformed key.
    let open = fiber_types::ChannelOpenRecord::samples(42)[0].clone();
    let new_watch = fiber_types::ChannelData::samples(42)[0].clone();
    let watch = fiber_types_0100::ChannelData {
        channel_id: decode_as_new(new_watch.channel_id).unwrap(),
        funding_udt_type_script: new_watch.funding_udt_type_script,
        local_settlement_key: decode_as_new(new_watch.local_settlement_key.expect("local key"))
            .unwrap(),
        remote_settlement_key: decode_as_new(new_watch.remote_settlement_key).unwrap(),
        local_funding_pubkey: decode_as_new(new_watch.local_funding_pubkey).unwrap(),
        remote_funding_pubkey: decode_as_new(new_watch.remote_funding_pubkey).unwrap(),
        remote_settlement_data: to_old_settlement(new_watch.remote_settlement_data),
        pending_remote_settlement_data: to_old_settlement(new_watch.pending_remote_settlement_data),
        local_settlement_data: to_old_settlement(new_watch.local_settlement_data),
        revocation_data: decode_as_new(new_watch.revocation_data).unwrap(),
        commitment_contract_features:
            fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    };
    let mut watch_key = vec![WATCHTOWER_CHANNEL_PREFIX];
    watch_key.extend_from_slice(&[7u8; 33]);
    watch_key.extend_from_slice(watch.channel_id.as_ref());
    let records = [
        (
            [&[CHANNEL_OPEN_RECORD_PREFIX], open.channel_id.as_ref()].concat(),
            bincode::serialize(&open).unwrap(),
        ),
        (watch_key, bincode::serialize(&watch).unwrap()),
    ];
    for (key, encoded) in records {
        // Seed the released epoch, so the *released* record decoders run: the current-layout path
        // would accept nothing anyway, and this is the path a real upgrade takes.
        let control = gen_path();
        {
            let store = fiber_store::Store::open_db(&control).unwrap();
            store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
            store.put(&key, &encoded);
        }
        let upgraded = crate::store::open_store(&control).expect("a valid record upgrades");
        assert_eq!(
            upgraded.get(MIGRATION_VERSION_KEY).unwrap(),
            LATEST_DB_VERSION.as_bytes()
        );
        drop(upgraded);

        let path = gen_path();
        let mut suffixed = encoded;
        suffixed.push(0);
        {
            let store = fiber_store::Store::open_db(&path).unwrap();
            store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
            store.put(&key, &suffixed);
        }
        assert!(crate::store::open_store(&path).is_err());
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(store.get(&key).unwrap(), suffixed);
        assert_eq!(
            store.get(MIGRATION_VERSION_KEY).unwrap(),
            INIT_DB_VERSION.as_bytes()
        );
    }
}

#[test]
fn test_v2_current_preflight_pages_closed_history_before_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, ChannelState, CloseFlags};
    let path = gen_path();
    let mut channels = Vec::new();
    for seed in 0..205 {
        channels.push(ChannelActorData::samples(seed).remove(0));
    }
    channels.sort_by(|a, b| a.id.as_ref().cmp(b.id.as_ref()));
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
        for channel in &channels {
            put_split_channel(&store, channel);
        }
    }
    drop(crate::store::open_store(&path).expect("valid multi-page history"));

    // A closed channel must still carry funded V2 history; the last record is on page three.
    let last = channels.last_mut().unwrap();
    last.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
    let (_, mutable) = last.clone().into_parts();
    let mutable = bincode::serialize(&mutable).unwrap();
    let mutable_key = [&[0], last.id.as_ref()].concat();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(&mutable_key, &mutable);
    }
    assert!(
        crate::store::open_store(&path).is_err(),
        "invalid closed record on page three must be scanned"
    );
    let store = fiber_store::Store::open_db(&path).unwrap();
    assert_eq!(store.get(&mutable_key).unwrap(), mutable);
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );
}

#[test]
fn test_v2_current_epoch_rejects_missing_session_before_writes() {
    use crate::store::sample::StoreSample;
    use fiber_types::{ChannelActorData, ChannelState, CloseFlags};
    let path = gen_path();
    let mut record = ChannelActorData::samples(1561).remove(0);
    record.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
    let (immutable, mutable) = record.clone().into_parts();
    let mut mutable = bincode::serialize(&mutable).unwrap();
    mutable.truncate(mutable.len() - bincode::serialize(&record.session).unwrap().len());
    let immutable = bincode::serialize(&immutable).unwrap();
    let mutable_key = [&[0], record.id.as_ref()].concat();
    let immutable_key = [&[1], record.id.as_ref()].concat();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
        store.put(&immutable_key, &immutable);
        store.put(&mutable_key, &mutable);
    }
    assert!(
        crate::store::open_store(&path).is_err(),
        "epoch cannot validate a full-hash channel without sessions"
    );
    let store = fiber_store::Store::open_db(&path).unwrap();
    assert_eq!(store.get(&mutable_key).unwrap(), mutable);
    assert_eq!(store.get(&immutable_key).unwrap(), immutable);
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );
}

#[test]
fn test_v2_current_epoch_rejects_bootstrap_session_as_closed_channel() {
    use crate::store::sample::StoreSample;
    let path = gen_path();
    let mut record = fiber_types::ChannelActorData::samples(1561).remove(0);
    record.state = fiber_types::ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
    let (immutable, mutable) = record.clone().into_parts();
    let mutable = bincode::serialize(&mutable).unwrap();
    let immutable = bincode::serialize(&immutable).unwrap();
    let mutable_key = [&[0], record.id.as_ref()].concat();
    let immutable_key = [&[1], record.id.as_ref()].concat();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, LATEST_DB_VERSION);
        store.put(&immutable_key, &immutable);
        store.put(&mutable_key, &mutable);
    }
    assert!(
        crate::store::open_store(&path).is_err(),
        "a genuine bootstrap nonce is insufficient evidence for a funded closed channel"
    );
    let store = fiber_store::Store::open_db(&path).unwrap();
    assert_eq!(store.get(&mutable_key).unwrap(), mutable);
}

#[test]
fn test_check_validate_accepts_released_records() {
    use fiber_types::sample::StoreSample;
    // A released database carries the legacy feature byte (`0x00`) in its opening records, the
    // released payment-session layout (without `upstream_tlc_key`), and released superseded
    // watchtower settlement snapshots (with untagged `SettlementTlc::local_key`), all of which the
    // current types reject: read-only validation must leave those to the migration instead of
    // failing on a database that can upgrade.
    let channel = fiber_types::ChannelActorData::samples(41).remove(0);
    let old_open = fiber_types_0100::ChannelOpenRecord {
        channel_id: decode_as_new(channel.id).unwrap(),
        pubkey: decode_as_new(channel.remote_pubkey).unwrap(),
        is_acceptor: false,
        status: fiber_types_0100::ChannelOpeningStatus::Failed,
        funding_amount: 1,
        failure_detail: None,
        created_at: 1,
        last_updated_at: 1,
        commitment_contract_features: fiber_types_0100::CommitmentContractFeatures::LEGACY,
    };
    let open_key = [
        &[fiber_types::schema::CHANNEL_OPEN_RECORD_PREFIX],
        old_open.channel_id.as_ref(),
    ]
    .concat();

    let payment_hash = fiber_types_0100::Hash256::from([0x42; 32]);
    let old_session = fiber_types_0100::PaymentSession {
        request: fiber_types_0100::SendPaymentData {
            target_pubkey: fiber_types_0100::Privkey::from(&[0x04; 32]).pubkey(),
            amount: 123_456,
            payment_hash,
            invoice: None,
            final_tlc_expiry_delta: 100,
            tlc_expiry_limit: 1000,
            timeout: None,
            max_fee_amount: Some(1_000),
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
            custom_records: None,
            allow_self_payment: false,
            hop_hints: vec![],
            router: vec![],
            allow_mpp: false,
            dry_run: false,
            trampoline_hops: None,
            trampoline_context: None,
        },
        last_error: Some("released error".to_owned()),
        last_error_code: None,
        try_limit: 3,
        status: fiber_types_0100::PaymentStatus::Failed,
        created_at: 100,
        last_updated_at: 200,
        cached_attempts: vec![],
    };
    let old_session_bytes = bincode::serialize(&old_session).unwrap();
    assert!(
        bincode::deserialize::<fiber_types::PaymentSession>(&old_session_bytes).is_err(),
        "released PaymentSession bytes must fail deserialization against current PaymentSession"
    );
    let session_key = [
        &[fiber_types::schema::PAYMENT_SESSION_PREFIX],
        payment_hash.as_ref(),
    ]
    .concat();

    let old_superseded = vec![fiber_types_0100::SettlementData {
        local_amount: 500_000,
        remote_amount: 750_000,
        tlcs: vec![fiber_types_0100::SettlementTlc {
            tlc_id: fiber_types_0100::TLCId::Offered(7),
            hash_algorithm: fiber_types_0100::HashAlgorithm::CkbHash,
            payment_amount: 88_000,
            payment_hash: fiber_types_0100::Hash256::from([0xab; 32]),
            expiry: 1_700_000_000_000,
            // Leading 0x02 byte is an invalid Option tag in head SettlementTlc.
            local_key: fiber_types_0100::Privkey::from(&[0x02u8; 32]),
            remote_key: fiber_types_0100::Privkey::from(&[0x03u8; 32]).pubkey(),
        }],
    }];
    let old_superseded_bytes = bincode::serialize(&old_superseded).unwrap();
    assert!(
        bincode::deserialize::<Vec<fiber_types::SettlementData>>(&old_superseded_bytes).is_err(),
        "released superseded settlement bytes must fail deserialization against current SettlementData"
    );
    let superseded_key = [
        &[fiber_types::schema::WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
        channel.id.as_ref(),
    ]
    .concat();

    let path = gen_path();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
        store.put(open_key, bincode::serialize(&old_open).unwrap());
        store.put(session_key, old_session_bytes);
        store.put(superseded_key, old_superseded_bytes);
    }
    crate::store::check_validate(&path)
        .expect("released records must not block read-only validation");
}

#[tokio::test]
async fn test_upgrade_retains_real_v2_signed_history_and_remains_valid() {
    use crate::fiber::channel::ChannelActorStateStore;
    use crate::tests::test_utils::create_nodes_with_established_channel;
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let payment = a.send_payment_keysend(&b, 1000, false).await.unwrap();
    a.wait_until_success(payment.payment_hash).await;
    crate::tests::test_utils::wait_until_timeout(5000, || {
        let channel = a.get_channel_actor_state(id);
        channel.get_offered_tlc_balance().unwrap() == 0 && !channel.tlc_state.waiting_ack
    })
    .await;
    let channel = a.get_channel_actor_state(id);
    assert!(channel.session.own.number > 2);
    assert!(channel.session.outgoing.is_some());
    assert!(channel.session.incoming.is_some());
    let original = bincode::serialize(&channel.core).unwrap();
    let old = released_v2_without_pending_tlcs(channel.core.clone());
    let key = [&[0], id.as_ref()].concat();
    let path = gen_path();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
        store.put(&key, old);
    }
    crate::store::check_validate(&path)
        .expect("read-only validation accepts an upgradeable V2 store");
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(
            store.get(MIGRATION_VERSION_KEY).unwrap(),
            INIT_DB_VERSION.as_bytes()
        );
    }
    let store = crate::store::open_store(&path).expect("migrate existing V2 channel");
    let restored = store.get_channel_actor_state(&id).unwrap();
    assert_eq!(bincode::serialize(&restored.core).unwrap(), original);
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );
    drop(store);
    crate::store::check_validate(&path).expect("upgraded signed history remains valid");
}

#[test]
fn test_upgrade_migrates_released_shutdown_settlement_record_with_tlc() {
    use crate::fiber::channel::ChannelActorStateStore;
    use fiber_types::schema::CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX;

    let channel_id = fiber_types::Hash256::from([0x91; 32]);
    let raw_key = [0x02u8; 32];
    let old_local_key = fiber_types_0100::Privkey::from(&raw_key);
    let expected_local_key = fiber_types::Privkey::from(&raw_key);
    let old_remote_key = fiber_types_0100::Privkey::from(&[0x03u8; 32]).pubkey();
    let expected_remote_key: fiber_types::Pubkey = decode_as_new(old_remote_key).unwrap();

    let old_record = fiber_types_0100::ShutdownSettlementRecord {
        shutdown_tx_hash: ckb_types::H256::from([0xab; 32]),
        for_remote: true,
        commitment_number: 14,
        settlement_data: fiber_types_0100::SettlementData {
            local_amount: 500_000,
            remote_amount: 750_000,
            tlcs: vec![fiber_types_0100::SettlementTlc {
                tlc_id: fiber_types_0100::TLCId::Offered(9),
                hash_algorithm: fiber_types_0100::HashAlgorithm::CkbHash,
                payment_amount: 88_000,
                payment_hash: fiber_types_0100::Hash256::from([0xcd; 32]),
                expiry: 1_700_000_012_345,
                local_key: old_local_key,
                remote_key: old_remote_key,
            }],
        },
    };
    let old_bytes = bincode::serialize(&old_record).unwrap();
    // Prove the unmigrated v0.10.1 bytes silently fail deserialization in Store::get_shutdown_settlement_record.
    assert!(
        bincode::deserialize::<fiber_types::ShutdownSettlementRecord>(&old_bytes).is_err(),
        "unmigrated v0.10.1 prefix-49 record with 0x02-prefixed TLC key must not deserialize as head"
    );

    let key = [&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX], channel_id.as_ref()].concat();
    let path = gen_path();
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        store.put(MIGRATION_VERSION_KEY, INIT_DB_VERSION);
        store.put(&key, &old_bytes);
    }

    crate::store::check_validate(&path)
        .expect("read-only validation accepts an upgradeable released prefix-49 snapshot");
    {
        let store = fiber_store::Store::open_db(&path).unwrap();
        assert_eq!(
            store.get(MIGRATION_VERSION_KEY).unwrap(),
            INIT_DB_VERSION.as_bytes()
        );
        assert_eq!(store.get(&key).unwrap(), old_bytes);
    }

    let store = crate::store::open_store(&path).expect("migrate released prefix-49 snapshot");
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        LATEST_DB_VERSION.as_bytes()
    );

    let restored = store
        .get_shutdown_settlement_record(&channel_id)
        .expect("migrated shutdown settlement record must be readable through normal getter");
    assert_eq!(restored.shutdown_tx_hash, old_record.shutdown_tx_hash);
    assert_eq!(restored.for_remote, old_record.for_remote);
    assert_eq!(restored.commitment_number, old_record.commitment_number);
    assert_eq!(restored.settlement_data.local_amount, 500_000);
    assert_eq!(restored.settlement_data.remote_amount, 750_000);
    assert_eq!(restored.settlement_data.tlcs.len(), 1);
    let tlc = &restored.settlement_data.tlcs[0];
    assert_eq!(tlc.tlc_id, fiber_types::TLCId::Offered(9));
    assert_eq!(tlc.hash_algorithm, fiber_types::HashAlgorithm::CkbHash);
    assert_eq!(tlc.payment_amount, 88_000);
    assert_eq!(tlc.payment_hash, fiber_types::Hash256::from([0xcd; 32]));
    assert_eq!(tlc.expiry, 1_700_000_012_345);
    assert_eq!(tlc.local_key.as_ref(), Some(&expected_local_key));
    assert_eq!(tlc.local_key_pubkey, Some(expected_local_key.pubkey()));
    assert_eq!(tlc.local_key_commitment_number, None);
    assert_eq!(tlc.remote_key, expected_remote_key);

    drop(store);
    crate::store::check_validate(&path).expect("upgraded prefix-49 snapshot remains valid");
}
