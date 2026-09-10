#[cfg(feature = "watchtower")]
use std::collections::HashMap;
#[cfg(feature = "watchtower")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "watchtower")]
use std::sync::{Arc, Mutex};

use ckb_hash::blake2b_256;
use ckb_sdk::util::blake160;
use ckb_types::core::TransactionBuilder;
#[cfg(feature = "watchtower")]
use ckb_types::core::TransactionView;
#[cfg(feature = "watchtower")]
use ckb_types::packed::{CellDep, CellInput, OutPoint};
use ckb_types::packed::{CellOutput, Script};
use ckb_types::prelude::*;
#[cfg(feature = "watchtower")]
use fiber_types::NodeId;
use fiber_types::{
    AppliedFlags, ChannelBasePublicKeys, ChannelData, ChannelState, CloseFlags, CommitmentNumbers,
    Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus, Privkey, Pubkey, RemoveTlcFulfill,
    RemoveTlcReason, RetryableTlcOperation, RevocationData, SettlementData, SettlementTlc,
    ShutdownSettlementRecord, TLCId, TlcErr, TlcErrPacket, TlcErrorCode, TlcInfo, TlcStatus,
};
use musig2::{secp::Point, CompactSignature, KeyAggContext};
use ractor::Actor;
#[cfg(feature = "watchtower")]
use ractor::{ActorProcessingErr, ActorRef};

#[cfg(feature = "watchtower")]
use crate::ckb::client::{CkbChainClient, GetShutdownTxResponse, GetTxResponse};
use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::fiber::channel::{
    settlement_data_to_witness, ChannelActor, ChannelActorMessage, ChannelActorStateStore,
    ChannelCommand, ChannelEvent, ChannelInitializationOperation, ChannelInitializationParameter,
};
use crate::fiber::network::{NetworkActorCommand, NetworkActorEvent, NetworkActorMessage};
use crate::fiber::onchain_tlc_reconcile::{
    can_reconcile_onchain_fulfillment, collect_onchain_fulfilled_tlcs,
    collect_onchain_received_timeout_settled_tlcs, collect_onchain_timeout_settled_tlcs,
    has_unresolved_onchain_tlcs, has_unresolved_onchain_tlcs_for_snapshot, parse_commitment_lock,
    recover_shutdown_settlement_data, resolve_onchain_tlc, settlement_data_for_commitment,
    tracked_settlement_tlcs, verify_and_select_settlement_data, LegacyOnChainTlcSettlement,
    OnChainTimeoutTlcRole, OnChainTlcResolution,
};
use crate::fiber::tests::settle_tlc_set_command_tests::{
    create_test_channel_state_with_tlc, MockStore,
};
#[cfg(feature = "watchtower")]
use crate::fiber::{
    network::check_channel_shutdown_settlement, onchain_tlc_reconcile::OnChainTlcSettlement,
};
use crate::store::open_store;
use crate::tests::test_utils::{NetworkNode, NetworkNodeConfigBuilder};
// Browser tests disable Watchtower; gate its integration fixtures with the same feature.
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;
use crate::{gen_rand_fiber_public_key, gen_rand_sha256_hash};

const TEST_SHARED_SECRET: [u8; 32] = [7u8; 32];

async fn wait_for_settlement_completion(node: &NetworkNode, channel_id: Hash256) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state = node.store.get_channel_actor_state(&channel_id).unwrap();
            if matches!(state.state, ChannelState::Closed(flags)
                if !flags.intersects(CloseFlags::WAITING_ONCHAIN_SETTLEMENT | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED))
                && node.store.get_shutdown_settlement_record(&channel_id).is_none()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("production reconciliation must complete and clean up the snapshot");
}

#[cfg(feature = "watchtower")]
fn trigger_shutdown_check(node: &NetworkNode) {
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannelsShutdown,
        ))
        .unwrap();
}

fn install_unit_snapshot(
    state: &mut crate::fiber::channel::ChannelActorState,
    store: &MockStore,
    for_remote: bool,
    settlement_data: SettlementData,
) {
    let tx_hash = state
        .shutdown_transaction_hash
        .clone()
        .unwrap_or_else(|| gen_rand_sha256_hash().into());
    state.shutdown_transaction_hash = Some(tx_hash.clone());
    store.store_shutdown_settlement_record(
        &state.get_id(),
        &ShutdownSettlementRecord {
            shutdown_tx_hash: tx_hash,
            for_remote,
            commitment_number: 1,
            settlement_data,
        },
    );
}

fn payment_hash_for(preimage: Hash256, hash_algorithm: HashAlgorithm) -> Hash256 {
    hash_algorithm.hash(preimage).into()
}

fn payment_hash_with_invalid_full_hash(
    preimage: Hash256,
    hash_algorithm: HashAlgorithm,
) -> Hash256 {
    let discovered_hash = hash_algorithm.hash(preimage);
    let mut payment_hash = discovered_hash;
    payment_hash[31] ^= 1;
    payment_hash.into()
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
fn resolve_prefix_valid_full_hash_invalid_preimage_is_failed_settlement() {
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let payment_hash = payment_hash_with_invalid_full_hash(preimage, hash_algorithm);
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
        OnChainTlcResolution::SettledWithInvalidPreimage
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
fn collect_timeout_invalid_preimage_is_immediate_for_forwarded_and_origin_payer() {
    let channel_id = gen_rand_sha256_hash();
    let upstream_channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let forwarded_preimage = gen_rand_sha256_hash();
    let origin_preimage = gen_rand_sha256_hash();
    let forwarded_hash = payment_hash_with_invalid_full_hash(forwarded_preimage, hash_algorithm);
    let origin_hash = payment_hash_with_invalid_full_hash(origin_preimage, hash_algorithm);

    let mut forwarded = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        forwarded_hash,
        hash_algorithm,
    );
    forwarded.expiry = 1_000;
    forwarded.forwarding_tlc = Some((upstream_channel_id, 42));

    let mut origin = tlc_info(
        TLCId::Offered(1),
        TlcStatus::Outbound(OutboundTlcStatus::Committed),
        origin_hash,
        hash_algorithm,
    );
    origin.expiry = 1_000;

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.offered_tlcs.tlcs = vec![forwarded, origin];
    let store = MockStore::new()
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(0),
            forwarded_hash,
            hash_algorithm,
            forwarded_preimage,
        )
        .with_onchain_preimage(
            channel_id,
            TLCId::Offered(1),
            origin_hash,
            hash_algorithm,
            origin_preimage,
        );

    let settled = collect_onchain_timeout_settled_tlcs(&state, &store, 100);

    assert_eq!(settled.len(), 2);
    assert_eq!(settled[0].tlc_id, TLCId::Offered(0));
    assert_eq!(
        settled[0].role,
        OnChainTimeoutTlcRole::Forwarded {
            forwarding_channel_id: upstream_channel_id,
            forwarding_tlc_id: 42,
        }
    );
    assert_eq!(settled[1].tlc_id, TLCId::Offered(1));
    assert_eq!(
        settled[1].role,
        OnChainTimeoutTlcRole::OriginPayer { attempt_id: None }
    );
}

#[test]
fn collect_received_timeout_accepts_invalid_preimage_settlement() {
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_with_invalid_full_hash(preimage, hash_algorithm);
    let tlc = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::Committed),
        payment_hash,
        hash_algorithm,
    );

    let mut state = empty_channel_state(channel_id);
    state.tlc_state.received_tlcs.tlcs = vec![tlc];
    let store = MockStore::new().with_onchain_preimage(
        channel_id,
        TLCId::Received(0),
        payment_hash,
        hash_algorithm,
        preimage,
    );

    let settled = collect_onchain_received_timeout_settled_tlcs(&state, &store);

    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].tlc_id, 0);
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
    let snapshot_store = MockStore::new();
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
    assert!(has_unresolved_onchain_tlcs(&state, &snapshot_store));
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
    let snapshot_store = MockStore::new();
    let channel_id = gen_rand_sha256_hash();
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );

    let snapshot = state.build_settlement_data(false).unwrap();
    assert!(snapshot.tlcs.is_empty());
    install_unit_snapshot(&mut state, &snapshot_store, false, snapshot);

    assert!(
        !has_unresolved_onchain_tlcs(&state, &snapshot_store),
        "a local force-close broadcasts the local commitment, which omits offered LocalAnnounced TLCs"
    );
}

#[test]
fn has_unresolved_falls_back_to_waiting_when_commitment_is_unknown() {
    let snapshot_store = MockStore::new();
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
        has_unresolved_onchain_tlcs(&state, &snapshot_store),
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
    let snapshot_store = MockStore::new();
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

    let tlcs = [TLCId::Offered(1), TLCId::Received(0), TLCId::Received(1)]
        .iter()
        .map(|id| {
            let mut tlc = settlement_tlc_for(state.tlc_state.get(id).unwrap());
            tlc.tlc_id = tlc.tlc_id.flip();
            tlc
        })
        .collect();
    install_unit_snapshot(
        &mut state,
        &snapshot_store,
        false,
        SettlementData {
            local_amount: 0,
            remote_amount: 0,
            tlcs,
        },
    );

    // Local force close should block while committed/announced-received TLCs are unresolved
    assert!(
        has_unresolved_onchain_tlcs(&state, &snapshot_store),
        "local force close has active committed & AnnounceWaitAck TLCs"
    );

    // Resolve TLC 1 (offered committed)
    state.tlc_state.set_offered_tlc_removed(
        1,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage_1,
        }),
    );
    assert!(has_unresolved_onchain_tlcs(&state, &snapshot_store));

    // Resolve TLC 2 (received AnnounceWaitAck)
    state.tlc_state.set_received_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage_2,
        }),
    );
    assert!(has_unresolved_onchain_tlcs(&state, &snapshot_store));

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
        !has_unresolved_onchain_tlcs(&state, &snapshot_store),
        "local force close should not be blocked by offered LocalAnnounced TLC"
    );
}

#[test]
fn regression_remote_force_close_with_received_announce_wait_prev_ack_does_not_block() {
    let snapshot_store = MockStore::new();
    let channel_id = gen_rand_sha256_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let preimage = gen_rand_sha256_hash();
    let payment_hash = payment_hash_for(preimage, hash_algorithm);
    let tlc = tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitPrevAck),
        payment_hash,
        hash_algorithm,
    );
    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    state.to_local_amount = 100_000_000;
    state.to_remote_amount = 100_000_000;
    state.tlc_state.received_tlcs.tlcs = vec![tlc];
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
    let (pending_tx, snapshot) = state
        .build_commitment_tx_and_settlement_data(true)
        .expect("build pending commitment tx");
    state.shutdown_transaction_hash = Some(pending_tx.hash().unpack());
    install_unit_snapshot(&mut state, &snapshot_store, true, snapshot);

    assert!(
        !has_unresolved_onchain_tlcs(&state, &snapshot_store),
        "received AnnounceWaitPrevAck is not included in remote commitment snapshot and must not block settlement"
    );
}

#[test]
fn test_parse_commitment_lock_direction_and_validation() {
    let local_privkey = Privkey::from([1u8; 32]);
    let remote_privkey = Privkey::from([2u8; 32]);
    let local_funding_pubkey = local_privkey.pubkey();
    let remote_funding_pubkey = remote_privkey.pubkey();

    // Remote commitment lock: local_funding first, then remote_funding
    let remote_ctx = KeyAggContext::new([local_funding_pubkey, remote_funding_pubkey]).unwrap();
    let remote_xonly = remote_ctx.aggregated_pubkey::<Point>().serialize_xonly();
    let remote_lock_pubkey_hash = &blake2b_256(remote_xonly)[0..20];

    let delay_epoch = 100u64;
    let commitment_number = 42u64;
    let witness_hash = [9u8; 20];

    let mut remote_lock_args = Vec::new();
    remote_lock_args.extend_from_slice(remote_lock_pubkey_hash);
    remote_lock_args.extend_from_slice(&delay_epoch.to_be_bytes());
    remote_lock_args.extend_from_slice(&commitment_number.to_be_bytes());
    remote_lock_args.extend_from_slice(&witness_hash);
    remote_lock_args.push(0x00);

    let remote_lock = get_script_by_contract(Contract::CommitmentLock, &remote_lock_args);

    let parsed = parse_commitment_lock(&remote_lock, &local_funding_pubkey, &remote_funding_pubkey)
        .expect("should parse remote commitment lock");
    assert!(parsed.for_remote);
    assert_eq!(parsed.commitment_number, 42);
    assert_eq!(parsed.witness_hash, witness_hash);

    // Local commitment lock: remote_funding first, then local_funding
    let local_ctx = KeyAggContext::new([remote_funding_pubkey, local_funding_pubkey]).unwrap();
    let local_xonly = local_ctx.aggregated_pubkey::<Point>().serialize_xonly();
    let local_lock_pubkey_hash = &blake2b_256(local_xonly)[0..20];

    let mut local_lock_args = Vec::new();
    local_lock_args.extend_from_slice(local_lock_pubkey_hash);
    local_lock_args.extend_from_slice(&delay_epoch.to_be_bytes());
    local_lock_args.extend_from_slice(&commitment_number.to_be_bytes());
    local_lock_args.extend_from_slice(&witness_hash);
    local_lock_args.push(0x00);

    let local_lock = get_script_by_contract(Contract::CommitmentLock, &local_lock_args);

    let parsed_local =
        parse_commitment_lock(&local_lock, &local_funding_pubkey, &remote_funding_pubkey)
            .expect("should parse local commitment lock");
    assert!(!parsed_local.for_remote);
    assert_eq!(parsed_local.commitment_number, 42);
    assert_eq!(parsed_local.witness_hash, witness_hash);

    // Corrupted / too short args
    let too_short_lock = get_script_by_contract(Contract::CommitmentLock, &[0u8; 35]);
    assert!(parse_commitment_lock(
        &too_short_lock,
        &local_funding_pubkey,
        &remote_funding_pubkey
    )
    .is_none());

    // Wrong pubkeys
    let unrelated_pubkey = Privkey::from([3u8; 32]).pubkey();
    assert!(
        parse_commitment_lock(&remote_lock, &unrelated_pubkey, &remote_funding_pubkey).is_none()
    );
}

#[test]
fn test_verify_and_select_settlement_data_preceding_and_pending() {
    let local_privkey = Privkey::from([1u8; 32]);
    let remote_privkey = Privkey::from([2u8; 32]);
    let local_settlement_key = Privkey::from([3u8; 32]);
    let remote_settlement_key = Privkey::from([4u8; 32]).pubkey();
    let channel_id = gen_rand_sha256_hash();

    let preceding_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 2000,
        tlcs: vec![],
    };
    let pending_settlement = SettlementData {
        local_amount: 500,
        remote_amount: 2500,
        tlcs: vec![],
    };
    let local_settlement = SettlementData {
        local_amount: 1500,
        remote_amount: 1500,
        tlcs: vec![],
    };

    let channel_data = ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: local_settlement_key.clone(),
        remote_settlement_key,
        local_funding_pubkey: local_privkey.pubkey(),
        remote_funding_pubkey: remote_privkey.pubkey(),
        remote_settlement_data: preceding_settlement.clone(),
        pending_remote_settlement_data: pending_settlement.clone(),
        local_settlement_data: local_settlement.clone(),
        revocation_data: Some(RevocationData {
            commitment_number: 10,
            aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Default::default(),
        }),
    };

    // A revoked remote lock needs no historical TLC snapshot. The same number in
    // the local direction, or a newer remote lock, must still pass hash binding.
    for (for_remote, number, accepted) in [
        (true, 9, true),
        (true, 10, true),
        (true, 11, false),
        (false, 9, false),
    ] {
        let mut lock = create_test_commitment_lock_with_keys(
            &local_privkey,
            &remote_privkey,
            &local_settlement_key,
            remote_settlement_key,
            &preceding_settlement,
            for_remote,
            number,
        );
        let mut args = lock.args().raw_data().to_vec();
        args[40] ^= 0xff;
        lock = lock.as_builder().args(args.pack()).build();
        assert!(verify_and_select_settlement_data(&channel_data, &lock).is_none());
        let recovered = recover_shutdown_settlement_data(&channel_data, &lock);
        assert_eq!(recovered.is_some(), accepted);
        if let Some((remote, recovered_number, scope)) = recovered {
            assert!(remote);
            assert_eq!(recovered_number, number);
            assert!(scope.tlcs.is_empty());
        }
    }

    // Revocation 10 leaves preceding 11 and pending 12; local uses its own snapshot.
    for (for_remote, number, expected) in [
        (true, 11, &preceding_settlement),
        (true, 12, &pending_settlement),
        (false, 11, &local_settlement),
    ] {
        let lock = create_test_commitment_lock_with_keys(
            &local_privkey,
            &remote_privkey,
            &local_settlement_key,
            remote_settlement_key,
            expected,
            for_remote,
            number,
        );
        let (actual_direction, actual_number, selected) =
            verify_and_select_settlement_data(&channel_data, &lock)
                .expect("commitment lock should verify");
        assert_eq!(actual_direction, for_remote);
        assert_eq!(actual_number, number);
        assert_eq!(selected, expected);

        let mut tampered_args = lock.args().raw_data().to_vec();
        tampered_args[40] ^= 0xff;
        let tampered_lock = get_script_by_contract(Contract::CommitmentLock, &tampered_args);
        assert!(verify_and_select_settlement_data(&channel_data, &tampered_lock).is_none());
    }
}

#[test]
fn test_tracked_settlement_tlcs_extraction() {
    let local_privkey = Privkey::from([1u8; 32]);
    let remote_privkey = Privkey::from([2u8; 32]);
    let local_settlement_key = Privkey::from([3u8; 32]);
    let remote_settlement_key = Privkey::from([4u8; 32]).pubkey();
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();

    let settlement_tlc = SettlementTlc {
        tlc_id: TLCId::Offered(5),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 3000,
        payment_hash,
        expiry: 200,
        local_key: Privkey::from([5u8; 32]),
        remote_key: Privkey::from([6u8; 32]).pubkey(),
    };

    let pending_settlement = SettlementData {
        local_amount: 500,
        remote_amount: 2500,
        tlcs: vec![settlement_tlc],
    };

    let channel_data = ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: local_settlement_key.clone(),
        remote_settlement_key,
        local_funding_pubkey: local_privkey.pubkey(),
        remote_funding_pubkey: remote_privkey.pubkey(),
        remote_settlement_data: SettlementData {
            local_amount: 0,
            remote_amount: 0,
            tlcs: vec![],
        },
        pending_remote_settlement_data: pending_settlement.clone(),
        local_settlement_data: SettlementData {
            local_amount: 0,
            remote_amount: 0,
            tlcs: vec![],
        },
        revocation_data: None,
    };

    // Commitment 2 = pending
    let pending_witness = settlement_data_to_witness(
        &pending_settlement,
        true,
        local_settlement_key,
        remote_settlement_key,
    );
    let pending_witness_hash = blake160(&pending_witness).0;

    let remote_ctx = KeyAggContext::new([local_privkey.pubkey(), remote_privkey.pubkey()]).unwrap();
    let remote_xonly = remote_ctx.aggregated_pubkey::<Point>().serialize_xonly();
    let remote_lock_pubkey_hash = &blake2b_256(remote_xonly)[0..20];

    let mut lock_args = Vec::new();
    lock_args.extend_from_slice(remote_lock_pubkey_hash);
    lock_args.extend_from_slice(&100u64.to_be_bytes());
    lock_args.extend_from_slice(&2u64.to_be_bytes());
    lock_args.extend_from_slice(&pending_witness_hash);
    lock_args.push(0x00);

    let lock = get_script_by_contract(Contract::CommitmentLock, &lock_args);

    let tracked = tracked_settlement_tlcs(&lock, &channel_data, true)
        .expect("should extract tracked settlement tlcs");
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0].tlc_id, TLCId::Offered(5));
    assert_eq!(tracked[0].payment_hash, payment_hash);

    // Direction mismatch: expected local, but lock is remote
    assert!(tracked_settlement_tlcs(&lock, &channel_data, false).is_none());
}

#[derive(Clone, Default)]
#[cfg(feature = "watchtower")]
struct MockSettlementChainClient {
    transactions: Arc<Mutex<HashMap<ckb_types::H256, Result<GetTxResponse, String>>>>,
    cells: Arc<Mutex<Vec<ckb_sdk::rpc::ckb_indexer::Cell>>>,
    get_tx_call_count: Arc<AtomicUsize>,
}

#[cfg(feature = "watchtower")]
impl MockSettlementChainClient {
    fn new() -> Self {
        Self::default()
    }

    fn set_tx(&self, hash: ckb_types::H256, tx: TransactionView) {
        self.transactions.lock().unwrap().insert(
            hash,
            Ok(GetTxResponse {
                transaction: Some(tx),
                tx_status: ckb_types::core::tx_pool::TxStatus::Unknown,
            }),
        );
    }

    fn set_tx_err(&self, hash: ckb_types::H256, err: &str) {
        self.transactions
            .lock()
            .unwrap()
            .insert(hash, Err(err.to_string()));
    }

    fn get_tx_call_count(&self) -> usize {
        self.get_tx_call_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
#[cfg(feature = "watchtower")]
impl CkbChainClient for MockSettlementChainClient {
    async fn get_transaction(&self, hash: ckb_types::H256) -> Result<GetTxResponse, anyhow::Error> {
        self.get_tx_call_count.fetch_add(1, Ordering::SeqCst);
        let txs = self.transactions.lock().unwrap();
        match txs.get(&hash) {
            Some(Ok(resp)) => Ok(resp.clone()),
            Some(Err(err)) => Err(anyhow::anyhow!("{}", err)),
            None => Ok(GetTxResponse::default()),
        }
    }

    async fn get_cells(
        &self,
        _search_key: ckb_sdk::rpc::ckb_indexer::SearchKey,
        _order: ckb_sdk::rpc::ckb_indexer::Order,
        _limit: u32,
        _after: Option<ckb_jsonrpc_types::JsonBytes>,
    ) -> Result<ckb_sdk::rpc::ckb_indexer::Pagination<ckb_sdk::rpc::ckb_indexer::Cell>, anyhow::Error>
    {
        let cells = self.cells.lock().unwrap().clone();
        Ok(ckb_sdk::rpc::ckb_indexer::Pagination {
            objects: cells,
            last_cursor: ckb_jsonrpc_types::JsonBytes::from_bytes(ckb_types::bytes::Bytes::new()),
        })
    }

    async fn get_block_timestamp(
        &self,
        _block_hash: Hash256,
    ) -> Result<Option<u64>, anyhow::Error> {
        Ok(None)
    }

    async fn get_shutdown_tx(
        &self,
        _funding_lock_script: Script,
    ) -> Result<Option<GetShutdownTxResponse>, anyhow::Error> {
        Ok(None)
    }
}

#[cfg(feature = "watchtower")]
struct MessageCollectorActor(tokio::sync::mpsc::UnboundedSender<NetworkActorMessage>);

#[async_trait::async_trait]
#[cfg(feature = "watchtower")]
impl Actor for MessageCollectorActor {
    type Msg = NetworkActorMessage;
    type State = ();
    type Arguments = ();
    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }
    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _ = self.0.send(message);
        Ok(())
    }
}

fn create_test_commitment_lock_with_keys(
    local_privkey: &Privkey,
    remote_privkey: &Privkey,
    local_settlement_privkey: &Privkey,
    remote_settlement_pubkey: Pubkey,
    settlement_data: &SettlementData,
    for_remote: bool,
    commitment_number: u64,
) -> Script {
    let witness = settlement_data_to_witness(
        settlement_data,
        for_remote,
        local_settlement_privkey.clone(),
        remote_settlement_pubkey,
    );
    let witness_hash = blake160(&witness).0;

    let (pubkey1, pubkey2) = if for_remote {
        (local_privkey.pubkey(), remote_privkey.pubkey())
    } else {
        (remote_privkey.pubkey(), local_privkey.pubkey())
    };
    let ctx = KeyAggContext::new([pubkey1, pubkey2]).unwrap();
    let xonly = ctx.aggregated_pubkey::<Point>().serialize_xonly();
    let lock_pubkey_hash = &blake2b_256(xonly)[0..20];

    let mut lock_args = Vec::new();
    lock_args.extend_from_slice(lock_pubkey_hash);
    lock_args.extend_from_slice(&100u64.to_be_bytes());
    lock_args.extend_from_slice(&commitment_number.to_be_bytes());
    lock_args.extend_from_slice(&witness_hash);
    lock_args.push(0x00);

    get_script_by_contract(Contract::CommitmentLock, &lock_args)
}

#[cfg(feature = "watchtower")]
fn create_test_commitment_tx(
    funding_outpoint: OutPoint,
    lock: Script,
    extra_cell_deps: Vec<CellDep>,
) -> TransactionView {
    let mut builder = TransactionBuilder::default()
        .input(CellInput::new(funding_outpoint, 0))
        .output(
            CellOutput::new_builder()
                .lock(lock)
                .capacity(100_000_000u64)
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default());
    for dep in extra_cell_deps {
        builder = builder.cell_dep(dep);
    }
    builder.build()
}

#[cfg(feature = "watchtower")]
async fn assert_confirmed_snapshot_recovery(wrong_direction: bool, revoked: bool) {
    let mut node = NetworkNode::new().await;
    let store = node.store.clone();
    let channel_id = gen_rand_sha256_hash();

    let local_privkey = Privkey::from_slice(&[11u8; 32]);
    let remote_privkey = Privkey::from_slice(&[22u8; 32]);
    let local_settlement_privkey = Privkey::from_slice(&[33u8; 32]);
    let remote_settlement_pubkey = Privkey::from_slice(&[44u8; 32]).pubkey();

    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    tlc.created_at = CommitmentNumbers {
        local: 10,
        remote: 10,
    };

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        tlc.clone(),
    );
    let funding_outpoint = state.get_funding_transaction_outpoint().unwrap();

    let preceding_settlement = SettlementData {
        local_amount: 100_000_000,
        remote_amount: 100_000_000,
        tlcs: vec![],
    };
    let pending_settlement = SettlementData {
        local_amount: 100_000_000,
        remote_amount: 100_000_000,
        tlcs: vec![settlement_tlc_for(&tlc)],
    };

    // Store watchtower channel data with commitment 11 (preceding) having no TLCs,
    // and commitment 12 (pending) having TLC 0
    store.insert_watch_channel(
        NodeId::local(),
        channel_id,
        None,
        local_settlement_privkey.clone(),
        remote_settlement_pubkey,
        local_privkey.pubkey(),
        remote_privkey.pubkey(),
        pending_settlement.clone(),
    );
    store.update_revocation(
        NodeId::local(),
        channel_id,
        RevocationData {
            commitment_number: 10,
            aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Default::default(),
        },
        preceding_settlement.clone(),
    );

    // Chain commitment transaction is for commitment 11 (preceding)
    let preceding_lock = create_test_commitment_lock_with_keys(
        &local_privkey,
        &remote_privkey,
        &local_settlement_privkey,
        remote_settlement_pubkey,
        &preceding_settlement,
        true,
        if revoked { 9 } else { 11 },
    );
    let preceding_tx = create_test_commitment_tx(funding_outpoint, preceding_lock, vec![]);
    let preceding_tx_hash: ckb_types::H256 = preceding_tx.hash().unpack();

    state.shutdown_transaction_hash = Some(preceding_tx_hash.clone());
    store.insert_channel_actor_state(state.clone());

    if wrong_direction {
        store.store_shutdown_settlement_record(
            &channel_id,
            &ShutdownSettlementRecord {
                shutdown_tx_hash: preceding_tx_hash.clone(),
                for_remote: false,
                commitment_number: 11,
                settlement_data: pending_settlement,
            },
        );
    }
    node.chain_client.state.write().unwrap().txs.insert(
        preceding_tx_hash.into(),
        GetTxResponse {
            transaction: Some(preceding_tx),
            ..Default::default()
        },
    );
    // Exercise the production scheduler, RPC recovery, recovered-event delivery,
    // offline reconciliation and cleanup. The test never clears flags itself.
    trigger_shutdown_check(&node);
    wait_for_settlement_completion(&node, channel_id).await;
    node.stop().await;
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_scenario_a_recovers_snapshot_and_clears_flags_when_already_confirmed() {
    assert_confirmed_snapshot_recovery(false, false).await;
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn review_scheduler_recovers_wrong_direction_snapshot() {
    assert_confirmed_snapshot_recovery(true, false).await;
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_scenario_b_transient_rpc_failure_retries_and_recovers_under_confirmed() {
    let temp_dir = tempfile::tempdir().unwrap();
    let store = open_store(temp_dir.path()).expect("open store");
    let channel_id = gen_rand_sha256_hash();

    let local_privkey = Privkey::from_slice(&[11u8; 32]);
    let remote_privkey = Privkey::from_slice(&[22u8; 32]);
    let local_settlement_privkey = Privkey::from_slice(&[33u8; 32]);
    let remote_settlement_pubkey = Privkey::from_slice(&[44u8; 32]).pubkey();

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        tlc_info(
            TLCId::Offered(0),
            TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
            gen_rand_sha256_hash(),
            HashAlgorithm::CkbHash,
        ),
    );
    let funding_outpoint = state.get_funding_transaction_outpoint().unwrap();

    let settlement = SettlementData {
        local_amount: 100_000_000,
        remote_amount: 100_000_000,
        tlcs: vec![],
    };
    store.insert_watch_channel(
        NodeId::local(),
        channel_id,
        None,
        local_settlement_privkey.clone(),
        remote_settlement_pubkey,
        local_privkey.pubkey(),
        remote_privkey.pubkey(),
        settlement.clone(),
    );

    let lock = create_test_commitment_lock_with_keys(
        &local_privkey,
        &remote_privkey,
        &local_settlement_privkey,
        remote_settlement_pubkey,
        &settlement,
        true,
        0,
    );
    let tx = create_test_commitment_tx(funding_outpoint, lock, vec![]);
    let tx_hash: ckb_types::H256 = tx.hash().unpack();

    state.shutdown_transaction_hash = Some(tx_hash.clone());
    store.insert_channel_actor_state(state.clone());

    let chain_client = MockSettlementChainClient::new();
    let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (myself, _) = Actor::spawn(None, MessageCollectorActor(sender), ())
        .await
        .expect("spawn collector");

    // 1st attempt: RPC returns error
    chain_client.set_tx_err(tx_hash.clone(), "RPC connection timeout");
    check_channel_shutdown_settlement(
        chain_client.clone(),
        myself.clone(),
        state.clone(),
        store.clone(),
    )
    .await;

    assert_eq!(chain_client.get_tx_call_count(), 1);
    assert!(
        store.get_shutdown_settlement_record(&channel_id).is_none(),
        "failed RPC must not store snapshot record"
    );
    assert!(
        rx.try_recv().is_err(),
        "no recovered message sent on failure"
    );

    // 2nd attempt: RPC succeeds
    chain_client.set_tx(tx_hash.clone(), tx);
    check_channel_shutdown_settlement(
        chain_client.clone(),
        myself.clone(),
        state.clone(),
        store.clone(),
    )
    .await;

    assert_eq!(chain_client.get_tx_call_count(), 2);
    let record = store
        .get_shutdown_settlement_record(&channel_id)
        .expect("second RPC call must recover snapshot record");
    assert_eq!(record.shutdown_tx_hash, tx_hash);

    let event = rx.recv().await.expect("receive message");
    assert!(matches!(
        event,
        NetworkActorMessage::Event(NetworkActorEvent::ChannelSettlementRecovered(..))
    ));
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_scenario_c_cell_dep_pending_tx_recovers_snapshot_and_blocks_until_tlc_resolved() {
    let temp_dir = tempfile::tempdir().unwrap();
    let store = open_store(temp_dir.path()).expect("open store");
    let mut node = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .build()
            .with_store(store.clone()),
    )
    .await;
    let preimage = gen_rand_sha256_hash();
    let channel_id = gen_rand_sha256_hash();

    let local_privkey = Privkey::from_slice(&[11u8; 32]);
    let remote_privkey = Privkey::from_slice(&[22u8; 32]);
    let local_settlement_privkey = Privkey::from_slice(&[33u8; 32]);
    let remote_settlement_pubkey = Privkey::from_slice(&[44u8; 32]).pubkey();

    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash_for(preimage, HashAlgorithm::CkbHash),
        HashAlgorithm::CkbHash,
    );
    tlc.created_at = CommitmentNumbers {
        local: 1,
        remote: 1,
    };

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc.clone(),
    );
    let funding_outpoint = state.get_funding_transaction_outpoint().unwrap();

    let pending_settlement = SettlementData {
        local_amount: 100_000_000,
        remote_amount: 100_000_000,
        tlcs: vec![settlement_tlc_for(&tlc)],
    };

    store.insert_watch_channel(
        NodeId::local(),
        channel_id,
        None,
        local_settlement_privkey.clone(),
        remote_settlement_pubkey,
        local_privkey.pubkey(),
        remote_privkey.pubkey(),
        pending_settlement.clone(),
    );

    // Commitment lock for commitment 2 (pending)
    let pending_lock = create_test_commitment_lock_with_keys(
        &local_privkey,
        &remote_privkey,
        &local_settlement_privkey,
        remote_settlement_pubkey,
        &pending_settlement,
        true,
        2,
    );

    // Real broadcast tx has additional cell_dep (simulating UDT/funding cell deps)
    let extra_dep = CellDep::new_builder()
        .out_point(OutPoint::new(gen_rand_sha256_hash().into(), 0))
        .build();
    let pending_tx_with_deps =
        create_test_commitment_tx(funding_outpoint, pending_lock, vec![extra_dep]);
    let pending_tx_hash: ckb_types::H256 = pending_tx_with_deps.hash().unpack();

    state.shutdown_transaction_hash = Some(pending_tx_hash.clone());
    store.insert_channel_actor_state(state.clone());

    let chain_client = MockSettlementChainClient::new();
    chain_client.set_tx(pending_tx_hash.clone(), pending_tx_with_deps);

    check_channel_shutdown_settlement(
        chain_client,
        node.network_actor.clone(),
        state.clone(),
        store.clone(),
    )
    .await;
    // A network RPC is a mailbox barrier after the recovered/completed events.
    node.node_info().await;
    assert!(store
        .get_channel_actor_state(&channel_id)
        .unwrap()
        .is_waiting_onchain_settlement());

    // Assert the exact recovered snapshot
    let record = store
        .get_shutdown_settlement_record(&channel_id)
        .expect("pending snapshot record must be persisted");
    assert_eq!(record.shutdown_tx_hash, pending_tx_hash);
    assert_eq!(record.commitment_number, 2);
    assert!(record.for_remote);
    assert_eq!(record.settlement_data.tlcs.len(), 1);
    assert_eq!(record.settlement_data.tlcs[0].tlc_id, TLCId::Offered(0));

    // Supplying the on-chain proof must cause production reconciliation to
    // remove the TLC, persist the terminal state and clean up the record.
    store.insert_onchain_tlc_settlement(
        &NodeId::local(),
        &channel_id,
        TLCId::Offered(0),
        OnChainTlcSettlement {
            payment_hash: tlc.payment_hash,
            hash_algorithm: tlc.hash_algorithm,
            preimage: Some(preimage),
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannels,
        ))
        .unwrap();
    wait_for_settlement_completion(&node, channel_id).await;
    let finished = store.get_channel_actor_state(&channel_id).unwrap();
    assert!(matches!(
        finished
            .tlc_state
            .get(&TLCId::Offered(0))
            .unwrap()
            .removed_reason,
        Some(RemoveTlcReason::RemoveTlcFulfill(_))
    ));
    node.stop().await;
}

#[test]
#[cfg(feature = "watchtower")]
fn test_scenario_d_multi_tenant_watchtower_store_isolation() {
    let temp_dir = tempfile::tempdir().unwrap();
    let store = open_store(temp_dir.path()).expect("open store");
    let channel_id = gen_rand_sha256_hash();

    let local_node = NodeId::local();
    let foreign_node = NodeId::from_bytes(vec![42u8; 32]);

    let local_priv = Privkey::from_slice(&[1u8; 32]);
    let remote_pub = Privkey::from_slice(&[2u8; 32]).pubkey();
    let local_funding = Privkey::from_slice(&[3u8; 32]).pubkey();
    let remote_funding = Privkey::from_slice(&[4u8; 32]).pubkey();

    let local_settlement = SettlementData {
        local_amount: 11111,
        remote_amount: 22222,
        tlcs: vec![],
    };
    let foreign_settlement = SettlementData {
        local_amount: 99999,
        remote_amount: 88888,
        tlcs: vec![],
    };

    // Initially neither has the channel
    assert!(store.get_local_watch_channel(&channel_id).is_none());

    // Insert channel ONLY for foreign node
    store.insert_watch_channel(
        foreign_node,
        channel_id,
        None,
        local_priv.clone(),
        remote_pub,
        local_funding,
        remote_funding,
        foreign_settlement,
    );

    // get_local_watch_channel MUST NOT return foreign node's channel
    assert!(
        store.get_local_watch_channel(&channel_id).is_none(),
        "foreign node record must not be returned for local node"
    );

    // Now insert channel for local node with different settlement data
    store.insert_watch_channel(
        local_node,
        channel_id,
        None,
        local_priv,
        remote_pub,
        local_funding,
        remote_funding,
        local_settlement,
    );

    // get_local_watch_channel MUST return local node's channel data
    let loaded = store
        .get_local_watch_channel(&channel_id)
        .expect("should load local watch channel");
    assert_eq!(loaded.pending_remote_settlement_data.local_amount, 11111);
    assert_eq!(loaded.pending_remote_settlement_data.remote_amount, 22222);
}

#[tokio::test]
async fn test_scenario_e_real_db_persistence_and_reopen() {
    let temp_dir = tempfile::tempdir().unwrap();
    let channel_id = gen_rand_sha256_hash();
    let tx_hash: ckb_types::H256 = gen_rand_sha256_hash().into();

    let record = ShutdownSettlementRecord {
        shutdown_tx_hash: tx_hash.clone(),
        for_remote: true,
        commitment_number: 2,
        settlement_data: SettlementData {
            local_amount: 12345,
            remote_amount: 67890,
            tlcs: vec![],
        },
    };

    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_REMOTE
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
    );
    state.shutdown_transaction_hash = Some(tx_hash.clone());

    // 1. Open store, write record and channel state
    {
        let store = open_store(temp_dir.path()).expect("open store");
        store.store_shutdown_settlement_record(&channel_id, &record);
        store.insert_channel_actor_state(state.clone());
    } // store is dropped here, all DB handles released

    // 2. Reopen store from the same path
    {
        let store2 = open_store(temp_dir.path()).expect("reopen store");
        let loaded_record = store2
            .get_shutdown_settlement_record(&channel_id)
            .expect("loaded record");
        assert_eq!(loaded_record, record);

        let loaded_state = store2
            .get_channel_actor_state(&channel_id)
            .expect("loaded state");
        assert_eq!(loaded_state.id, channel_id);
        assert_eq!(
            loaded_state.shutdown_transaction_hash,
            Some(tx_hash.clone())
        );

        // Run real offline reconciliation against the reopened store;
        // it must load the persisted record and finish itself.
        let config = NetworkNodeConfigBuilder::new()
            .build()
            .with_store(store2.clone());
        let mut node = NetworkNode::new_with_config(config).await;
        node.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::CheckChannels,
            ))
            .unwrap();
        wait_for_settlement_completion(&node, channel_id).await;
        node.stop().await;
    }
}

#[tokio::test]
async fn test_scenario_f_live_actor_and_no_actor_recovery_and_finalization() {
    let temp_dir = tempfile::tempdir().unwrap();
    let store = open_store(temp_dir.path()).expect("open store");
    let config = NetworkNodeConfigBuilder::new()
        .build()
        .with_store(store.clone());
    let mut node = NetworkNode::new_with_config(config).await;

    // --- Part 1: Live actor path ---
    {
        let channel_id = gen_rand_sha256_hash();
        let tx_hash: ckb_types::H256 = gen_rand_sha256_hash().into();
        let local_privkey = Privkey::from_slice(&[11u8; 32]);
        let remote_pubkey = Privkey::from_slice(&[22u8; 32]).pubkey();

        let mut state = empty_channel_state(channel_id);
        state.state = ChannelState::Closed(
            CloseFlags::UNCOOPERATIVE_REMOTE
                | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
                | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        );
        state.shutdown_transaction_hash = Some(tx_hash.clone());
        #[cfg(feature = "watchtower")]
        for (id, preimage) in [(0, Some(gen_rand_sha256_hash())), (1, None)] {
            let payment_hash = preimage
                .map(|p| payment_hash_for(p, HashAlgorithm::CkbHash))
                .unwrap_or_else(gen_rand_sha256_hash);
            state.tlc_state.add_received_tlc(tlc_info(
                TLCId::Received(id),
                TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitPrevAck),
                payment_hash,
                HashAlgorithm::CkbHash,
            ));
            store.insert_onchain_tlc_settlement(
                &NodeId::local(),
                &channel_id,
                TLCId::Received(id),
                OnChainTlcSettlement {
                    payment_hash,
                    hash_algorithm: HashAlgorithm::CkbHash,
                    preimage,
                    tx_hash: gen_rand_sha256_hash(),
                    tlc_index: id as u8,
                },
            );
        }

        store.insert_channel_actor_state(state.clone());

        let record = ShutdownSettlementRecord {
            shutdown_tx_hash: tx_hash.clone(),
            for_remote: true,
            commitment_number: 1,
            settlement_data: SettlementData {
                local_amount: 100_000_000,
                remote_amount: 100_000_000,
                tlcs: vec![], // all TLCs resolved
            },
        };

        let channel_actor = ChannelActor::new(
            local_privkey.pubkey(),
            remote_pubkey,
            node.network_actor.clone(),
            store.clone(),
            None,
        );

        let (actor_ref, handle) = Actor::spawn(
            None,
            channel_actor,
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::RestoreOfflineChannel(channel_id),
                ephemeral_config: Default::default(),
                private_key: local_privkey,
            },
        )
        .await
        .expect("spawn channel actor");

        actor_ref
            .send_message(ChannelActorMessage::Event(
                ChannelEvent::OnChainSettlementCompleted,
            ))
            .unwrap();
        let mut invalid = record.clone();
        invalid.for_remote = false;
        actor_ref
            .send_message(ChannelActorMessage::Event(
                ChannelEvent::ShutdownSettlementRecovered(invalid),
            ))
            .unwrap();
        ractor::call!(actor_ref, |reply| ChannelActorMessage::Command(
            ChannelCommand::TestBarrier(reply)
        ))
        .unwrap();
        assert!(store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .is_waiting_onchain_settlement());
        store.store_shutdown_settlement_record(&channel_id, &record);

        ractor::call!(
            node.network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::InstallTestChannelActor(
                channel_id, actor_ref, reply
            ))
        )
        .unwrap();
        node.network_actor
            .send_message(NetworkActorMessage::Event(
                NetworkActorEvent::ChannelSettlementRecovered(channel_id, record),
            ))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("live actor must complete")
            .unwrap();

        // Verify channel state in store: waiting flags are cleared!
        let final_state = store
            .get_channel_actor_state(&channel_id)
            .expect("load final state");
        let ChannelState::Closed(flags) = final_state.state else {
            panic!()
        };
        assert!(!flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT));
        #[cfg(feature = "watchtower")]
        for id in [0, 1] {
            let tlc = final_state.tlc_state.get(&TLCId::Received(id)).unwrap();
            assert_eq!(tlc.inbound_status(), InboundTlcStatus::LocalRemoved);
            assert!(
                matches!(
                    &tlc.removed_reason,
                    Some(RemoveTlcReason::RemoveTlcFulfill(_))
                ) == (id == 0)
            );
            assert!(tlc.removed_reason.is_some());
        }

        assert!(!flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED));
        // Verify snapshot record was deleted after finalization
        assert!(store.get_shutdown_settlement_record(&channel_id).is_none());
    }

    // --- Part 2: No-actor path ---
    {
        let channel_id = gen_rand_sha256_hash();
        let tx_hash: ckb_types::H256 = gen_rand_sha256_hash().into();

        let mut state = empty_channel_state(channel_id);
        state.state = ChannelState::Closed(
            CloseFlags::UNCOOPERATIVE_REMOTE
                | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
                | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        );
        state.shutdown_transaction_hash = Some(tx_hash.clone());
        #[cfg(feature = "watchtower")]
        for (id, preimage) in [(0, Some(gen_rand_sha256_hash())), (1, None)] {
            let payment_hash = preimage
                .map(|p| payment_hash_for(p, HashAlgorithm::CkbHash))
                .unwrap_or_else(gen_rand_sha256_hash);
            state.tlc_state.add_received_tlc(tlc_info(
                TLCId::Received(id),
                TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitPrevAck),
                payment_hash,
                HashAlgorithm::CkbHash,
            ));
            store.insert_onchain_tlc_settlement(
                &NodeId::local(),
                &channel_id,
                TLCId::Received(id),
                OnChainTlcSettlement {
                    payment_hash,
                    hash_algorithm: HashAlgorithm::CkbHash,
                    preimage,
                    tx_hash: gen_rand_sha256_hash(),
                    tlc_index: id as u8,
                },
            );
        }

        store.insert_channel_actor_state(state.clone());

        let record = ShutdownSettlementRecord {
            shutdown_tx_hash: tx_hash.clone(),
            for_remote: true,
            commitment_number: 1,
            settlement_data: SettlementData {
                local_amount: 100_000_000,
                remote_amount: 100_000_000,
                tlcs: vec![],
            },
        };
        node.network_actor
            .send_message(NetworkActorMessage::Event(
                NetworkActorEvent::ChannelSettlementCompleted(channel_id),
            ))
            .unwrap();
        node.node_info().await;
        assert!(store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .is_waiting_onchain_settlement());

        store.store_shutdown_settlement_record(&channel_id, &record);

        node.network_actor
            .send_message(NetworkActorMessage::Event(
                NetworkActorEvent::ChannelSettlementRecovered(channel_id, record),
            ))
            .unwrap();
        wait_for_settlement_completion(&node, channel_id).await;

        let final_state = store.get_channel_actor_state(&channel_id).unwrap();
        let ChannelState::Closed(final_flags) = final_state.state else {
            panic!()
        };
        assert!(!final_flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT));
        #[cfg(feature = "watchtower")]
        for id in [0, 1] {
            let tlc = final_state.tlc_state.get(&TLCId::Received(id)).unwrap();
            assert_eq!(tlc.inbound_status(), InboundTlcStatus::LocalRemoved);
            assert_eq!(
                matches!(
                    &tlc.removed_reason,
                    Some(RemoveTlcReason::RemoveTlcFulfill(_))
                ),
                id == 0
            );
            assert!(tlc.removed_reason.is_some());
        }

        assert!(!final_flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED));
        assert!(store.get_shutdown_settlement_record(&channel_id).is_none());
    }
    node.stop().await;
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_scenario_g_malformed_script_wrong_funding_and_direction_mismatch_stay_waiting() {
    let temp_dir = tempfile::tempdir().unwrap();
    let store = open_store(temp_dir.path()).expect("open store");
    let channel_id = gen_rand_sha256_hash();

    let local_privkey = Privkey::from_slice(&[11u8; 32]);
    let remote_privkey = Privkey::from_slice(&[22u8; 32]);
    let local_settlement_privkey = Privkey::from_slice(&[33u8; 32]);
    let remote_settlement_pubkey = Privkey::from_slice(&[44u8; 32]).pubkey();

    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc_info(
            TLCId::Offered(0),
            TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
            gen_rand_sha256_hash(),
            HashAlgorithm::CkbHash,
        ),
    );
    let funding_outpoint = state.get_funding_transaction_outpoint().unwrap();

    let settlement = SettlementData {
        local_amount: 100_000_000,
        remote_amount: 100_000_000,
        tlcs: vec![],
    };
    store.insert_watch_channel(
        NodeId::local(),
        channel_id,
        None,
        local_settlement_privkey.clone(),
        remote_settlement_pubkey,
        local_privkey.pubkey(),
        remote_privkey.pubkey(),
        settlement.clone(),
    );

    let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (myself, _) = Actor::spawn(None, MessageCollectorActor(sender), ())
        .await
        .expect("spawn collector");

    // 1. Malformed commitment lock (wrong code hash)
    {
        let malformed_lock = Script::new_builder()
            .code_hash(gen_rand_sha256_hash())
            .hash_type(ckb_types::core::ScriptHashType::Data2)
            .args([0u8; 60].pack())
            .build();
        assert!(
            parse_commitment_lock(
                &malformed_lock,
                &local_privkey.pubkey(),
                &remote_privkey.pubkey()
            )
            .is_none(),
            "malformed lock with wrong code hash must return None"
        );

        let tx = create_test_commitment_tx(funding_outpoint.clone(), malformed_lock, vec![]);
        let tx_hash: ckb_types::H256 = tx.hash().unpack();

        let chain_client = MockSettlementChainClient::new();
        chain_client.set_tx(tx_hash.clone(), tx);

        state.shutdown_transaction_hash = Some(tx_hash);
        check_channel_shutdown_settlement(
            chain_client,
            myself.clone(),
            state.clone(),
            store.clone(),
        )
        .await;
        assert!(
            store.get_shutdown_settlement_record(&channel_id).is_none(),
            "malformed script must not recover snapshot"
        );
    }

    // 2. Wrong funding input (tx does not spend channel's funding outpoint)
    {
        let valid_lock = create_test_commitment_lock_with_keys(
            &local_privkey,
            &remote_privkey,
            &local_settlement_privkey,
            remote_settlement_pubkey,
            &settlement,
            true,
            0,
        );
        let wrong_outpoint = OutPoint::new(gen_rand_sha256_hash().into(), 99);
        let tx_wrong_funding = create_test_commitment_tx(wrong_outpoint, valid_lock, vec![]);
        let tx_hash: ckb_types::H256 = tx_wrong_funding.hash().unpack();

        let chain_client = MockSettlementChainClient::new();
        chain_client.set_tx(tx_hash.clone(), tx_wrong_funding);

        state.shutdown_transaction_hash = Some(tx_hash);
        check_channel_shutdown_settlement(
            chain_client,
            myself.clone(),
            state.clone(),
            store.clone(),
        )
        .await;
        assert!(
            store.get_shutdown_settlement_record(&channel_id).is_none(),
            "tx with mismatched funding input must not recover snapshot"
        );
    }

    // 3. Direction mismatch: channel state is UNCOOPERATIVE_REMOTE, but lock is built for local close (for_remote=false)
    {
        let local_lock = create_test_commitment_lock_with_keys(
            &local_privkey,
            &remote_privkey,
            &local_settlement_privkey,
            remote_settlement_pubkey,
            &settlement,
            false, // for_remote = false
            0,
        );
        let tx_local_lock = create_test_commitment_tx(funding_outpoint.clone(), local_lock, vec![]);
        let tx_hash: ckb_types::H256 = tx_local_lock.hash().unpack();

        let chain_client = MockSettlementChainClient::new();
        chain_client.set_tx(tx_hash.clone(), tx_local_lock);

        state.shutdown_transaction_hash = Some(tx_hash);
        check_channel_shutdown_settlement(
            chain_client,
            myself.clone(),
            state.clone(),
            store.clone(),
        )
        .await;
        assert!(
            store.get_shutdown_settlement_record(&channel_id).is_none(),
            "direction mismatch must not recover snapshot"
        );
    }

    // 4. Missing watchtower data: non-existent channel_id
    {
        let unknown_channel_id = gen_rand_sha256_hash();
        let mut unknown_state = state.clone();
        unknown_state.id = unknown_channel_id;

        let valid_lock = create_test_commitment_lock_with_keys(
            &local_privkey,
            &remote_privkey,
            &local_settlement_privkey,
            remote_settlement_pubkey,
            &settlement,
            true,
            0,
        );
        let tx = create_test_commitment_tx(funding_outpoint, valid_lock, vec![]);
        let tx_hash: ckb_types::H256 = tx.hash().unpack();

        let chain_client = MockSettlementChainClient::new();
        chain_client.set_tx(tx_hash.clone(), tx);

        unknown_state.shutdown_transaction_hash = Some(tx_hash);
        check_channel_shutdown_settlement(
            chain_client,
            myself.clone(),
            unknown_state.clone(),
            store.clone(),
        )
        .await;
        assert!(
            store
                .get_shutdown_settlement_record(&unknown_channel_id)
                .is_none(),
            "missing watchtower data must not recover snapshot"
        );
    }

    assert!(rx.try_recv().is_err(), "no messages should have been sent");
}

#[test]
fn review_missing_snapshot_must_remain_unknown() {
    let snapshot_store = MockStore::new();
    let mut state = empty_channel_state(gen_rand_sha256_hash());
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_REMOTE
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
    );
    state.shutdown_transaction_hash = Some(gen_rand_sha256_hash().into());
    assert!(
        has_unresolved_onchain_tlcs(&state, &snapshot_store),
        "missing verified snapshot must not allow finalization"
    );
}

#[test]
fn review_wrong_direction_record_must_not_resolve_active_tlc() {
    let snapshot_store = MockStore::new();
    let mut state = closed_state_with_offered_local_announced(
        gen_rand_sha256_hash(),
        CloseFlags::UNCOOPERATIVE_REMOTE
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        tlc_info(
            TLCId::Offered(0),
            TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
            gen_rand_sha256_hash(),
            HashAlgorithm::CkbHash,
        ),
    );
    let tx_hash = gen_rand_sha256_hash().into();
    state.shutdown_transaction_hash = Some(tx_hash);
    snapshot_store.store_shutdown_settlement_record(
        &state.get_id(),
        &ShutdownSettlementRecord {
            shutdown_tx_hash: state.shutdown_transaction_hash.clone().unwrap(),
            for_remote: false,
            commitment_number: 1,
            settlement_data: SettlementData {
                local_amount: 0,
                remote_amount: 0,
                tlcs: vec![],
            },
        },
    );
    assert!(
        has_unresolved_onchain_tlcs(&state, &snapshot_store),
        "a local-direction record cannot resolve a remote close"
    );

    // Reconciliation must see updates to the persisted record without refreshing state.
    let mut record = snapshot_store
        .get_shutdown_settlement_record(&state.get_id())
        .unwrap();
    record.for_remote = true;
    snapshot_store.store_shutdown_settlement_record(&state.get_id(), &record);
    assert!(!has_unresolved_onchain_tlcs(&state, &snapshot_store));

    record.shutdown_tx_hash = gen_rand_sha256_hash().into();
    snapshot_store.store_shutdown_settlement_record(&state.get_id(), &record);
    assert!(has_unresolved_onchain_tlcs(&state, &snapshot_store));

    snapshot_store.delete_shutdown_settlement_record(&state.get_id());
    assert!(has_unresolved_onchain_tlcs(&state, &snapshot_store));
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_revoked_remote_close_recovers_and_clears_waiting() {
    assert_confirmed_snapshot_recovery(false, true).await;
}

#[test]
fn test_fresh_channel_pre_tlc_commitment_uses_verified_snapshot() {
    let local_privkey = Privkey::from([1u8; 32]);
    let remote_privkey = Privkey::from([2u8; 32]);
    let local_settlement_key = Privkey::from([3u8; 32]);
    let remote_settlement_key = Privkey::from([4u8; 32]).pubkey();
    let channel_id = gen_rand_sha256_hash();

    let preceding_settlement = SettlementData {
        local_amount: 1000,
        remote_amount: 2000,
        tlcs: vec![],
    };
    let pending_settlement = SettlementData {
        local_amount: 500,
        remote_amount: 2500,
        tlcs: vec![],
    };
    let local_settlement = SettlementData {
        local_amount: 1500,
        remote_amount: 1500,
        tlcs: vec![],
    };

    let channel_data = ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: local_settlement_key.clone(),
        remote_settlement_key,
        local_funding_pubkey: local_privkey.pubkey(),
        remote_funding_pubkey: remote_privkey.pubkey(),
        remote_settlement_data: preceding_settlement.clone(),
        pending_remote_settlement_data: pending_settlement.clone(),
        local_settlement_data: local_settlement.clone(),
        revocation_data: None,
    };

    for (for_remote, number, expected) in [
        (true, 1, &preceding_settlement),
        (true, 2, &pending_settlement),
        (false, 1, &local_settlement),
    ] {
        let lock = create_test_commitment_lock_with_keys(
            &local_privkey,
            &remote_privkey,
            &local_settlement_key,
            remote_settlement_key,
            expected,
            for_remote,
            number,
        );
        let (direction, actual_number, selected) =
            verify_and_select_settlement_data(&channel_data, &lock)
                .expect("fresh channel must recover the hash-matching snapshot without RAA");
        assert_eq!((direction, actual_number), (for_remote, number));
        assert_eq!(selected, expected);
        assert!(tracked_settlement_tlcs(&lock, &channel_data, for_remote).is_some());
        assert!(tracked_settlement_tlcs(&lock, &channel_data, !for_remote).is_none());
        let mut args = lock.args().raw_data().to_vec();
        args[40] ^= 0xff;
        let invalid = lock.as_builder().args(args.pack()).build();
        assert!(verify_and_select_settlement_data(&channel_data, &invalid).is_none());
    }
}

async fn assert_excluded_payer_reconciliation(
    close_flag: CloseFlags,
    live_actor: bool,
    partial_failure: bool,
    reused_attempt: bool,
    other_inflight: bool,
    block_payment: bool,
) {
    use crate::fiber::graph::NetworkGraphStateStore;
    use crate::fiber::payment::SendPaymentDataBuilder;
    use fiber_types::{AttemptStatus, PaymentHopData, PaymentSession, PaymentStatus};

    let mut node = NetworkNode::new().await;
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let mut state = empty_channel_state(channel_id);
    state.state = ChannelState::Closed(
        close_flag
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
    );
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        HashAlgorithm::CkbHash,
    );
    tlc.attempt_id = Some(1);
    // Exclusion is conclusive even before the TLC's expiry.
    tlc.expiry = u64::MAX;
    state.tlc_state.add_offered_tlc(tlc);
    state.funding_tx = Some(
        TransactionBuilder::default()
            .output(
                CellOutput::new_builder()
                    .capacity(100_000_000_000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data(),
    );
    let source = state.get_local_pubkey();
    let target = gen_rand_fiber_public_key();
    let mut session = PaymentSession {
        request: SendPaymentDataBuilder::new(target, 1000, payment_hash)
            .max_fee_amount(Some(100))
            .build()
            .unwrap(),
        status: PaymentStatus::Inflight,
        last_error: None,
        last_error_code: None,
        try_limit: 3,
        created_at: 0,
        last_updated_at: 0,
        cached_attempts: vec![],
    };
    let mut attempt = session.new_attempt(
        1,
        source,
        target,
        vec![
            PaymentHopData {
                amount: 1000,
                next_hop: Some(target),
                funding_tx_hash: state
                    .get_funding_transaction_outpoint()
                    .unwrap()
                    .tx_hash()
                    .into(),
                ..Default::default()
            },
            PaymentHopData {
                amount: 1000,
                ..Default::default()
            },
        ],
    );
    attempt.set_inflight_status();
    attempt.try_limit = 1;
    attempt.tried_times = 2;
    session.append_attempt(attempt.clone());
    if other_inflight {
        session.request.amount = 2000;
        let mut other = attempt.clone();
        other.id = 2;
        other.route.nodes[0].channel_outpoint =
            ckb_types::packed::OutPoint::new(gen_rand_sha256_hash().into(), 0);
        session.append_attempt(other.clone());
        node.store.insert_attempt(other);
    }
    node.store.insert_payment_session(session);
    if partial_failure {
        // Seed inconsistent persisted state: the attempt is failed, but the session is unfinished.
        attempt.set_failed_status("TLC excluded from confirmed remote commitment", false);
    }
    if reused_attempt {
        attempt.route.nodes[0].channel_outpoint =
            ckb_types::packed::OutPoint::new(gen_rand_sha256_hash().into(), 0);
    }
    node.store.insert_attempt(attempt);
    state.shutdown_transaction_hash = Some(gen_rand_sha256_hash().into());
    node.store.store_shutdown_settlement_record(
        &channel_id,
        &ShutdownSettlementRecord {
            shutdown_tx_hash: state.shutdown_transaction_hash.clone().unwrap(),
            for_remote: close_flag == CloseFlags::UNCOOPERATIVE_REMOTE,
            commitment_number: 1,
            settlement_data: SettlementData {
                local_amount: 1000,
                remote_amount: 1000,
                tlcs: vec![],
            },
        },
    );
    node.store.insert_channel_actor_state(state);
    let mut live_ref = None;
    if live_actor {
        let key = Privkey::from([11; 32]);
        let (actor, _handle) = Actor::spawn(
            None,
            ChannelActor::new(
                key.pubkey(),
                target,
                node.network_actor.clone(),
                node.store.clone(),
                None,
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::RestoreOfflineChannel(channel_id),
                ephemeral_config: Default::default(),
                private_key: key,
            },
        )
        .await
        .unwrap();
        live_ref = Some(actor.clone());
        ractor::call!(
            node.network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::InstallTestChannelActor(
                channel_id, actor, reply
            ))
        )
        .unwrap();
    }
    let graph_guard = if block_payment {
        Some(node.network_graph.write().await)
    } else {
        None
    };
    node.network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channel_id),
        ))
        .unwrap();
    if block_payment {
        if let Some(actor) = &live_ref {
            ractor::call_t!(
                actor,
                |reply| ChannelActorMessage::Command(ChannelCommand::TestBarrier(reply)),
                500
            )
            .expect("channel actor must not wait for payment persistence");
        }
        tokio::time::timeout(std::time::Duration::from_millis(500), node.node_info())
            .await
            .expect("network actor must not wait for payment persistence");
        assert!(node
            .store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .is_waiting_onchain_settlement());
        // Duplicate maintenance before the payment reply must remain harmless.
        for _ in 0..3 {
            node.network_actor
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::ChannelSettlementCompleted(channel_id),
                ))
                .unwrap();
        }
    }
    drop(graph_guard);
    wait_for_settlement_completion(&node, channel_id).await;
    let finished = node.store.get_channel_actor_state(&channel_id).unwrap();
    let tlc = finished.tlc_state.get(&TLCId::Offered(0)).unwrap();
    assert!(
        matches!(tlc.removed_reason, Some(RemoveTlcReason::RemoveTlcFail(_))),
        "excluded TLC must fail before channel finalizes (live={live_actor})"
    );
    assert_eq!(tlc.outbound_status(), OutboundTlcStatus::RemoteRemoved);
    assert_eq!(
        node.store.get_attempt(payment_hash, 1).unwrap().status,
        if reused_attempt {
            AttemptStatus::Inflight
        } else {
            AttemptStatus::Failed
        }
    );
    assert_eq!(
        node.store.get_payment_session(payment_hash).unwrap().status,
        if reused_attempt || other_inflight {
            PaymentStatus::Inflight
        } else {
            PaymentStatus::Failed
        }
    );
    assert_eq!(
        node.store.get_persisted_payment_status(payment_hash),
        Some(if reused_attempt || other_inflight {
            PaymentStatus::Inflight
        } else {
            PaymentStatus::Failed
        })
    );
    node.stop().await;
}

#[tokio::test]
async fn test_excluded_local_announced_fails_payer_before_finalization() {
    for live in [false, true] {
        assert_excluded_payer_reconciliation(
            CloseFlags::UNCOOPERATIVE_REMOTE,
            live,
            false,
            false,
            false,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn test_excluded_local_announced_repairs_partial_payment_write() {
    assert_excluded_payer_reconciliation(
        CloseFlags::UNCOOPERATIVE_REMOTE,
        false,
        true,
        false,
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_excluded_local_announced_does_not_fail_reused_attempt() {
    assert_excluded_payer_reconciliation(
        CloseFlags::UNCOOPERATIVE_REMOTE,
        false,
        false,
        true,
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_excluded_local_announced_preserves_other_inflight_shards() {
    assert_excluded_payer_reconciliation(
        CloseFlags::UNCOOPERATIVE_REMOTE,
        false,
        false,
        false,
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_excluded_local_announced_relays_failure_before_finalization() {
    assert_excluded_tlc_relays_failure_before_finalization(CloseFlags::UNCOOPERATIVE_REMOTE, true)
        .await;
}

#[tokio::test]
async fn test_local_force_close_excluded_tlc_relays_failure_before_finalization() {
    assert_excluded_tlc_relays_failure_before_finalization(CloseFlags::UNCOOPERATIVE_LOCAL, true)
        .await;
}

async fn assert_excluded_tlc_relays_failure_before_finalization(
    close_flag: CloseFlags,
    settlement_completed: bool,
) {
    let mut node = NetworkNode::new().await;
    let channel_id = gen_rand_sha256_hash();
    let upstream_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let mut state = empty_channel_state(channel_id);
    let waiting_flags = close_flag | CloseFlags::WAITING_ONCHAIN_SETTLEMENT;
    state.state = ChannelState::Closed(if settlement_completed {
        waiting_flags | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED
    } else {
        waiting_flags
    });
    state.shutdown_transaction_hash = Some(gen_rand_sha256_hash().into());
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        HashAlgorithm::CkbHash,
    );
    tlc.forwarding_tlc = Some((upstream_id, 0));
    let expected_reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
        TlcErr::new(TlcErrorCode::PermanentChannelFailure),
        &tlc.shared_secret,
    ));
    state.tlc_state.add_offered_tlc(tlc);
    node.store.insert_channel_actor_state(state.clone());
    node.store.store_shutdown_settlement_record(
        &channel_id,
        &ShutdownSettlementRecord {
            shutdown_tx_hash: state.shutdown_transaction_hash.clone().unwrap(),
            for_remote: close_flag == CloseFlags::UNCOOPERATIVE_REMOTE,
            commitment_number: 1,
            settlement_data: SettlementData {
                local_amount: 1000,
                remote_amount: 1000,
                tlcs: vec![],
            },
        },
    );
    node.network_actor
        .send_message(if settlement_completed {
            NetworkActorMessage::new_event(NetworkActorEvent::ChannelSettlementCompleted(
                channel_id,
            ))
        } else {
            NetworkActorMessage::new_command(NetworkActorCommand::CheckChannels)
        })
        .unwrap();
    node.node_info().await;
    assert!(
        node.store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .is_waiting_onchain_settlement(),
        "missing upstream must keep downstream retryable"
    );
    let mut upstream = empty_channel_state(upstream_id);
    upstream.state = ChannelState::ChannelReady;
    upstream.tlc_state.add_received_tlc(tlc_info(
        TLCId::Received(0),
        TlcStatus::Inbound(InboundTlcStatus::Committed),
        payment_hash,
        HashAlgorithm::CkbHash,
    ));
    node.store.insert_channel_actor_state(upstream);
    // The offline upstream must durably queue the failure before acknowledging it.
    for _ in 0..3 {
        node.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::CheckChannels,
            ))
            .unwrap();
        node.node_info().await;
    }
    if settlement_completed {
        wait_for_settlement_completion(&node, channel_id).await;
    } else {
        let downstream = node.store.get_channel_actor_state(&channel_id).unwrap();
        assert_eq!(downstream.state, ChannelState::Closed(waiting_flags));
        assert!(node
            .store
            .get_shutdown_settlement_record(&channel_id)
            .is_some());
    }
    let upstream = node.store.get_channel_actor_state(&upstream_id).unwrap();
    assert_eq!(
        upstream.retryable_tlc_operations,
        std::collections::VecDeque::from([RetryableTlcOperation::RemoveTlc(
            TLCId::Received(0),
            expected_reason.clone(),
        )]),
        "the upstream must persist exactly one failure for the forwarded TLC"
    );
    let finished = node.store.get_channel_actor_state(&channel_id).unwrap();
    assert_eq!(
        finished
            .tlc_state
            .get(&TLCId::Offered(0))
            .unwrap()
            .removed_reason,
        Some(expected_reason)
    );
    node.stop().await;
}

#[test]
fn test_excluded_local_announced_requires_confirmed_close_and_real_snapshot() {
    use crate::fiber::onchain_tlc_reconcile::collect_onchain_excluded_tlcs;
    let store = MockStore::new();
    let mut state = empty_channel_state(gen_rand_sha256_hash());
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    state.tlc_state.add_offered_tlc(tlc.clone());
    let waiting = CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT;
    state.state = ChannelState::Closed(waiting);
    let snapshot = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![],
    };
    install_unit_snapshot(&mut state, &store, true, snapshot);
    assert_eq!(collect_onchain_excluded_tlcs(&state, &store).len(), 1);
    // A cached snapshot alone is not enough before the force-close confirmation,
    // and a finalized channel must not start new exclusion effects.
    for channel_state in [
        ChannelState::ChannelReady,
        ChannelState::ShuttingDown(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION),
        ChannelState::Closed(CloseFlags::UNCOOPERATIVE_REMOTE),
        ChannelState::Closed(CloseFlags::COOPERATIVE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT),
        ChannelState::Closed(waiting | CloseFlags::UNCOOPERATIVE_LOCAL),
    ] {
        state.state = channel_state;
        assert!(collect_onchain_excluded_tlcs(&state, &store).is_empty());
    }
    state.state = ChannelState::Closed(waiting);
    let record = store
        .get_shutdown_settlement_record(&state.get_id())
        .unwrap();
    for invalid in [
        ShutdownSettlementRecord {
            shutdown_tx_hash: gen_rand_sha256_hash().into(),
            ..record.clone()
        },
        ShutdownSettlementRecord {
            for_remote: false,
            ..record.clone()
        },
        ShutdownSettlementRecord {
            settlement_data: SettlementData {
                local_amount: 0,
                remote_amount: 0,
                tlcs: vec![],
            },
            ..record.clone()
        },
        ShutdownSettlementRecord {
            settlement_data: SettlementData {
                tlcs: vec![settlement_tlc_for(&tlc)],
                ..record.settlement_data.clone()
            },
            ..record.clone()
        },
    ] {
        store.store_shutdown_settlement_record(&state.get_id(), &invalid);
        assert!(collect_onchain_excluded_tlcs(&state, &store).is_empty());
    }
    store.delete_shutdown_settlement_record(&state.get_id());
    assert!(collect_onchain_excluded_tlcs(&state, &store).is_empty());
    store.store_shutdown_settlement_record(&state.get_id(), &record);
    state.tlc_state.get_mut(&TLCId::Offered(0)).unwrap().status =
        TlcStatus::Outbound(OutboundTlcStatus::Committed);
    assert!(collect_onchain_excluded_tlcs(&state, &store).is_empty());
}

#[tokio::test]
async fn test_excluded_payment_does_not_block_channel_or_network_actor() {
    for live in [false, true] {
        assert_excluded_payer_reconciliation(
            CloseFlags::UNCOOPERATIVE_REMOTE,
            live,
            false,
            false,
            false,
            true,
        )
        .await;
    }
}

#[tokio::test]
async fn test_local_force_close_excluded_tlc_fails_payer_offline() {
    assert_excluded_payer_reconciliation(
        CloseFlags::UNCOOPERATIVE_LOCAL,
        false,
        false,
        false,
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_local_force_close_excluded_tlc_fails_payer_live() {
    assert_excluded_payer_reconciliation(
        CloseFlags::UNCOOPERATIVE_LOCAL,
        true,
        false,
        false,
        false,
        false,
    )
    .await;
}

#[test]
fn test_local_force_close_excluded_tlc_respects_snapshot_direction() {
    use crate::fiber::onchain_tlc_reconcile::collect_onchain_excluded_tlcs;
    let store = MockStore::new();
    let mut state = empty_channel_state(gen_rand_sha256_hash());
    state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
    );
    let tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    );
    state.tlc_state.add_offered_tlc(tlc.clone());
    // In a local snapshot, Offered(0) represents the opposite direction and must
    // not hide our excluded outgoing TLC with the same numeric id.
    let mut included = settlement_tlc_for(&tlc);
    install_unit_snapshot(
        &mut state,
        &store,
        false,
        SettlementData {
            local_amount: 1000,
            remote_amount: 1000,
            tlcs: vec![included.clone()],
        },
    );
    assert_eq!(collect_onchain_excluded_tlcs(&state, &store).len(), 1);
    included.tlc_id = included.tlc_id.flip();
    install_unit_snapshot(
        &mut state,
        &store,
        false,
        SettlementData {
            local_amount: 1000,
            remote_amount: 1000,
            tlcs: vec![included],
        },
    );
    assert!(
        collect_onchain_excluded_tlcs(&state, &store).is_empty(),
        "an included outgoing TLC must follow normal on-chain resolution"
    );
}

#[tokio::test]
async fn test_local_force_close_excluded_tlc_waits_for_payment_persistence() {
    for live in [false, true] {
        assert_excluded_payer_reconciliation(
            CloseFlags::UNCOOPERATIVE_LOCAL,
            live,
            false,
            false,
            false,
            true,
        )
        .await;
    }
}

#[test]
fn test_local_force_close_excluded_tlc_ignores_remote_revocation_number() {
    use crate::fiber::onchain_tlc_reconcile::collect_onchain_excluded_tlcs;
    let store = MockStore::new();
    let channel_id = gen_rand_sha256_hash();
    let mut state = empty_channel_state(channel_id);
    let snapshot = SettlementData {
        local_amount: 1000,
        remote_amount: 1000,
        tlcs: vec![],
    };
    state.tlc_state.add_offered_tlc(tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        gen_rand_sha256_hash(),
        HashAlgorithm::CkbHash,
    ));
    store.watch_channels.borrow_mut().insert(
        channel_id,
        ChannelData {
            channel_id,
            funding_udt_type_script: None,
            local_settlement_key: Privkey::from([1u8; 32]),
            remote_settlement_key: Privkey::from([2u8; 32]).pubkey(),
            local_funding_pubkey: Privkey::from([3u8; 32]).pubkey(),
            remote_funding_pubkey: Privkey::from([4u8; 32]).pubkey(),
            remote_settlement_data: snapshot.clone(),
            pending_remote_settlement_data: snapshot.clone(),
            local_settlement_data: snapshot.clone(),
            revocation_data: Some(RevocationData {
                commitment_number: 1,
                aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
                output: CellOutput::default(),
                output_data: Default::default(),
            }),
        },
    );
    for for_remote in [false, true] {
        let close_flag = if for_remote {
            CloseFlags::UNCOOPERATIVE_REMOTE
        } else {
            CloseFlags::UNCOOPERATIVE_LOCAL
        };
        state.state = ChannelState::Closed(
            close_flag
                | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
                | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
        );
        install_unit_snapshot(&mut state, &store, for_remote, snapshot.clone());
        assert_eq!(
            collect_onchain_excluded_tlcs(&state, &store).len(),
            usize::from(!for_remote)
        );
    }
}

#[cfg(feature = "watchtower")]
async fn assert_excluded_payer_with_unclaimed_balance(
    for_remote: bool,
    live_actor: bool,
    restart_before_delivery: bool,
) {
    use crate::fiber::graph::NetworkGraphStateStore;
    use crate::fiber::payment::SendPaymentDataBuilder;
    use fiber_types::{AttemptStatus, PaymentHopData, PaymentSession, PaymentStatus};

    let mut node = NetworkNode::new().await;
    let channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();
    let close_flag = if for_remote {
        CloseFlags::UNCOOPERATIVE_REMOTE
    } else {
        CloseFlags::UNCOOPERATIVE_LOCAL
    };
    let mut tlc = tlc_info(
        TLCId::Offered(0),
        TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        payment_hash,
        HashAlgorithm::CkbHash,
    );
    tlc.attempt_id = Some(1);
    tlc.expiry = u64::MAX;
    let mut state = closed_state_with_offered_local_announced(
        channel_id,
        close_flag | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        tlc,
    );
    let source = state.get_local_pubkey();
    let target = gen_rand_fiber_public_key();
    let funding_outpoint = state.get_funding_transaction_outpoint().unwrap();
    let mut session = PaymentSession {
        request: SendPaymentDataBuilder::new(target, 1000, payment_hash)
            .max_fee_amount(Some(100))
            .build()
            .unwrap(),
        status: PaymentStatus::Inflight,
        last_error: None,
        last_error_code: None,
        try_limit: 3,
        created_at: 0,
        last_updated_at: 0,
        cached_attempts: vec![],
    };
    let mut attempt = session.new_attempt(
        1,
        source,
        target,
        vec![
            PaymentHopData {
                amount: 1000,
                next_hop: Some(target),
                funding_tx_hash: funding_outpoint.tx_hash().into(),
                ..Default::default()
            },
            PaymentHopData {
                amount: 1000,
                ..Default::default()
            },
        ],
    );
    attempt.set_inflight_status();
    // Exhaust retries so the expected session result is deterministic.
    attempt.try_limit = 1;
    attempt.tried_times = 2;
    session.append_attempt(attempt.clone());
    node.store.insert_payment_session(session);
    node.store.insert_attempt(attempt);

    let local_key = Privkey::from([11; 32]);
    let remote_key = Privkey::from([22; 32]);
    let settlement_key = Privkey::from([33; 32]);
    let remote_settlement_key = Privkey::from([44; 32]).pubkey();
    let snapshot = SettlementData {
        local_amount: 100_000_000,
        remote_amount: 9_900_000_000,
        tlcs: vec![],
    };
    node.store.insert_watch_channel(
        NodeId::local(),
        channel_id,
        None,
        settlement_key.clone(),
        remote_settlement_key,
        local_key.pubkey(),
        remote_key.pubkey(),
        snapshot.clone(),
    );
    let lock = create_test_commitment_lock_with_keys(
        &local_key,
        &remote_key,
        &settlement_key,
        remote_settlement_key,
        &snapshot,
        for_remote,
        1,
    );
    let commitment_tx = create_test_commitment_tx(funding_outpoint, lock.clone(), vec![]);
    state.shutdown_transaction_hash = Some(commitment_tx.hash().unpack());
    node.store.insert_channel_actor_state(state);
    {
        let mut chain = node.chain_client.state.write().unwrap();
        chain.txs.insert(
            commitment_tx.hash().into(),
            GetTxResponse {
                transaction: Some(commitment_tx),
                tx_status: ckb_types::core::tx_pool::TxStatus::Committed(
                    1,
                    gen_rand_sha256_hash().into(),
                    0,
                ),
            },
        );
        // Our 0xff withdrawal has completed; the peer's continuation output is still live.
        // It shares the commitment prefix used by the production settlement checker.
        let mut args = lock.args().raw_data().to_vec();
        *args.last_mut().unwrap() = 0xff;
        chain.indexer_cells.push(ckb_sdk::rpc::ckb_indexer::Cell {
            output: CellOutput::new_builder()
                .capacity(9_900_000_000u64)
                .lock(lock.as_builder().args(args.pack()).build())
                .build()
                .into(),
            output_data: None,
            out_point: OutPoint::new(gen_rand_sha256_hash().into(), 0).into(),
            block_number: 2u64.into(),
            tx_index: 0u32.into(),
        });
    }
    if restart_before_delivery {
        // Model a crash after snapshot persistence but before recovered-event delivery.
        // Recover through the production checker while the node is stopped, then
        // discard the event so restart maintenance must use the persisted evidence.
        node.stop().await;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let (collector, _) = Actor::spawn(None, MessageCollectorActor(sender), ())
            .await
            .unwrap();
        check_channel_shutdown_settlement(
            node.chain_client.clone(),
            collector.clone(),
            node.store.get_channel_actor_state(&channel_id).unwrap(),
            node.store.clone(),
        )
        .await;
        assert!(matches!(
            receiver.recv().await.unwrap(),
            NetworkActorMessage::Event(NetworkActorEvent::ChannelSettlementRecovered(id, _))
                if id == channel_id
        ));
        collector.stop(None);
        assert_eq!(
            node.store.get_attempt(payment_hash, 1).unwrap().status,
            AttemptStatus::Inflight
        );
        assert!(node
            .store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .tlc_state
            .get(&TLCId::Offered(0))
            .unwrap()
            .removed_reason
            .is_none());
        let chain_state = node.chain_client.state.read().unwrap().clone();
        node.start().await;
        *node.chain_client.state.write().unwrap() = chain_state;
    }
    let mut live_ref = None;
    if live_actor {
        let (actor, _) = Actor::spawn(
            None,
            ChannelActor::new(
                local_key.pubkey(),
                target,
                node.network_actor.clone(),
                node.store.clone(),
                None,
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::RestoreOfflineChannel(channel_id),
                ephemeral_config: Default::default(),
                private_key: local_key,
            },
        )
        .await
        .unwrap();
        live_ref = Some(actor.clone());
        ractor::call!(
            node.network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::InstallTestChannelActor(
                channel_id, actor, reply
            ))
        )
        .unwrap();
    }

    // Normal checking recovers a fresh snapshot, or uses the one persisted before restart.
    // Neither path installs a settlement-completed flag or sends a completion event.
    trigger_shutdown_check(&node);
    node.network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannels,
        ))
        .unwrap();
    if let Some(actor) = &live_ref {
        actor
            .send_message(ChannelActorMessage::Event(
                ChannelEvent::MaintainChannelTlcs,
            ))
            .unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let channel = node.store.get_channel_actor_state(&channel_id).unwrap();
            if channel
                .tlc_state
                .get(&TLCId::Offered(0))
                .unwrap()
                .removed_reason
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("excluded payer TLC must fail while the peer balance remains live");
    let channel = node.store.get_channel_actor_state(&channel_id).unwrap();
    assert_eq!(
        channel.state,
        ChannelState::Closed(close_flag | CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
    );
    assert_eq!(
        channel
            .tlc_state
            .get(&TLCId::Offered(0))
            .unwrap()
            .outbound_status(),
        OutboundTlcStatus::RemoteRemoved
    );
    assert_eq!(
        node.store.get_attempt(payment_hash, 1).unwrap().status,
        AttemptStatus::Failed
    );
    assert_eq!(
        node.store.get_payment_session(payment_hash).unwrap().status,
        PaymentStatus::Failed
    );
    assert_eq!(
        node.store.get_persisted_payment_status(payment_hash),
        Some(PaymentStatus::Failed)
    );
    assert_eq!(
        node.store
            .get_shutdown_settlement_record(&channel_id)
            .unwrap()
            .settlement_data,
        snapshot
    );
    assert_eq!(
        node.chain_client.state.read().unwrap().indexer_cells.len(),
        1
    );

    // Repeated maintenance must preserve the outcome and leave the channel waiting.
    for _ in 0..3 {
        trigger_shutdown_check(&node);
        node.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::CheckChannels,
            ))
            .unwrap();
        if let Some(actor) = &live_ref {
            actor
                .send_message(ChannelActorMessage::Event(
                    ChannelEvent::MaintainChannelTlcs,
                ))
                .unwrap();
            ractor::call!(actor, |reply| ChannelActorMessage::Command(
                ChannelCommand::TestBarrier(reply)
            ))
            .unwrap();
        }
        node.node_info().await;
    }
    assert_eq!(
        node.store
            .get_channel_actor_state(&channel_id)
            .unwrap()
            .state,
        channel.state
    );
    assert_eq!(
        node.store.get_attempt(payment_hash, 1).unwrap().status,
        AttemptStatus::Failed
    );

    // Only spending the last balance cell may finalize the channel.
    node.chain_client
        .state
        .write()
        .unwrap()
        .indexer_cells
        .clear();
    trigger_shutdown_check(&node);
    wait_for_settlement_completion(&node, channel_id).await;
    node.stop().await;
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_excluded_payer_with_unclaimed_balance_offline() {
    for for_remote in [false, true] {
        assert_excluded_payer_with_unclaimed_balance(for_remote, false, false).await;
    }
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_excluded_payer_with_unclaimed_balance_live() {
    for for_remote in [false, true] {
        assert_excluded_payer_with_unclaimed_balance(for_remote, true, false).await;
    }
}

#[tokio::test]
#[cfg(feature = "watchtower")]
async fn test_excluded_payer_with_unclaimed_balance_recovers_after_restart() {
    for for_remote in [false, true] {
        for live_actor in [false, true] {
            assert_excluded_payer_with_unclaimed_balance(for_remote, live_actor, true).await;
        }
    }
}

#[tokio::test]
async fn test_excluded_forwarded_tlc_relays_before_balance_settlement() {
    for close_flag in [
        CloseFlags::UNCOOPERATIVE_LOCAL,
        CloseFlags::UNCOOPERATIVE_REMOTE,
    ] {
        assert_excluded_tlc_relays_failure_before_finalization(close_flag, false).await;
    }
}
