use crate::fiber::onchain_tlc_reconcile::{
    collect_onchain_fulfilled_tlcs, collect_onchain_received_timeout_settled_tlcs,
    collect_onchain_timeout_settled_tlcs, has_unresolved_onchain_tlcs, resolve_onchain_tlc,
    LegacyOnChainTlcSettlement, OnChainTimeoutTlcRole, OnChainTlcResolution,
};
use crate::fiber::tests::settle_tlc_set_command_tests::{
    create_test_channel_state_with_tlc, MockStore,
};
use crate::gen_rand_sha256_hash;

use fiber_types::{
    AppliedFlags, CommitmentNumbers, Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus,
    RemoveTlcFulfill, RemoveTlcReason, TLCId, TlcErr, TlcErrPacket, TlcErrorCode, TlcInfo,
    TlcStatus,
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
    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
        preimage,
    );

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
fn resolve_returns_unknown_when_preimage_mismatches() {
    let channel_id = gen_rand_sha256_hash();
    let correct_preimage = gen_rand_sha256_hash();
    let wrong_preimage = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = payment_hash_for(correct_preimage, hash_algorithm);
    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
        wrong_preimage,
    );

    assert_eq!(
        resolve_onchain_tlc(
            &channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            hash_algorithm,
        ),
        OnChainTlcResolution::Unknown
    );
}

#[test]
fn legacy_no_preimage_record_is_not_attributed_to_a_tlc() {
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let store = MockStore::new().with_legacy_onchain_settlement(
        channel_id,
        payment_hash,
        LegacyOnChainTlcSettlement {
            preimage: None,
            tx_hash: Some(gen_rand_sha256_hash()),
            tlc_index: Some(0),
        },
    );

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
fn legacy_preimage_record_requires_a_full_hash_match() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let store = MockStore::new().with_legacy_onchain_settlement(
        channel_id,
        payment_hash,
        LegacyOnChainTlcSettlement {
            preimage: Some(preimage),
            tx_hash: Some(gen_rand_sha256_hash()),
            tlc_index: Some(0),
        },
    );

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
fn resolve_returns_settled_without_preimage() {
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        HashAlgorithm::CkbHash,
    );

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
fn resolve_is_channel_scoped() {
    let channel_id = gen_rand_sha256_hash();
    let other_channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
        preimage,
    );

    assert_eq!(
        resolve_onchain_tlc(
            &other_channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            hash_algorithm,
        ),
        OnChainTlcResolution::Unknown
    );
}

#[test]
fn resolve_ignores_locally_known_preimage_without_settlement_record() {
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let store = MockStore::new().with_preimage(payment_hash, preimage);

    assert_eq!(
        resolve_onchain_tlc(
            &channel_id,
            &store,
            TLCId::Offered(0),
            payment_hash,
            hash_algorithm,
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
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(0),
            active_hash,
            hash_algorithm,
            active_preimage,
        )
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(1),
            removed_hash,
            hash_algorithm,
            removed_preimage,
        )
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(2),
            uncommitted_hash,
            hash_algorithm,
            uncommitted_preimage,
        );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);

    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Offered(0));
    assert_eq!(fulfilled[0].preimage, active_preimage);
}

#[test]
fn collect_fulfilled_allows_non_unique_prefix_when_full_hash_matches() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_for(preimage, hash_algorithm);

    let first = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        payment_hash,
        hash_algorithm,
    );
    let second = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        payment_hash,
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![first, second];
    let store = MockStore::new()
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(0),
            payment_hash,
            hash_algorithm,
            preimage,
        )
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(1),
            payment_hash,
            hash_algorithm,
            preimage,
        );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);

    assert_eq!(fulfilled.len(), 2);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Offered(0));
    assert_eq!(fulfilled[0].preimage, preimage);
    assert_eq!(fulfilled[1].tlc_id, TLCId::Offered(1));
    assert_eq!(fulfilled[1].preimage, preimage);
}

#[test]
fn collect_timeout_settled_includes_forwarded_and_origin_payer() {
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
        .with_onchain_settled(channel_id, TLCId::Offered(0), matched_hash, hash_algorithm)
        .with_onchain_settled(
            channel_id,
            TLCId::Offered(1),
            no_forwarding_hash,
            hash_algorithm,
        )
        .with_onchain_settled(
            channel_id,
            TLCId::Offered(3),
            not_expired_hash,
            hash_algorithm,
        );

    let expired = collect_onchain_timeout_settled_tlcs(&state, &store, 100);

    assert_eq!(expired.len(), 2);
    assert_eq!(expired[0].tlc_id, TLCId::Offered(0));
    assert_eq!(
        expired[0].role,
        OnChainTimeoutTlcRole::Forwarded {
            forwarding_channel_id: upstream_channel_id,
            forwarding_tlc_id: 42,
        }
    );
    assert_eq!(expired[0].shared_secret, TEST_SHARED_SECRET);
    assert_eq!(expired[1].tlc_id, TLCId::Offered(1));
    assert_eq!(
        expired[1].role,
        OnChainTimeoutTlcRole::OriginPayer { attempt_id: None }
    );
}

#[test]
fn collect_timeout_settled_skips_already_removed() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = gen_rand_sha256_hash();
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        payment_hash,
        hash_algorithm,
    );
    tlc.removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: gen_rand_sha256_hash(),
    }));

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
    );

    assert!(collect_onchain_timeout_settled_tlcs(&state, &store, 100).is_empty());
}

#[test]
fn collect_received_timeout_skips_uncommitted_tlc_with_same_hash() {
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let committed = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::Committed),
        payment_hash,
        hash_algorithm,
    );
    let uncommitted = tlc_info(
        TLCId::Received(1),
        TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced),
        payment_hash,
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.received_tlcs.tlcs = vec![committed, uncommitted];
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Received(0),
        payment_hash,
        hash_algorithm,
    );

    let settled = collect_onchain_received_timeout_settled_tlcs(&state, &store);

    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].tlc_id, 0);
    for tlc in settled {
        state.tlc_state.set_received_tlc_removed(
            tlc.tlc_id,
            RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                TlcErr::new(TlcErrorCode::ExpiryTooSoon),
                &TEST_SHARED_SECRET,
            )),
        );
    }
}

#[test]
fn collect_timeout_uses_exact_tlc_identity_for_shared_prefixes() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let first_hash_bytes = [1u8; 32];
    let mut second_hash_bytes = [2u8; 32];
    second_hash_bytes[..20].copy_from_slice(&first_hash_bytes[..20]);
    let first_hash = Hash256::from(first_hash_bytes);
    let second_hash = Hash256::from(second_hash_bytes);

    let first = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        first_hash,
        hash_algorithm,
    );
    let second = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        second_hash,
        hash_algorithm,
    );
    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![first, second];
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Offered(0),
        first_hash,
        hash_algorithm,
    );

    let settled = collect_onchain_timeout_settled_tlcs(&state, &store, 100);
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].tlc_id, TLCId::Offered(0));
    assert!(has_unresolved_onchain_tlcs(&state));
}

#[test]
fn forged_full_hash_does_not_inherit_removed_tlc_prefix_settlement() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let fulfilled_hash = payment_hash_for(preimage, hash_algorithm);
    let mut forged_hash_bytes: [u8; 32] = fulfilled_hash.into();
    forged_hash_bytes[31] ^= 1;
    let forged_hash = Hash256::from(forged_hash_bytes);

    let fulfilled = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        fulfilled_hash,
        hash_algorithm,
    );
    let forged = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        forged_hash,
        hash_algorithm,
    );
    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![fulfilled, forged];
    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Offered(0),
        fulfilled_hash,
        hash_algorithm,
        preimage,
    );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);
    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Offered(0));

    state.tlc_state.set_offered_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        }),
    );

    assert!(
        collect_onchain_timeout_settled_tlcs(&state, &store, 100).is_empty(),
        "a settlement record for the removed TLC must not settle the forged full hash"
    );
}
