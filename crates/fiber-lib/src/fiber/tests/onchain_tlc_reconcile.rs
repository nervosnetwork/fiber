use crate::fiber::onchain_tlc_reconcile::{
    can_reconcile_onchain_fulfillment, collect_onchain_fulfilled_tlcs,
    collect_onchain_received_timeout_settled_tlcs, collect_onchain_timeout_settled_tlcs,
    has_unresolved_onchain_tlcs, has_unresolved_onchain_tlcs_for_snapshot, resolve_onchain_tlc,
    LegacyOnChainTlcSettlement, OnChainTimeoutTlcRole, OnChainTlcResolution,
};
use crate::fiber::tests::settle_tlc_set_command_tests::{
    create_test_channel_state_with_tlc, MockStore,
};
use crate::{gen_rand_fiber_public_key, gen_rand_sha256_hash};

use ckb_types::core::TransactionBuilder;
use ckb_types::packed::CellOutput;
use ckb_types::prelude::*;
use fiber_types::{
    AppliedFlags, ChannelBasePublicKeys, ChannelData, ChannelState, CloseFlags, CommitmentNumbers,
    Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus, Privkey, RemoveTlcFulfill,
    RemoveTlcReason, RevocationData, SettlementData, SettlementTlc, TLCId, TlcErr, TlcErrPacket,
    TlcErrorCode, TlcInfo, TlcStatus,
};
use musig2::CompactSignature;

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
        TLCId::Offered(0),
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
        TLCId::Offered(0),
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
fn collect_skips_removed_offered_tlcs() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let active_preimage = gen_rand_sha256_hash();
    let removed_preimage = gen_rand_sha256_hash();
    let active_hash = payment_hash_for(active_preimage, hash_algorithm);
    let removed_hash = payment_hash_for(removed_preimage, hash_algorithm);

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

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![active, removed];
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
        );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);

    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Offered(0));
    assert_eq!(fulfilled[0].preimage, active_preimage);
}

#[test]
fn collect_skips_inbound_remote_announced() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let committed_preimage = gen_rand_sha256_hash();
    let uncommitted_preimage = gen_rand_sha256_hash();
    let committed_hash = payment_hash_for(committed_preimage, hash_algorithm);
    let uncommitted_hash = payment_hash_for(uncommitted_preimage, hash_algorithm);

    let committed = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::Committed),
        committed_hash,
        hash_algorithm,
    );
    let uncommitted = tlc_info(
        TLCId::Received(1),
        TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced),
        uncommitted_hash,
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.received_tlcs.tlcs = vec![committed, uncommitted];
    let store = MockStore::new()
        .with_onchain_preimage(
            channel_id,
            TLCId::Received(0),
            committed_hash,
            hash_algorithm,
            committed_preimage,
        )
        .with_onchain_preimage(
            channel_id,
            TLCId::Received(1),
            uncommitted_hash,
            hash_algorithm,
            uncommitted_preimage,
        );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);

    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Received(0));
    assert_eq!(fulfilled[0].preimage, committed_preimage);
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
    // A confirmed remove has completed its commitment handshake and was already propagated
    // upstream, so on-chain timeout reconciliation must skip it.
    tlc.removed_confirmed_at = Some(1);

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
fn collect_timeout_settled_collects_uncommitted_removed() {
    // Issue #1612: a TLC marked removed by a peer RemoveTlc message whose remove commitment
    // handshake never completed (channel shutting down with WAITING_COMMITMENT_CONFIRMATION)
    // still needs on-chain timeout reconciliation. `removed_reason` is set but
    // `removed_confirmed_at` is not, so the upstream RemoveTlc was never propagated.
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = gen_rand_sha256_hash();
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::RemoteRemoved),
        payment_hash,
        hash_algorithm,
    );
    tlc.removed_reason = Some(RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
        TlcErr::new(TlcErrorCode::ExpiryTooSoon),
        &TEST_SHARED_SECRET,
    )));

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
    );

    let expired = collect_onchain_timeout_settled_tlcs(&state, &store, 100);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].tlc_id, TLCId::Offered(0));
    assert_eq!(
        expired[0].role,
        OnChainTimeoutTlcRole::OriginPayer { attempt_id: None }
    );
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

fn closed_state_with_offered_local_announced(
    channel_id: Hash256,
    flags: CloseFlags,
    tlc: TlcInfo,
) -> crate::fiber::channel::ChannelActorState {
    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(flags);
    state.to_local_amount = 100_000_000;
    state.to_remote_amount = 100_000_000;
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];
    let funding_tx = TransactionBuilder::default()
        .output(
            CellOutput::new_builder()
                .capacity(100_000_000 * 100_000_000u64)
                .build(),
        )
        .build();
    state.funding_tx = Some(funding_tx.data());
    state.remote_channel_public_keys = Some(ChannelBasePublicKeys {
        funding_pubkey: gen_rand_fiber_public_key(),
        tlc_base_key: gen_rand_fiber_public_key(),
    });
    state.remote_commitment_points = vec![
        (0, gen_rand_fiber_public_key()),
        (1, gen_rand_fiber_public_key()),
    ];
    state
}

#[test]
fn collect_fulfilled_includes_offered_local_announced() {
    // A signed remote commitment already includes offered LocalAnnounced TLCs. If the
    // counterparty broadcasts that commitment and spends the TLC on-chain, fulfillment
    // reconciliation must still pick it up.
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        hash_algorithm,
    );
    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];
    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
        preimage,
    );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);
    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Offered(0));
    assert_eq!(fulfilled[0].preimage, preimage);
}

#[test]
fn collect_timeout_includes_expired_offered_local_announced() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = gen_rand_sha256_hash();
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        hash_algorithm,
    );
    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
    );

    let expired = collect_onchain_timeout_settled_tlcs(&state, &store, 100);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].tlc_id, TLCId::Offered(0));
    assert_eq!(
        expired[0].role,
        OnChainTimeoutTlcRole::OriginPayer { attempt_id: None }
    );
}

#[test]
fn collect_timeout_includes_forwarded_local_announced() {
    let channel_id = gen_rand_sha256_hash();
    let upstream_channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = gen_rand_sha256_hash();
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        hash_algorithm,
    );
    tlc.forwarding_tlc = Some((upstream_channel_id, 7));
    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];
    let store = MockStore::new().with_onchain_settled(
        channel_id,
        TLCId::Offered(0),
        payment_hash,
        hash_algorithm,
    );

    let expired = collect_onchain_timeout_settled_tlcs(&state, &store, 100);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].tlc_id, TLCId::Offered(0));
    assert_eq!(
        expired[0].role,
        OnChainTimeoutTlcRole::Forwarded {
            forwarding_channel_id: upstream_channel_id,
            forwarding_tlc_id: 7,
        }
    );
}

fn settlement_tlc_for(tlc: &TlcInfo) -> SettlementTlc {
    SettlementTlc {
        tlc_id: tlc.tlc_id,
        hash_algorithm: tlc.hash_algorithm,
        payment_amount: tlc.amount,
        payment_hash: tlc.payment_hash,
        expiry: tlc.expiry,
        local_key: Privkey::from([1u8; 32]),
        remote_key: Privkey::from([2u8; 32]).pubkey(),
    }
}

fn settlement_data_for_commitment(
    channel_data: &ChannelData,
    for_remote: bool,
    commitment_number: u64,
) -> &SettlementData {
    if for_remote {
        if channel_data
            .revocation_data
            .as_ref()
            .and_then(|revocation| {
                commitment_number
                    .checked_sub(1)
                    .map(|previous| revocation.commitment_number == previous)
            })
            .unwrap_or(false)
        {
            &channel_data.remote_settlement_data
        } else {
            &channel_data.pending_remote_settlement_data
        }
    } else {
        &channel_data.local_settlement_data
    }
}

#[test]
fn settlement_data_for_commitment_distinguishes_pending_and_preceding_remote_commitments() {
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_for(preimage, HashAlgorithm::CkbHash);
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        HashAlgorithm::CkbHash,
    );

    let preceding_remote_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![],
    };
    let pending_remote_settlement = SettlementData {
        local_amount: 0,
        remote_amount: 1000,
        tlcs: vec![settlement_tlc_for(&tlc)],
    };
    let local_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![],
    };

    let channel_data = ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: Privkey::from([1u8; 32]),
        remote_settlement_key: Privkey::from([2u8; 32]).pubkey(),
        local_funding_pubkey: Privkey::from([3u8; 32]).pubkey(),
        remote_funding_pubkey: Privkey::from([4u8; 32]).pubkey(),
        remote_settlement_data: preceding_remote_settlement.clone(),
        pending_remote_settlement_data: pending_remote_settlement.clone(),
        local_settlement_data: local_settlement.clone(),
        revocation_data: Some(RevocationData {
            commitment_number: 5,
            aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Default::default(),
        }),
    };

    // Commitment 6 is the preceding unrevoked remote commitment (revocation.commitment_number == 6 - 1)
    let selected_preceding = settlement_data_for_commitment(&channel_data, true, 6);
    assert_eq!(selected_preceding.tlcs.len(), 0);

    // Commitment 7 is the pending remote commitment
    let selected_pending = settlement_data_for_commitment(&channel_data, true, 7);
    assert_eq!(selected_pending.tlcs.len(), 1);
    assert_eq!(selected_pending.tlcs[0].tlc_id, TLCId::Offered(0));

    // Local commitment
    let selected_local = settlement_data_for_commitment(&channel_data, false, 6);
    assert_eq!(selected_local.tlcs.len(), 0);
}

#[test]
fn has_unresolved_distinguishes_pending_and_preceding_remote_force_close() {
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        HashAlgorithm::CkbHash,
    );
    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc.clone(),
    );

    let preceding_remote_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![],
    };
    let pending_remote_settlement = SettlementData {
        local_amount: 0,
        remote_amount: 1000,
        tlcs: vec![settlement_tlc_for(&tlc)],
    };

    // When the preceding remote commitment is published on-chain, the LocalAnnounced TLC is NOT in the snapshot
    // and must not block on-chain settlement.
    assert!(
        !has_unresolved_onchain_tlcs_for_snapshot(&state, &preceding_remote_settlement, true),
        "preceding remote commitment omits offered LocalAnnounced TLC and must not block finalization"
    );

    // When the pending remote commitment is published on-chain, the LocalAnnounced TLC IS in the snapshot
    // and must be waited on until settled.
    assert!(
        has_unresolved_onchain_tlcs_for_snapshot(&state, &pending_remote_settlement, true),
        "pending remote commitment includes offered LocalAnnounced TLC and must wait for settlement"
    );

    // Once resolved on-chain (e.g. marked removed after preimage or timeout), it no longer blocks.
    state.tlc_state.set_offered_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &TEST_SHARED_SECRET,
        )),
    );
    assert!(
        !has_unresolved_onchain_tlcs_for_snapshot(&state, &pending_remote_settlement, true),
        "resolved TLC in pending remote commitment must not block finalization"
    );
}

#[test]
fn has_unresolved_and_fulfill_for_received_announce_wait_ack_on_local_close() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let tlc = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitAck),
        payment_hash,
        hash_algorithm,
    );
    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    state.tlc_state.received_tlcs.tlcs = vec![tlc.clone()];

    // Local commitment contains received AnnounceWaitAck TLC (from counterparty's view it is flipped)
    let local_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0), // flipped for local commitment
            hash_algorithm,
            payment_amount: 1000,
            payment_hash,
            expiry: 10,
            local_key: Privkey::from([1u8; 32]),
            remote_key: Privkey::from([2u8; 32]).pubkey(),
        }],
    };

    assert!(can_reconcile_onchain_fulfillment(&tlc));
    assert!(
        has_unresolved_onchain_tlcs_for_snapshot(&state, &local_settlement, false),
        "local force close commitment includes received AnnounceWaitAck TLC and must wait for settlement"
    );

    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Received(0),
        payment_hash,
        hash_algorithm,
        preimage,
    );

    let fulfilled = collect_onchain_fulfilled_tlcs(&state, &store);
    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].tlc_id, TLCId::Received(0));
    assert_eq!(fulfilled[0].preimage, preimage);

    state.tlc_state.set_received_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        }),
    );
    assert!(
        !has_unresolved_onchain_tlcs_for_snapshot(&state, &local_settlement, false),
        "resolved received TLC in local commitment must not block finalization"
    );
}

#[test]
fn has_unresolved_ignores_local_announced_on_local_force_close() {
    let channel_id = gen_rand_sha256_hash();
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    let state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );

    assert!(
        !has_unresolved_onchain_tlcs(&state),
        "a local force-close broadcasts the local commitment, which omits offered LocalAnnounced TLCs"
    );
}

#[test]
fn has_unresolved_keeps_local_announced_on_remote_force_close() {
    let channel_id = gen_rand_sha256_hash();
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    let state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );

    assert!(
        has_unresolved_onchain_tlcs(&state),
        "a remote force-close can spend offered LocalAnnounced TLCs from the signed remote commitment"
    );
}

#[test]
fn has_unresolved_ignores_local_announced_on_preceding_remote_commitment_force_close() {
    let channel_id = gen_rand_sha256_hash();
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    tlc.created_at = CommitmentNumbers {
        local: 1,
        remote: 1,
    };

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );
    // Remote force-closed by broadcasting preceding commitment (not matching pending commitment hash)
    state.shutdown_transaction_hash = Some(gen_rand_sha256_hash().into());

    assert!(
        !has_unresolved_onchain_tlcs(&state),
        "a remote force-close broadcasting preceding commitment omits LocalAnnounced TLC and must not block settlement"
    );
}

#[test]
fn has_unresolved_waits_for_local_announced_on_pending_remote_commitment_force_close() {
    let channel_id = gen_rand_sha256_hash();
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    tlc.created_at = CommitmentNumbers {
        local: 1,
        remote: 1,
    };

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );
    let (pending_tx, _) = state
        .build_commitment_tx_and_settlement_data(true)
        .expect("build pending commitment tx");
    state.shutdown_transaction_hash = Some(pending_tx.hash().unpack());

    assert!(
        has_unresolved_onchain_tlcs(&state),
        "a remote force-close broadcasting pending commitment includes LocalAnnounced TLC and must wait for settlement"
    );

    // Once resolved on-chain, it no longer blocks
    state.tlc_state.set_offered_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &TEST_SHARED_SECRET,
        )),
    );
    assert!(
        !has_unresolved_onchain_tlcs(&state),
        "resolved LocalAnnounced TLC must not block settlement"
    );
}

#[test]
fn has_unresolved_falls_back_to_waiting_when_commitment_is_unknown() {
    let channel_id = gen_rand_sha256_hash();
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    tlc.created_at = CommitmentNumbers {
        local: 1,
        remote: 1,
    };

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );
    state.shutdown_transaction_hash = None;

    assert!(
        has_unresolved_onchain_tlcs(&state),
        "when confirmed commitment transaction is not yet known, conservatively wait for LocalAnnounced TLC"
    );
}

#[test]
fn set_offered_tlc_removed_accepts_local_announced() {
    let channel_id = gen_rand_sha256_hash();
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![tlc];

    state.tlc_state.set_offered_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &TEST_SHARED_SECRET,
        )),
    );

    let updated = state
        .tlc_state
        .get(&TLCId::Offered(0))
        .expect("offered tlc remains after on-chain remove");
    assert_eq!(updated.outbound_status(), OutboundTlcStatus::RemoteRemoved);
    assert!(updated.removed_reason.is_some());
}

#[test]
fn settlement_data_for_commitment_edge_cases_no_revocation_and_zero_commitment() {
    let channel_id = gen_rand_sha256_hash();
    let preceding_remote = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![],
    };
    let pending_remote = SettlementData {
        local_amount: 300,
        remote_amount: 400,
        tlcs: vec![],
    };
    let local = SettlementData {
        local_amount: 500,
        remote_amount: 600,
        tlcs: vec![],
    };

    let channel_data_no_revocation = ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: Privkey::from([1u8; 32]),
        remote_settlement_key: Privkey::from([2u8; 32]).pubkey(),
        local_funding_pubkey: Privkey::from([3u8; 32]).pubkey(),
        remote_funding_pubkey: Privkey::from([4u8; 32]).pubkey(),
        remote_settlement_data: preceding_remote.clone(),
        pending_remote_settlement_data: pending_remote.clone(),
        local_settlement_data: local.clone(),
        revocation_data: None,
    };

    // When revocation_data is None, remote commitment falls back to pending_remote_settlement_data
    assert_eq!(
        settlement_data_for_commitment(&channel_data_no_revocation, true, 1).local_amount,
        300
    );
    // Commitment number 0 should not underflow and should return pending
    assert_eq!(
        settlement_data_for_commitment(&channel_data_no_revocation, true, 0).local_amount,
        300
    );
    // Local commitment returns local_settlement_data
    assert_eq!(
        settlement_data_for_commitment(&channel_data_no_revocation, false, 0).local_amount,
        500
    );

    let channel_data_with_revocation = ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: Privkey::from([1u8; 32]),
        remote_settlement_key: Privkey::from([2u8; 32]).pubkey(),
        local_funding_pubkey: Privkey::from([3u8; 32]).pubkey(),
        remote_funding_pubkey: Privkey::from([4u8; 32]).pubkey(),
        remote_settlement_data: preceding_remote,
        pending_remote_settlement_data: pending_remote,
        local_settlement_data: local,
        revocation_data: Some(RevocationData {
            commitment_number: 0,
            aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Default::default(),
        }),
    };

    // Commitment 0 with revocation for 0: checked_sub(1) underflows safely to None -> returns pending
    assert_eq!(
        settlement_data_for_commitment(&channel_data_with_revocation, true, 0).local_amount,
        300
    );
    // Commitment 1 with revocation for 0: 1 - 1 == 0 -> returns preceding (remote_settlement_data)
    assert_eq!(
        settlement_data_for_commitment(&channel_data_with_revocation, true, 1).local_amount,
        100
    );
}

#[test]
fn multiple_concurrent_tlcs_distinguish_pending_and_preceding_remote_close() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let preimage_0 = gen_rand_sha256_hash();
    let tlc_0 = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash_for(preimage_0, hash_algorithm),
        hash_algorithm,
    );

    let preimage_1 = gen_rand_sha256_hash();
    let tlc_1 = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        payment_hash_for(preimage_1, hash_algorithm),
        hash_algorithm,
    );

    let preimage_2 = gen_rand_sha256_hash();
    let tlc_2 = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::Committed),
        payment_hash_for(preimage_2, hash_algorithm),
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    state.tlc_state.offered_tlcs.tlcs = vec![tlc_0.clone(), tlc_1.clone()];
    state.tlc_state.received_tlcs.tlcs = vec![tlc_2.clone()];

    // Preceding commitment contains TLC 1 and TLC 2, but NOT TLC 0 (LocalAnnounced)
    let preceding_remote_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![settlement_tlc_for(&tlc_1), settlement_tlc_for(&tlc_2)],
    };

    // Pending commitment contains TLC 0, TLC 1, and TLC 2
    let pending_remote_settlement = SettlementData {
        local_amount: 0,
        remote_amount: 1000,
        tlcs: vec![
            settlement_tlc_for(&tlc_0),
            settlement_tlc_for(&tlc_1),
            settlement_tlc_for(&tlc_2),
        ],
    };

    // Both snapshots initially report unresolved TLCs
    assert!(has_unresolved_onchain_tlcs_for_snapshot(
        &state,
        &preceding_remote_settlement,
        true
    ));
    assert!(has_unresolved_onchain_tlcs_for_snapshot(
        &state,
        &pending_remote_settlement,
        true
    ));

    // Resolve TLC 1 (offered committed)
    state.tlc_state.set_offered_tlc_removed(
        1,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage_1,
        }),
    );
    assert!(has_unresolved_onchain_tlcs_for_snapshot(
        &state,
        &preceding_remote_settlement,
        true
    ));
    assert!(has_unresolved_onchain_tlcs_for_snapshot(
        &state,
        &pending_remote_settlement,
        true
    ));

    // Resolve TLC 2 (received committed)
    state.tlc_state.set_received_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &TEST_SHARED_SECRET,
        )),
    );

    // Preceding commitment now has NO unresolved TLCs (TLC 0 LocalAnnounced was not in it)
    assert!(
        !has_unresolved_onchain_tlcs_for_snapshot(&state, &preceding_remote_settlement, true),
        "preceding remote commitment should be fully resolved once TLC 1 & 2 are settled"
    );

    // Pending commitment STILL has unresolved TLC 0 (LocalAnnounced)
    assert!(
        has_unresolved_onchain_tlcs_for_snapshot(&state, &pending_remote_settlement, true),
        "pending remote commitment must still wait for TLC 0 LocalAnnounced to resolve"
    );

    // Resolve TLC 0 (offered LocalAnnounced)
    state.tlc_state.set_offered_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &TEST_SHARED_SECRET,
        )),
    );

    // Now pending commitment is also fully resolved
    assert!(
        !has_unresolved_onchain_tlcs_for_snapshot(&state, &pending_remote_settlement, true),
        "pending remote commitment should be fully resolved once TLC 0 is also settled"
    );
}

#[test]
fn multiple_concurrent_tlcs_local_force_close_resolution() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let tlc_0 = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        hash_algorithm,
    );

    let preimage_1 = gen_rand_sha256_hash();
    let tlc_1 = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        payment_hash_for(preimage_1, hash_algorithm),
        hash_algorithm,
    );

    let preimage_2 = gen_rand_sha256_hash();
    let tlc_2 = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitAck),
        payment_hash_for(preimage_2, hash_algorithm),
        hash_algorithm,
    );

    let preimage_3 = gen_rand_sha256_hash();
    let tlc_3 = tlc_info(
        TLCId::Received(1),
        TlcStatus::Inbound(InboundTlcStatus::Committed),
        payment_hash_for(preimage_3, hash_algorithm),
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    state.tlc_state.offered_tlcs.tlcs = vec![tlc_0, tlc_1];
    state.tlc_state.received_tlcs.tlcs = vec![tlc_2, tlc_3];

    // Local force close should block while committed/announced-received TLCs are unresolved
    assert!(
        has_unresolved_onchain_tlcs(&state),
        "local force close has active committed & AnnounceWaitAck TLCs"
    );

    // Resolve TLC 1 (offered committed)
    state.tlc_state.set_offered_tlc_removed(
        1,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage_1,
        }),
    );
    assert!(has_unresolved_onchain_tlcs(&state));

    // Resolve TLC 2 (received AnnounceWaitAck)
    state.tlc_state.set_received_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage_2,
        }),
    );
    assert!(has_unresolved_onchain_tlcs(&state));

    // Resolve TLC 3 (received committed)
    state.tlc_state.set_received_tlc_removed(
        1,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage_3,
        }),
    );

    // Now all local commitment TLCs are resolved. TLC 0 (Offered LocalAnnounced) is omitted
    // from local commitment and must not block finalization!
    assert!(
        !has_unresolved_onchain_tlcs(&state),
        "local force close should not be blocked by offered LocalAnnounced TLC"
    );
}
