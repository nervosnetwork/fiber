use crate::fiber::onchain_tlc_reconcile::{
    collect_onchain_expired_settled_tlcs, collect_onchain_fulfilled_tlcs, resolve_onchain_tlc,
    OnChainTlcResolution,
};
use crate::fiber::tests::settle_tlc_set_command_tests::{
    create_test_channel_state_with_tlc, MockStore,
};
use crate::gen_rand_sha256_hash;

use fiber_types::{
    AppliedFlags, CommitmentNumbers, Hash256, HashAlgorithm, OutboundTlcStatus, RemoveTlcFulfill,
    RemoveTlcReason, TLCId, TlcInfo, TlcStatus,
};

const TEST_SHARED_SECRET: [u8; 32] = [7u8; 32];

fn payment_hash_for(preimage: Hash256, hash_algorithm: HashAlgorithm) -> Hash256 {
    hash_algorithm.hash(preimage).into()
}

fn empty_channel_state(channel_id: Hash256) -> crate::fiber::channel::ChannelActorState {
    let mut state =
        create_test_channel_state_with_tlc(channel_id, 0, 1000, gen_rand_sha256_hash(), None);
    state.tlc_state.offered_tlcs.tlcs.clear();
    state.tlc_state.received_tlcs.tlcs.clear();
    state
}

fn tlc_info(
    tlc_id: TLCId,
    status: TlcStatus,
    payment_hash: Hash256,
    hash_algorithm: HashAlgorithm,
) -> TlcInfo {
    TlcInfo {
        status,
        tlc_id,
        amount: 1000,
        payment_hash,
        total_amount: None,
        payment_secret: None,
        attempt_id: None,
        expiry: 10,
        hash_algorithm,
        onion_packet: None,
        shared_secret: TEST_SHARED_SECRET,
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::default(),
        removed_reason: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::empty(),
        forwarding_tlc: None,
    }
}

#[test]
fn resolve_returns_fulfilled_when_preimage_matches() {
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let store = MockStore::new().with_onchain_preimage(payment_hash, preimage);

    assert_eq!(
        resolve_onchain_tlc(
            &channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            hash_algorithm,
        ),
        OnChainTlcResolution::Fulfilled(preimage)
    );
}

#[test]
fn resolve_falls_through_to_settled_when_preimage_mismatches() {
    let channel_id = gen_rand_sha256_hash();
    let correct_preimage = gen_rand_sha256_hash();
    let wrong_preimage = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = payment_hash_for(correct_preimage, hash_algorithm);
    let store = MockStore::new()
        .with_onchain_preimage(payment_hash, wrong_preimage)
        .with_onchain_settled(payment_hash);

    assert_eq!(
        resolve_onchain_tlc(
            &channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            hash_algorithm,
        ),
        OnChainTlcResolution::SettledWithoutPreimage
    );
}

#[test]
fn resolve_returns_settled_without_preimage() {
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let store = MockStore::new().with_onchain_settled(payment_hash);

    assert_eq!(
        resolve_onchain_tlc(
            &channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            HashAlgorithm::CkbHash,
        ),
        OnChainTlcResolution::SettledWithoutPreimage
    );
}

#[test]
fn resolve_returns_unknown_by_default() {
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let store = MockStore::new();

    assert_eq!(
        resolve_onchain_tlc(
            &channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            HashAlgorithm::CkbHash,
        ),
        OnChainTlcResolution::Unknown
    );
}

#[test]
fn collect_skips_removed_and_uncommitted_tlcs() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let active_preimage = gen_rand_sha256_hash();
    let removed_preimage = gen_rand_sha256_hash();
    let uncommitted_preimage = gen_rand_sha256_hash();
    let active_hash = payment_hash_for(active_preimage, hash_algorithm);
    let removed_hash = payment_hash_for(removed_preimage, hash_algorithm);
    let uncommitted_hash = payment_hash_for(uncommitted_preimage, hash_algorithm);

    let active = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        active_hash,
        hash_algorithm,
    );
    let mut removed = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        removed_hash,
        hash_algorithm,
    );
    removed.removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: removed_preimage,
    }));
    let uncommitted = tlc_info(
        TLCId::Offered(2),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        uncommitted_hash,
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![active, removed, uncommitted];
    let store = MockStore::new()
        .with_onchain_preimage(active_hash, active_preimage)
        .with_onchain_preimage(removed_hash, removed_preimage)
        .with_onchain_preimage(uncommitted_hash, uncommitted_preimage);

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);

    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Offered(0));
    assert_eq!(fulfilled[0].preimage, active_preimage);
}

#[test]
fn collect_expired_settled_requires_forwarding_and_settled_marker() {
    let channel_id = gen_rand_sha256_hash();
    let upstream_channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let matched_hash = gen_rand_sha256_hash();
    let no_forwarding_hash = gen_rand_sha256_hash();
    let not_settled_hash = gen_rand_sha256_hash();
    let not_expired_hash = gen_rand_sha256_hash();

    let mut matched = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        matched_hash,
        hash_algorithm,
    );
    matched.forwarding_tlc = Some((upstream_channel_id, 42));

    let no_forwarding = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        no_forwarding_hash,
        hash_algorithm,
    );

    let mut not_settled = tlc_info(
        TLCId::Offered(2),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        not_settled_hash,
        hash_algorithm,
    );
    not_settled.forwarding_tlc = Some((gen_rand_sha256_hash(), 43));

    let mut not_expired = tlc_info(
        TLCId::Offered(3),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        not_expired_hash,
        hash_algorithm,
    );
    not_expired.forwarding_tlc = Some((gen_rand_sha256_hash(), 44));
    not_expired.expiry = 1_000;

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![matched, no_forwarding, not_settled, not_expired];
    let store = MockStore::new()
        .with_onchain_settled(matched_hash)
        .with_onchain_settled(no_forwarding_hash)
        .with_onchain_settled(not_expired_hash);

    let expired = collect_onchain_expired_settled_tlcs(&state, &store, 100);

    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].forwarding_channel_id, upstream_channel_id);
    assert_eq!(expired[0].forwarding_tlc_id, 42);
    assert_eq!(expired[0].shared_secret, TEST_SHARED_SECRET);
}
