use super::*;

use fiber_types::{HashAlgorithm, TlcState};

struct PaymentNoticeCounter;

#[async_trait::async_trait]
impl Actor for PaymentNoticeCounter {
    type Msg = FiberActorMessage;
    type State = u32;
    type Arguments = ();
    async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<u32, ActorProcessingErr> {
        Ok(0)
    }
    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        count: &mut u32,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            FiberActorMessage::Command(FiberActorCommand::PaymentTlcUpdated(_)) => *count += 1,
            FiberActorMessage::Command(FiberActorCommand::GetInflightPaymentCount(reply)) => {
                let _ = reply.send(Ok(*count));
            }
            FiberActorMessage::Command(FiberActorCommand::CompleteClosedLocalAdd {
                reply, ..
            }) => {
                let _ = reply.send(Ok(()));
            }
            _ => {}
        }
        Ok(())
    }
}

struct NoticeChannelMailbox;

#[async_trait::async_trait]
impl Actor for NoticeChannelMailbox {
    type Msg = ChannelActorMessage;
    type State = ();
    type Arguments = ();
    async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        _: Self::Msg,
        _: &mut (),
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
}

#[tokio::test]
async fn test_review_queued_add_owner_release_notifies_once() {
    for closed in [false, true] {
        let (store, _dir) = crate::tests::test_utils::generate_store();
        let (network, task) = Actor::spawn(None, PaymentNoticeCounter, ()).await.unwrap();
        let (myself, channel_task) = Actor::spawn(None, NoticeChannelMailbox, ()).await.unwrap();
        let mut state =
            channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
        state.funding_tx = Some(ckb_types::packed::Transaction::default());
        if closed {
            state.state = ChannelState::Closed(
                CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
            );
        }
        let command = AddTlcCommand {
            upstream_tlc_key: None,
            amount: 100,
            payment_hash: [7; 32].into(),
            attempt_id: Some(1),
            expiry: 0,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
        };
        state
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::AddTlc(command.clone()));
        store.insert_channel_actor_state_for_test(state.clone());
        let actor = ChannelActor::new(
            state.local_pubkey,
            state.remote_pubkey,
            network.clone(),
            store,
            None,
        );
        if closed {
            actor
                .finalize_closed_retryable_tlc_operations(&myself, &mut state)
                .await;
        } else {
            state.retryable_tlc_operations.pop_front();
            actor.post_add_tlc_command(
                &myself,
                &mut state,
                command,
                &Err(ProcessingChannelError::InternalError(
                    "queued Add rejected".to_owned(),
                )),
            );
        }
        assert!(state.retryable_tlc_operations.is_empty());
        actor.persist_state_and_publish_pending_effects(&mut state);
        actor.persist_state_and_publish_pending_effects(&mut state);
        let count = ractor::call_t!(
            network,
            |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(
                reply
            )),
            5000
        )
        .unwrap()
        .unwrap();
        myself.stop(None);
        channel_task.await.unwrap();
        network.stop(None);
        task.await.unwrap();
        assert_eq!(
            count, 1,
            "releasing the last queued Add owner must notify once (closed={closed})"
        );
    }
}

#[tokio::test]
async fn test_review_unchanged_tlc_does_not_republish_payment() {
    let (store, _dir) = crate::tests::test_utils::generate_store();
    let (network, task) = Actor::spawn(None, PaymentNoticeCounter, ()).await.unwrap();
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    let mut tlc = failed_remove_ack_confirmed_tlc(TLCId::Offered(0), 100);
    tlc.attempt_id = Some(1);
    let hash = tlc.payment_hash;
    state.tlc_state.add_offered_tlc(tlc);
    // Synthetic established channel: seed the fixed-field record it would have.
    store.insert_channel_actor_state_for_test(state.clone());
    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store,
        None,
    );
    state.pending_payment_notifications.insert(hash.into());
    for _ in 0..3 {
        actor.persist_state_and_publish_pending_effects(&mut state);
    }
    let count = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    network.stop(None);
    task.await.unwrap();
    assert_eq!(
        count, 1,
        "unrelated channel messages must not republish a completed TLC"
    );
}

#[test]
fn reestablish_timer_excludes_offline_time_and_preserves_remaining_budget() {
    let start = 1_000_000;
    let online = REESTABLISH_TIMEOUT * 3 / 5;
    let running = ReestablishTimer::RunningSince(start);
    let paused = running.pause(start + online);
    assert_eq!(paused, ReestablishTimer::PausedAfter(online));
    let reconnect = start + online + REESTABLISH_TIMEOUT * 100;
    assert_eq!(paused.elapsed(reconnect), online);
    assert!(!paused.expired(reconnect));
    assert_eq!(paused.pause(reconnect), paused);
    let resumed = paused.resume(reconnect);
    assert_eq!(resumed.elapsed(reconnect), online);
    assert_eq!(
        resumed.remaining_millis(reconnect),
        REESTABLISH_TIMEOUT - online + 1
    );
    // Duplicate reconnect/handshake events must not grant another window.
    assert_eq!(resumed.resume(reconnect + online), resumed);
    assert!(!resumed.expired(reconnect + REESTABLISH_TIMEOUT - online));
    assert!(resumed.expired(reconnect + REESTABLISH_TIMEOUT - online + 1));
    let expired_pause = resumed.pause(reconnect + online);
    assert!(!expired_pause.expired(reconnect + online));
    let resumed_expired = expired_pause.resume(reconnect + online * 10);
    assert_eq!(resumed_expired.remaining_millis(reconnect + online * 10), 1);
    assert!(resumed_expired.expired(reconnect + online * 10));
}

#[test]
fn test_musig2_agg_context_is_cached_per_key_order() {
    let state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    let local = state.get_local_channel_public_keys().funding_pubkey;
    let remote = state.get_remote_channel_public_keys().funding_pubkey;

    let local_first = state.get_musig2_agg_context(true);
    let local_first_again = state.get_musig2_agg_context(true);
    assert!(Arc::ptr_eq(&local_first, &local_first_again), "cached");

    let remote_first = state.get_musig2_agg_context(false);
    assert!(
        !Arc::ptr_eq(&local_first, &remote_first),
        "one per key order"
    );

    // The cache must not change the aggregated key for either order.
    assert_eq!(
        local_first.aggregated_pubkey::<Pubkey>(),
        KeyAggContext::new([local, remote])
            .unwrap()
            .aggregated_pubkey::<Pubkey>()
    );
    assert_eq!(
        remote_first.aggregated_pubkey::<Pubkey>(),
        KeyAggContext::new([remote, local])
            .unwrap()
            .aggregated_pubkey::<Pubkey>()
    );

    // The deterministic helper reuses whichever cached order matches its ordering.
    let deterministic = state.get_deterministic_musig2_agg_context();
    assert_eq!(
        deterministic.aggregated_pubkey::<Pubkey>(),
        KeyAggContext::new(state.order_things_for_musig2(local, remote))
            .unwrap()
            .aggregated_pubkey::<Pubkey>()
    );
    let expected = if state.should_local_go_first_in_musig2() {
        local_first
    } else {
        remote_first
    };
    assert!(Arc::ptr_eq(&deterministic, &expected));
}

fn channel_state_with_limits(
    local_constraints: ChannelConstraints,
    remote_constraints: ChannelConstraints,
) -> ChannelActorState {
    let signer = InMemorySigner::generate_from_seed(b"tlc-limit-local");
    let remote_signer = InMemorySigner::generate_from_seed(b"tlc-limit-remote");

    ChannelActorState {
        channel_signer: ChannelSigner::local(signer.clone()),
        pending_messages: Default::default(),
        core: ChannelActorData {
            state: ChannelState::ChannelReady,
            signing_context: Default::default(),
            public_channel_info: None,
            local_tlc_info: ChannelTlcInfo {
                enabled: true,
                ..Default::default()
            },
            remote_tlc_info: None,
            local_pubkey: signer.funding_key.pubkey(),
            remote_pubkey: remote_signer.funding_key.pubkey(),
            id: Hash256::from([42; 32]),
            funding_tx: None,
            funding_tx_confirmed_at: None,
            funding_udt_type_script: None,
            is_acceptor: false,
            is_one_way: false,
            to_local_amount: 1_000_000,
            to_remote_amount: 1_000_000,
            local_reserved_ckb_amount: 0,
            remote_reserved_ckb_amount: 0,
            commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
            commitment_delay_epoch: DEFAULT_COMMITMENT_DELAY_EPOCHS,
            funding_fee_rate: DEFAULT_FEE_RATE,
            local_channel_public_keys: signer.get_base_public_keys(),
            session: new_session(Hash256::from([42; 32]), &signer.funding_key, None),
            signer: Some(signer),
            commitment_numbers: CommitmentNumbers::default(),
            local_constraints,
            remote_constraints,
            tlc_state: TlcState::default(),
            retryable_tlc_operations: VecDeque::new(),
            waiting_forward_tlc_tasks: HashMap::new(),
            remote_shutdown_script: None,
            local_shutdown_script: Script::default(),
            remote_commitment_points: Vec::new(),
            remote_channel_public_keys: Some(remote_signer.get_base_public_keys()),
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            reestablishing: false,
            created_at: SystemTime::now(),
            pending_replay_updates: vec![],
            last_was_revoke: false,
            connectivity_state: ChannelConnectivityState::Online,
            external_funding: None,
            channel_features: Default::default(),
        },
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: VecDeque::new(),
        waiting_peer_response: None,
        reestablish_timer: None,
        network: None,
        recovery_peer_v2: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        ephemeral_config: ChannelEphemeralConfig::default(),
        funding_abort_detail: None,
        private_key: None,
        needs_backup: false,
        musig2_agg_context_local_first: Default::default(),
        musig2_agg_context_remote_first: Default::default(),
        pending_remove_effects_v2: 0,
        pending_payment_notifications: Default::default(),
        replayed_tlc_updates_v2: 0,
        resigning_outgoing_v2: false,
    }
}

#[test]
fn external_v2_material_is_atomic_and_nonce_exhaustion_fails_closed() {
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    let signer = state.signer.take().unwrap();
    state.channel_signer = ChannelSigner::external();
    let owner = signer.funding_key.pubkey();
    let other_owner = state
        .remote_channel_public_keys
        .as_ref()
        .unwrap()
        .funding_pubkey;
    let nonces: Vec<_> = (1..=3)
        .map(|byte| musig2::SecNonce::build([byte; 32]).build().public_nonce())
        .collect();
    let point = signer.get_commitment_point(3);
    let other_point = signer.get_commitment_point(4);
    assert!(state
        .install_public_material_v2(nonces.clone(), vec![(3, point), (3, other_point)])
        .is_err());
    assert!(state.signing_context.v2_nonce_pool.is_empty());
    assert!(state.signing_context.v2_commitment_points.is_empty());
    state
        .install_public_material_v2(nonces, vec![(3, point)])
        .unwrap();
    let first = state
        .allocate_v2(owner, 2, NoncePurposeV2::Commitment)
        .unwrap();
    let replay = state
        .allocate_v2(owner, 2, NoncePurposeV2::Commitment)
        .unwrap();
    assert_eq!(first.public_nonce, replay.public_nonce);
    let reverse = state
        .allocate_v2(other_owner, 2, NoncePurposeV2::Commitment)
        .unwrap();
    let closing = state
        .allocate_v2(owner, 0, NoncePurposeV2::Closing)
        .unwrap();
    assert_ne!(first.public_nonce, reverse.public_nonce);
    assert_ne!(first.public_nonce, closing.public_nonce);
    assert!(state.refresh_own_nonce_v2(3).is_err());
    assert_eq!(state.signing_context.v2_allocations.len(), 3);
    assert!(state
        .signing_context
        .v2_allocations
        .iter()
        .all(|record| record.seed == [0; 32]));
    assert!(state.signer.is_none());
    assert!(state
        .install_public_material_v2(vec![first.public_nonce], vec![])
        .is_err());
    assert!(state.signing_context.v2_nonce_pool.is_empty());
}

fn failed_remove_ack_confirmed_tlc(tlc_id: TLCId, amount: u128) -> TlcInfo {
    let status = if tlc_id.is_offered() {
        TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed)
    } else {
        TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed)
    };

    TlcInfo {
        upstream_tlc_key: None,
        status,
        tlc_id,
        amount,
        payment_hash: Hash256::from([u64::from(tlc_id) as u8; 32]),
        total_amount: None,
        payment_secret: None,
        attempt_id: None,
        expiry: 0,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
        is_trampoline_hop: false,
        created_at: CommitmentNumbers {
            local: 1,
            remote: 1,
        },
        removed_reason: Some(RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::TemporaryChannelFailure),
            &NO_SHARED_SECRET,
        ))),
        forwarding_tlc: None,
        removed_confirmed_at: Some(1),
        applied_flags: AppliedFlags::REMOVE,
    }
}

fn committed_tlc(tlc_id: TLCId, amount: u128) -> TlcInfo {
    let status = if tlc_id.is_offered() {
        TlcStatus::Outbound(OutboundTlcStatus::Committed)
    } else {
        TlcStatus::Inbound(InboundTlcStatus::Committed)
    };

    TlcInfo {
        upstream_tlc_key: None,
        status,
        tlc_id,
        amount,
        payment_hash: Hash256::from([u64::from(tlc_id) as u8 + 1; 32]),
        total_amount: None,
        payment_secret: None,
        attempt_id: None,
        expiry: 0,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
        is_trampoline_hop: false,
        created_at: CommitmentNumbers {
            local: 1,
            remote: 1,
        },
        removed_reason: None,
        forwarding_tlc: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::ADD,
    }
}

#[test]
fn remote_commitment_points_keep_references_and_prune_obsolete_rounds() {
    let mut state = channel_state_with_limits(
        ChannelConstraints::new(1_000_000, 1),
        ChannelConstraints::new(1_000_000, 1),
    );
    let remote_signer = InMemorySigner::generate_from_seed(b"tlc-limit-remote");
    let mut offered = committed_tlc(TLCId::Offered(0), 10);
    offered.created_at = CommitmentNumbers {
        local: 2,
        remote: 3,
    };
    let mut received = committed_tlc(TLCId::Received(0), 10);
    received.created_at = CommitmentNumbers {
        local: 4,
        remote: 5,
    };
    state.tlc_state.add_offered_tlc(offered.clone());
    state.tlc_state.add_received_tlc(received.clone());
    state.remote_commitment_points = (2..=5)
        .map(|number| (number, remote_signer.get_commitment_point(number)))
        .collect();
    let offered_keys = state.get_tlc_pubkeys(&offered).unwrap();
    let received_keys = state.get_tlc_pubkeys(&received).unwrap();

    assert_eq!(state.channel_features, ChannelFeatures::V2);
    for number in 6..=40 {
        state.commitment_numbers = CommitmentNumbers {
            local: number,
            remote: number - 1,
        };
        state.append_remote_commitment_point(remote_signer.get_commitment_point(number));
        assert_eq!(state.get_tlc_pubkeys(&offered).unwrap(), offered_keys);
        assert_eq!(state.get_tlc_pubkeys(&received).unwrap(), received_keys);
        assert!(state.remote_commitment_points.len() <= 6);
    }
    let point_numbers = |state: &ChannelActorState| {
        state
            .remote_commitment_points
            .iter()
            .map(|(number, _)| *number)
            .collect::<Vec<_>>()
    };
    assert_eq!(point_numbers(&state), vec![2, 3, 4, 5, 39, 40]);
    state.apply_remove_tlc(offered.tlc_id);
    assert_eq!(point_numbers(&state), vec![4, 5, 39, 40]);
    state.apply_remove_tlc(received.tlc_id);
    assert_eq!(point_numbers(&state), vec![39, 40]);
    for number in 41..=64 {
        state.commitment_numbers = CommitmentNumbers {
            local: number,
            remote: number - 1,
        };
        state.append_remote_commitment_point(remote_signer.get_commitment_point(number));
        assert_eq!(point_numbers(&state), vec![number - 1, number]);
    }
}

#[test]
fn signature_deadline_reserves_normal_onchain_delay() {
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    state.commitment_delay_epoch = EpochNumberWithFraction::new(6, 0, 1).full_value();
    let reserve = tlc_expiry_delay(&EpochNumberWithFraction::from_full_value(
        state.commitment_delay_epoch,
    ));
    let mut tlc = committed_tlc(TLCId::Received(0), 10);
    tlc.status = TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitAck);
    tlc.expiry = 100_000 + reserve;
    state.tlc_state.received_tlcs.tlcs.push(tlc);
    assert_eq!(state.pending_tlc_signature_deadline(), Some(100_000));
    // A removal is still at risk until its acknowledgement is confirmed.
    state.tlc_state.received_tlcs.tlcs[0].removed_reason = Some(RemoveTlcReason::RemoveTlcFail(
        TlcErrPacket::new(TlcErr::new(TlcErrorCode::ExpiryTooSoon), &NO_SHARED_SECRET),
    ));
    assert_eq!(state.pending_tlc_signature_deadline(), Some(100_000));
    state.tlc_state.received_tlcs.tlcs[0].removed_confirmed_at = Some(1);
    assert_eq!(state.pending_tlc_signature_deadline(), None);
    // Include outgoing updates not yet signed, unlike normal committed-TLC maintenance.
    let mut offered = committed_tlc(TLCId::Offered(0), 10);
    offered.status = TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced);
    offered.expiry = reserve - 1;
    state.tlc_state.offered_tlcs.tlcs.push(offered);
    assert_eq!(state.pending_tlc_signature_deadline(), Some(0));
}

#[test]
fn periodic_signature_deadline_guards_healthy_received_tlcs_and_catches_stalled_local_announced() {
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    state.state = ChannelState::ChannelReady;
    state.ephemeral_config.is_internal_channel = true;
    state.commitment_delay_epoch = EpochNumberWithFraction::new(6, 0, 1).full_value();
    let reserve = tlc_expiry_delay(&EpochNumberWithFraction::from_full_value(
        state.commitment_delay_epoch,
    ));

    // Healthy internal channel with an expiring Received TLC must NOT trigger force-close.
    let mut received = committed_tlc(TLCId::Received(0), 10);
    received.status = TlcStatus::Inbound(InboundTlcStatus::Committed);
    received.expiry = 100_000 + reserve;
    state.tlc_state.received_tlcs.tlcs.push(received);
    assert!(!state.periodic_signature_tlc_deadline_reached(100_000));
    assert!(!state.periodic_signature_tlc_deadline_reached(100_000 + reserve));

    // Stalled LocalAnnounced offered TLC on internal channel MUST trigger at deadline.
    let mut offered = committed_tlc(TLCId::Offered(0), 10);
    offered.status = TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced);
    offered.expiry = 100_000 + reserve;
    state.tlc_state.offered_tlcs.tlcs.push(offered);
    state.tlc_state.waiting_ack = true;
    assert!(!state.periodic_signature_tlc_deadline_reached(99_999));
    assert!(state.periodic_signature_tlc_deadline_reached(100_000));

    // Even if waiting_ack was cleared by reestablish while offered TLC stays LocalAnnounced,
    // the deadline check still triggers.
    state.tlc_state.waiting_ack = false;
    assert!(state.periodic_signature_tlc_deadline_reached(100_000));

    // Once force-close has started waiting for on-chain confirmation, stop re-triggering.
    state.state = ChannelState::ShuttingDown(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION);
    assert!(!state.periodic_signature_tlc_deadline_reached(100_000));
}

#[test]
fn onchain_received_fulfill_can_mark_announce_wait_ack_tlc() {
    let mut tlc_state = TlcState::default();
    let mut tlc = committed_tlc(TLCId::Received(0), 10);
    let payment_hash = tlc.payment_hash;
    tlc.status = TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitAck);
    tlc_state.received_tlcs.tlcs.push(tlc);

    tlc_state.set_received_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: Hash256::from([42; 32]),
        }),
    );

    let tlc = tlc_state
        .get(&TLCId::Received(0))
        .expect("received TLC remains in state");
    assert_eq!(tlc.payment_hash, payment_hash);
    assert!(matches!(
        tlc.removed_reason,
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
    assert_eq!(tlc.inbound_status(), InboundTlcStatus::LocalRemoved);
}

#[test]
fn failed_remove_ack_confirmed_tlcs_do_not_consume_number_limits() {
    let mut state = channel_state_with_limits(
        ChannelConstraints::new(1_000_000, 1),
        ChannelConstraints::new(1_000_000, 1),
    );
    state
        .tlc_state
        .offered_tlcs
        .tlcs
        .push(failed_remove_ack_confirmed_tlc(TLCId::Offered(0), 10));
    assert!(
        state.check_tlc_limits(10, true).is_ok(),
        "settled failed offered TLC should not consume remote incoming number limit"
    );

    let mut state = channel_state_with_limits(
        ChannelConstraints::new(1_000_000, 1),
        ChannelConstraints::new(1_000_000, 1),
    );
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .push(failed_remove_ack_confirmed_tlc(TLCId::Received(0), 10));
    assert!(
        state.check_tlc_limits(10, false).is_ok(),
        "settled failed received TLC should not consume local incoming number limit"
    );
}

#[test]
fn failed_remove_ack_confirmed_tlcs_do_not_consume_value_limits() {
    let mut state = channel_state_with_limits(
        ChannelConstraints::new(100, 10),
        ChannelConstraints::new(100, 10),
    );
    state
        .tlc_state
        .offered_tlcs
        .tlcs
        .push(failed_remove_ack_confirmed_tlc(TLCId::Offered(0), 90));
    assert!(
        state.check_tlc_limits(20, true).is_ok(),
        "settled failed offered TLC should not consume remote incoming value limit"
    );

    let mut state = channel_state_with_limits(
        ChannelConstraints::new(100, 10),
        ChannelConstraints::new(100, 10),
    );
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .push(failed_remove_ack_confirmed_tlc(TLCId::Received(0), 90));
    assert!(
        state.check_tlc_limits(20, false).is_ok(),
        "settled failed received TLC should not consume local incoming value limit"
    );
}

#[test]
fn active_received_tlcs_still_limit_peer_adds_with_settled_failed_tlcs_present() {
    let mut state = channel_state_with_limits(
        ChannelConstraints::new(100, 1),
        ChannelConstraints::new(100, 1),
    );
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .push(failed_remove_ack_confirmed_tlc(TLCId::Received(0), 90));
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .push(committed_tlc(TLCId::Received(1), 90));

    assert!(matches!(
        state.check_tlc_limits(1, false),
        Err(ProcessingChannelError::TlcNumberExceedLimit)
    ));

    let mut state = channel_state_with_limits(
        ChannelConstraints::new(100, 10),
        ChannelConstraints::new(100, 10),
    );
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .push(failed_remove_ack_confirmed_tlc(TLCId::Received(0), 90));
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .push(committed_tlc(TLCId::Received(1), 90));
    assert!(matches!(
        state.check_tlc_limits(20, false),
        Err(ProcessingChannelError::TlcValueInflightExceedLimit)
    ));
}

#[test]
fn received_tlc_fulfill_cannot_be_overridden_by_expiry_fail() {
    use crate::store::sample::StoreSample;

    for algorithm in [HashAlgorithm::Sha256, HashAlgorithm::CkbHash] {
        let mut state = channel_state_with_limits(
            ChannelConstraints::new(1_000_000, 10),
            ChannelConstraints::new(1_000_000, 10),
        );
        state.session = ChannelActorState::samples(0).remove(0).session.clone();
        let preimage = Hash256::from([7; 32]);
        let fulfill = RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        });
        let fail = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &NO_SHARED_SECRET,
        ));
        let mut tlc = committed_tlc(TLCId::Received(0), 10);
        tlc.hash_algorithm = algorithm;
        tlc.payment_hash = algorithm.hash(preimage).into();
        state.tlc_state.received_tlcs.tlcs.push(tlc.clone());
        // A fulfill for another TLC, even with the same hash, does not authorize this one.
        state
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::RemoveTlc(
                TLCId::Received(1),
                fulfill.clone(),
            ));
        assert!(state.pending_received_tlc_claim_preimage(&tlc).is_none());
        assert!(state
            .check_remove_tlc_with_reason(tlc.tlc_id, &fail)
            .is_ok());

        // An older queued failure must neither hide nor supersede a valid fulfill.
        state
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::RemoveTlc(tlc.tlc_id, fail.clone()));
        state
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::RemoveTlc(
                tlc.tlc_id,
                fulfill.clone(),
            ));
        assert_eq!(
            state.pending_received_tlc_claim_preimage(&tlc),
            Some(preimage)
        );
        assert!(matches!(
            state.check_remove_tlc_with_reason(tlc.tlc_id, &fail),
            Err(ProcessingChannelError::RepeatedProcessing(_))
        ));
        assert!(state
            .check_remove_tlc_with_reason(tlc.tlc_id, &fulfill)
            .is_ok());
        tlc.payment_hash = Hash256::from([9; 32]);
        assert!(state.pending_received_tlc_claim_preimage(&tlc).is_none());
        tlc.payment_hash = algorithm.hash(preimage).into();
        tlc.status = TlcStatus::Inbound(InboundTlcStatus::LocalRemoved);
        tlc.removed_reason = Some(fulfill);
        assert_eq!(
            state.pending_received_tlc_claim_preimage(&tlc),
            Some(preimage)
        );
        tlc.removed_confirmed_at = Some(1);
        assert!(state.pending_received_tlc_claim_preimage(&tlc).is_none());
        tlc.removed_confirmed_at = None;
        tlc.removed_reason = Some(fail);
        assert!(state.pending_received_tlc_claim_preimage(&tlc).is_none());
    }
}

#[test]
fn reject_invalid_script_hash_type_in_channel_validation_and_tenant_opening() {
    let state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    let invalid_script = Script::new_builder()
        .hash_type(ckb_types::packed::Byte::new(0xff))
        .build();

    assert!(!state.check_shutdown_fee_valid(&invalid_script, DEFAULT_FEE_RATE));

    let err = ChannelActorState::check_accept_channel_parameters_for_values(
        1_000_000,
        1_000_000,
        6_100_000_000,
        6_100_000_000,
        DEFAULT_COMMITMENT_FEE_RATE,
        &None,
        &invalid_script,
        1,
        1,
    )
    .unwrap_err();
    assert!(matches!(err, ProcessingChannelError::InvalidParameter(_)));

    let udt_err = ChannelActorState::check_accept_channel_parameters_for_values(
        1_000_000,
        1_000_000,
        14_200_000_000,
        14_200_000_000,
        DEFAULT_COMMITMENT_FEE_RATE,
        &Some(invalid_script.clone()),
        &Script::default(),
        1,
        1,
    )
    .unwrap_err();
    assert!(matches!(
        udt_err,
        ProcessingChannelError::InvalidParameter(_)
    ));

    let mut tenant_state = state;
    tenant_state.core.remote_shutdown_script = Some(invalid_script);
    assert!(tenant_state
        .tenant_opening_result(Transaction::default())
        .is_err());
}

impl ChannelActorState {
    /// Give synthetic on-chain fixtures genuine signed V2 bootstrap history.
    #[cfg(test)]
    pub(crate) fn sign_test_bootstrap_v2(&mut self) -> ProcessingChannelResult {
        let state = self.state;
        let tlcs = std::mem::take(&mut self.tlc_state);
        self.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty());
        self.funding_tx.get_or_insert_with(Default::default);
        self.local_channel_public_keys = self.signer.as_ref().unwrap().get_base_public_keys();
        let remote_signer = fiber_types::InMemorySigner::generate_from_seed(&[1; 32]);
        self.remote_channel_public_keys = Some(remote_signer.get_base_public_keys());
        self.commitment_numbers = fiber_types::CommitmentNumbers {
            local: 1,
            remote: 1,
        };
        self.musig2_agg_context_local_first = OnceLock::new();
        self.musig2_agg_context_remote_first = OnceLock::new();
        let mut peer = self.clone();
        peer.channel_signer = ChannelSigner::local(remote_signer.clone());
        peer.signer = Some(remote_signer);
        peer.local_channel_public_keys = peer.signer.as_ref().unwrap().get_base_public_keys();
        peer.remote_channel_public_keys = Some(self.local_channel_public_keys.clone());
        peer.to_local_amount = self.to_remote_amount;
        peer.to_remote_amount = self.to_local_amount;
        peer.local_reserved_ckb_amount = self.remote_reserved_ckb_amount;
        peer.remote_reserved_ckb_amount = self.local_reserved_ckb_amount;
        self.session = new_session(
            self.id,
            &self
                .channel_signer
                .local_material()
                .ok_or_else(|| invalid("External signing required"))?
                .funding_key,
            None,
        );
        let local_nonce = self.session.own.public_nonce.clone();
        peer.session = new_session(
            peer.id,
            &peer.signer.as_ref().unwrap().funding_key,
            Some(local_nonce),
        );
        let remote_nonce = peer.session.own.public_nonce.clone();
        let session = &mut self.session;
        session.remote_nonce = Some(remote_nonce.clone());
        session.bootstrap_remote_nonce = Some(remote_nonce);
        let (ours, _) = self.prepare_outgoing_v2(true)?;
        let (theirs, _) = peer.prepare_outgoing_v2(true)?;
        let verified = self.stage_incoming_v2(&theirs, true)?;
        self.accept_commitment_v2(&theirs, true, verified)?;
        let verified = peer.stage_incoming_v2(&ours, true)?;
        peer.accept_commitment_v2(&ours, true, verified)?;
        self.commitment_numbers = fiber_types::CommitmentNumbers {
            local: 2,
            remote: 2,
        };
        peer.commitment_numbers = self.commitment_numbers;
        self.ready_wire_message()?;
        let FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(ready)) =
            peer.ready_wire_message()?
        else {
            return Err(invalid("Expected V2 bootstrap Ready"));
        };
        let session = &mut self.session;
        session.remote_number = ready.next_commitment_number;
        session.remote_nonce = Some(ready.next_commitment_nonce.clone());
        session.remote_ready = Some(ready.into());
        if tlcs.waiting_ack {
            self.prepare_outgoing_v2(false)?;
        }
        self.tlc_state = tlcs;
        self.state = state;
        self.validate_session()
    }
}

pub(crate) fn handle_test_message(
    state: &mut ChannelActorState,
    message: crate::tests::TestChannelMessage,
) {
    match message {
        crate::tests::TestChannelMessage::GetPendingMessages(reply) => {
            let _ = reply.send(crate::tests::TestPendingMessages {
                pending_peer_message_count: state.pending_messages.peer_messages.len(),
                has_pending_peer_commitment: state
                    .pending_messages
                    .peer_messages
                    .iter()
                    .any(|(m, _)| matches!(m, FiberChannelMessage::CommitmentSignedV2(_))),
            });
        }
    }
}

pub(crate) async fn handle_test_command(command: crate::tests::TestChannelCommand) {
    match command {
        crate::tests::TestChannelCommand::Pause(reply) => {
            let (release, wait) = oneshot::channel();
            let _ = reply.send(release);
            let _ = wait.await;
        }
    }
}

#[tokio::test]
async fn test_onchain_tlc_relay_confirmed_notifies_scoped_trampoline_session_key() {
    // Capture every emitted `PaymentTlcUpdated(session_key)` directly.
    let noticed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<PaymentSessionKey>::new()));
    struct KeyCollector(std::sync::Arc<std::sync::Mutex<Vec<PaymentSessionKey>>>);

    #[async_trait::async_trait]
    impl Actor for KeyCollector {
        type Msg = FiberActorMessage;
        type State = ();
        type Arguments = ();

        async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
            Ok(())
        }

        async fn handle(
            &self,
            _: ActorRef<Self::Msg>,
            message: Self::Msg,
            _: &mut (),
        ) -> Result<(), ActorProcessingErr> {
            match message {
                FiberActorMessage::Command(FiberActorCommand::PaymentTlcUpdated(key)) => {
                    self.0.lock().unwrap().push(key);
                }
                FiberActorMessage::Command(FiberActorCommand::GetInflightPaymentCount(reply)) => {
                    let _ = reply.send(Ok(0));
                }
                _ => {}
            }
            Ok(())
        }
    }

    let (store, _dir) = crate::tests::test_utils::generate_store();
    let (network, task) = Actor::spawn(None, KeyCollector(noticed.clone()), ())
        .await
        .unwrap();
    let (myself, channel_task) = Actor::spawn(None, NoticeChannelMailbox, ()).await.unwrap();
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    let mut tlc = failed_remove_ack_confirmed_tlc(TLCId::Offered(0), 100);
    let upstream = fiber_types::UpstreamTlcKey {
        incoming_channel_id: Hash256::from([0x77; 32]),
        incoming_tlc_id: 9,
    };
    let preimage = Hash256::from([0x42; 32]);
    let reason = RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    });
    tlc.attempt_id = Some(1);
    tlc.upstream_tlc_key = Some(upstream);
    tlc.removed_reason = Some(reason.clone());
    let expected_key = tlc.session_key();
    // Fulfilled offered TLC has already left `tlc_state` and lives only in `remove_effects`.
    state.session.remove_effects.push((tlc, Some(1)));
    store.insert_channel_actor_state_for_test(state.clone());
    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store,
        None,
    );
    actor
        .handle_event(
            &myself,
            &mut state,
            ChannelEvent::OnChainTlcRelayConfirmed(TLCId::Offered(0), reason),
        )
        .await
        .unwrap();
    actor.persist_state_and_publish_pending_effects(&mut state);
    let _ = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    myself.stop(None);
    channel_task.await.unwrap();
    network.stop(None);
    task.await.unwrap();
    assert_eq!(
        *noticed.lock().unwrap(),
        vec![expected_key],
        "retiring the last remove_effects entry must notify the scoped trampoline PaymentSessionKey"
    );
}

#[tokio::test]
async fn test_maintain_ready_channel_tlcs_recovers_only_persisted_received_selection() {
    let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Hash256>::new()));
    struct SettleCollector(std::sync::Arc<std::sync::Mutex<Vec<Hash256>>>);

    #[async_trait::async_trait]
    impl Actor for SettleCollector {
        type Msg = FiberActorMessage;
        type State = ();
        type Arguments = ();

        async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
            Ok(())
        }

        async fn handle(
            &self,
            _: ActorRef<Self::Msg>,
            message: Self::Msg,
            _: &mut (),
        ) -> Result<(), ActorProcessingErr> {
            match message {
                FiberActorMessage::Command(FiberActorCommand::SettleReceivedHoldTlcSet(hash))
                | FiberActorMessage::Command(FiberActorCommand::SettleTlcSet(hash, _)) => {
                    self.0.lock().unwrap().push(hash);
                }
                FiberActorMessage::Command(FiberActorCommand::GetInflightPaymentCount(reply)) => {
                    let _ = reply.send(Ok(0));
                }
                _ => {}
            }
            Ok(())
        }
    }

    let (store, _dir) = crate::tests::test_utils::generate_store();
    let (network, net_task) = Actor::spawn(None, SettleCollector(commands.clone()), ())
        .await
        .unwrap();
    let (myself, chan_task) = Actor::spawn(None, NoticeChannelMailbox, ()).await.unwrap();

    let preimage = Hash256::from([0x61; 32]);
    let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
    let invoice = crate::invoice::InvoiceBuilder::new(crate::invoice::Currency::Fibd)
        .amount(Some(1_000))
        .payment_hash(payment_hash)
        .hash_algorithm(HashAlgorithm::CkbHash)
        .build()
        .expect("build test invoice");
    store
        .insert_invoice(invoice, Some(preimage))
        .expect("insert invoice");
    store
        .update_invoice_status(&payment_hash, CkbInvoiceStatus::Received)
        .expect("mark received");

    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    let expiry = now_timestamp_as_millis_u64() + 600_000;
    state.tlc_state.add_received_tlc(TlcInfo {
        upstream_tlc_key: None,
        status: TlcStatus::Inbound(InboundTlcStatus::Committed),
        tlc_id: TLCId::Received(0),
        amount: 1_000,
        payment_hash,
        total_amount: None,
        payment_secret: None,
        attempt_id: None,
        expiry,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [0x22; 32],
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::default(),
        removed_reason: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::empty(),
        forwarding_tlc: None,
    });
    store.insert_channel_actor_state_for_test(state.clone());

    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store.clone(),
        None,
    );

    // Without a persisted selection in `payment_hold_tlcs`, maintenance on a `Received`
    // invoice must NOT request fulfillment (prevents double-settling competing channels).
    actor
        .handle_event(&myself, &mut state, ChannelEvent::MaintainChannelTlcs)
        .await
        .unwrap();
    let _ = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    assert!(
        commands.lock().unwrap().is_empty(),
        "unselected channel on Received invoice must not request fulfillment"
    );

    // Once `(state.id, 0)` is persisted in `payment_hold_tlcs`, maintenance must drive
    // recovery via `SettleReceivedHoldTlcSet`.
    store.insert_payment_hold_tlc(
        payment_hash,
        crate::fiber::types::HoldTlc {
            channel_id: state.id,
            tlc_id: 0,
            hold_expire_at: expiry,
        },
    );
    actor
        .handle_event(&myself, &mut state, ChannelEvent::MaintainChannelTlcs)
        .await
        .unwrap();
    let _ = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        *commands.lock().unwrap(),
        vec![payment_hash],
        "persisted selection on Received invoice must request recovery"
    );

    myself.stop(None);
    chan_task.await.unwrap();
    network.stop(None);
    net_task.await.unwrap();
}

#[tokio::test]
async fn test_external_funding_acceptor_second_signer_replays_tx_signatures() {
    let sent_tx_sigs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<Vec<u8>>>::new()));
    struct PeerMsgCollector(std::sync::Arc<std::sync::Mutex<Vec<Vec<Vec<u8>>>>>);

    #[async_trait::async_trait]
    impl Actor for PeerMsgCollector {
        type Msg = FiberActorMessage;
        type State = ();
        type Arguments = ();

        async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
            Ok(())
        }

        async fn handle(
            &self,
            _: ActorRef<Self::Msg>,
            message: Self::Msg,
            _: &mut (),
        ) -> Result<(), ActorProcessingErr> {
            match message {
                FiberActorMessage::Command(FiberActorCommand::SendFiberMessage(msg)) => {
                    if let FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::TxSignatures(sig),
                    ) = msg.message
                    {
                        self.0.lock().unwrap().push(sig.witnesses);
                    }
                }
                FiberActorMessage::Command(FiberActorCommand::GetInflightPaymentCount(reply)) => {
                    let _ = reply.send(Ok(0));
                }
                _ => {}
            }
            Ok(())
        }
    }

    let (network, net_task) = Actor::spawn(None, PeerMsgCollector(sent_tx_sigs.clone()), ())
        .await
        .unwrap();
    let (myself, chan_task) = Actor::spawn(None, NoticeChannelMailbox, ()).await.unwrap();

    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    // Make local the second signer (`to_local_amount > to_remote_amount`).
    state.to_local_amount = 2_000_000;
    state.to_remote_amount = 1_000_000;
    assert!(!state.should_local_send_tx_signatures_first());
    state.is_acceptor = true;
    state.ephemeral_config.external_funding.enabled = true;
    state.state = ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::OUR_CHANNEL_READY);
    state.sign_test_bootstrap_v2().unwrap();
    let expected_witnesses = vec![vec![0xaa; 65], vec![0xbb; 65]];
    let packed_witnesses: Vec<Bytes> = expected_witnesses.iter().map(|w| w.pack()).collect();
    state.funding_tx = Some(
        Transaction::default()
            .as_advanced_builder()
            .set_witnesses(packed_witnesses)
            .build()
            .data(),
    );
    state.network = Some(network.clone().into());

    // Externally funded acceptor signing second must replay its final `TxSignatures`.
    state.resume_funding(&myself);
    let _ = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    assert_eq!(*sent_tx_sigs.lock().unwrap(), vec![expected_witnesses]);

    // Externally funded opener (not acceptor) must still skip this non-external replay branch.
    sent_tx_sigs.lock().unwrap().clear();
    state.is_acceptor = false;
    state.resume_funding(&myself);
    let _ = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    assert!(sent_tx_sigs.lock().unwrap().is_empty());

    myself.stop(None);
    chan_task.await.unwrap();
    network.stop(None);
    net_task.await.unwrap();
}

struct NoopFiberActor;

#[async_trait::async_trait]
impl Actor for NoopFiberActor {
    type Msg = FiberActorMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        _: Self::Msg,
        _: &mut (),
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
}

#[tokio::test]
async fn test_tx_signatures_replay_in_awaiting_channel_ready_idempotent_and_rejects_conflict() {
    let (store, _dir) = crate::tests::test_utils::generate_store();
    let (network, net_task) = Actor::spawn(None, NoopFiberActor, ()).await.unwrap();
    let (myself, chan_task) = Actor::spawn(None, NoticeChannelMailbox, ()).await.unwrap();

    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    // Local is the first signer (`to_local_amount < to_remote_amount`), receiving the
    // second signer's replayed complete `TxSignatures` while already in `AwaitingChannelReady`.
    state.to_local_amount = 1_000_000;
    state.to_remote_amount = 2_000_000;
    assert!(state.should_local_send_tx_signatures_first());
    state.state = ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::empty());
    state.sign_test_bootstrap_v2().unwrap();
    let witnesses = vec![vec![0x11; 65], vec![0x22; 65]];
    let packed_witnesses: Vec<Bytes> = witnesses.iter().map(|w| w.pack()).collect();
    state.funding_tx = Some(
        Transaction::default()
            .as_advanced_builder()
            .set_witnesses(packed_witnesses)
            .build()
            .data(),
    );
    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store,
        None,
    );

    // Identical witness replay in `AwaitingChannelReady` is accepted idempotently.
    let channel_id = state.id;
    let matching = actor
        .handle_peer_message(
            &myself,
            &mut state,
            FiberChannelMessage::TxSignatures(TxSignatures {
                channel_id,
                witnesses: witnesses.clone(),
            }),
        )
        .await;
    assert!(
        matching.is_ok(),
        "matching TxSignatures replay in AwaitingChannelReady must succeed: {matching:?}"
    );

    // Conflicting witnesses in `AwaitingChannelReady` must still be rejected.
    let conflicting = actor
        .handle_peer_message(
            &myself,
            &mut state,
            FiberChannelMessage::TxSignatures(TxSignatures {
                channel_id,
                witnesses: vec![vec![0x99; 65], vec![0x22; 65]],
            }),
        )
        .await;
    assert!(
        matches!(conflicting, Err(ProcessingChannelError::InvalidState(_))),
        "conflicting TxSignatures in AwaitingChannelReady must fail with InvalidState: {conflicting:?}"
    );

    myself.stop(None);
    chan_task.await.unwrap();
    network.stop(None);
    net_task.await.unwrap();
}

#[tokio::test]
async fn test_tx_abort_bypasses_pending_queue_while_awaiting_external_signature() {
    let (store, _dir) = crate::tests::test_utils::generate_store();
    let (network, net_task) = Actor::spawn(None, NoopFiberActor, ()).await.unwrap();
    let (myself, chan_task) = Actor::spawn(None, NoticeChannelMailbox, ()).await.unwrap();

    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    state.state = ChannelState::SigningCommitment(SigningCommitmentFlags::empty());
    let key_agg_ctx = state.get_musig2_agg_context(true).as_ref().clone();
    let agg_nonce = musig2::AggNonce::sum([
        state.session.own.public_nonce.clone(),
        state.session.own.public_nonce.clone(),
    ]);
    let content = fiber_types::Musig2SigningContent {
        v2: None,
        slot: fiber_types::NonceSlot {
            purpose: fiber_types::NoncePurpose::Commitment,
            commitment_number: 0,
        },
        commitment_counter: None,
        key_agg_ctx,
        agg_nonce,
        content: fiber_types::Musig2SignableContent::CommitmentTransaction(Transaction::default()),
    };
    state.signing_context.pending_signature = Some(fiber_types::PendingChannelSignature {
        request_id: fiber_types::SignatureRequestId(Hash256::from([0x55; 32])),
        request: fiber_types::ChannelSignatureRequest::SendClosingSigned { content },
    });
    assert!(state.signing_context.is_awaiting_signature());

    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store,
        None,
    );
    let channel_id = state.id;

    // Once funding can no longer abort (e.g. `ChannelReady`), `TxAbort` must still queue
    // behind an external signature so peer message buffering/overflow protection holds.
    let mut ready_state = state.clone();
    ready_state.state = ChannelState::ChannelReady;
    actor
        .handle_peer_message(
            &myself,
            &mut ready_state,
            FiberChannelMessage::TxAbort(TxAbort {
                channel_id,
                message: b"late abort".to_vec(),
            }),
        )
        .await
        .expect("late TxAbort should buffer while awaiting signature");
    assert_eq!(ready_state.state, ChannelState::ChannelReady);
    assert_eq!(ready_state.pending_messages.peer_messages.len(), 1);

    actor
        .handle_peer_message(
            &myself,
            &mut state,
            FiberChannelMessage::TxAbort(TxAbort {
                channel_id,
                message: b"peer abandoned opening".to_vec(),
            }),
        )
        .await
        .expect("TxAbort while awaiting external signature must be handled immediately");

    assert_eq!(
        state.state,
        ChannelState::Closed(CloseFlags::FUNDING_ABORTED),
        "eligible TxAbort must abort immediately rather than buffering behind external signature"
    );
    assert!(state.pending_messages.peer_messages.is_empty());

    chan_task.await.unwrap();
    network.stop(None);
    net_task.await.unwrap();
}

#[tokio::test]
async fn test_force_close_and_confirmed_close_clear_reestablish_state() {
    let (store, _dir) = crate::tests::test_utils::generate_store();
    let (network, net_task) = Actor::spawn(None, NoopFiberActor, ()).await.unwrap();

    // 1. Local force-close initiated while reestablishing must clear recovery/wait state.
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    state.state = ChannelState::ChannelReady;
    state.network = Some(network.clone().into());
    state.sign_test_bootstrap_v2().unwrap();
    state.reestablishing = true;
    state.reestablish_timer = Some(ReestablishTimer::RunningSince(1_000));
    state.waiting_peer_response = Some(1_000);

    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store,
        None,
    );
    actor
        .handle_shutdown_command(
            &mut state,
            ShutdownCommand {
                close_script: None,
                fee_rate: None,
                force: true,
            },
        )
        .await
        .expect("force shutdown while reestablishing must succeed");
    assert!(
        !state.reestablishing,
        "force shutdown must clear reestablishing flag"
    );
    assert!(
        state.reestablish_timer.is_none(),
        "force shutdown must clear reestablish_timer"
    );
    assert!(
        state.waiting_peer_response.is_none(),
        "force shutdown must clear waiting_peer_response"
    );

    // 2. Confirmed remote force-close while reestablishing must also clear recovery/wait state.
    let mut remote_closed =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    remote_closed.state = ChannelState::ChannelReady;
    remote_closed.network = Some(network.clone().into());
    remote_closed.reestablishing = true;
    remote_closed.reestablish_timer = Some(ReestablishTimer::RunningSince(2_000));
    remote_closed.waiting_peer_response = Some(2_000);
    remote_closed
        .update_close_transaction_confirmed(H256::default(), true, false)
        .await
        .expect("confirmed remote force close must succeed");
    assert!(
        !remote_closed.reestablishing,
        "confirmed close must clear reestablishing flag"
    );
    assert!(
        remote_closed.reestablish_timer.is_none(),
        "confirmed close must clear reestablish_timer"
    );
    assert!(
        remote_closed.waiting_peer_response.is_none(),
        "confirmed close must clear waiting_peer_response"
    );

    network.stop(None);
    net_task.await.unwrap();
}

#[tokio::test]
async fn test_receive_shutdown_v2_replay_suppressed_after_force_close() {
    let (store, _dir) = crate::tests::test_utils::generate_store();
    let sent_closing_signed = std::sync::Arc::new(std::sync::Mutex::new(0usize));

    struct ClosingSignedCounter(std::sync::Arc<std::sync::Mutex<usize>>);
    #[async_trait::async_trait]
    impl Actor for ClosingSignedCounter {
        type Msg = FiberActorMessage;
        type State = ();
        type Arguments = ();
        async fn pre_start(&self, _: ActorRef<Self::Msg>, _: ()) -> Result<(), ActorProcessingErr> {
            Ok(())
        }
        async fn handle(
            &self,
            _: ActorRef<Self::Msg>,
            message: Self::Msg,
            _: &mut (),
        ) -> Result<(), ActorProcessingErr> {
            match message {
                FiberActorMessage::Command(FiberActorCommand::SendFiberMessage(msg)) => {
                    if matches!(
                        msg.message,
                        FiberMessage::ChannelNormalOperation(FiberChannelMessage::ClosingSignedV2(
                            _
                        ))
                    ) {
                        *self.0.lock().unwrap() += 1;
                    }
                }
                FiberActorMessage::Command(FiberActorCommand::GetInflightPaymentCount(reply)) => {
                    let _ = reply.send(Ok(0));
                }
                _ => {}
            }
            Ok(())
        }
    }

    let (network, net_task) =
        Actor::spawn(None, ClosingSignedCounter(sent_closing_signed.clone()), ())
            .await
            .unwrap();
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    state.state = ChannelState::ShuttingDown(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION);
    state.network = Some(network.clone().into());
    let own = state
        .allocate_v2(
            state.local_channel_public_keys.funding_pubkey,
            0,
            NoncePurposeV2::Closing,
        )
        .unwrap();
    let remote_shutdown = ShutdownV2 {
        channel_id: state.id,
        close_script: state.get_local_shutdown_script(),
        fee_rate: 1_000,
        closing_nonce: own.public_nonce.clone(),
    };
    let packed_remote: fiber_types::gen::fiber::ShutdownV2 = remote_shutdown.clone().into();
    let response = ClosingSignedV2 {
        channel_id: state.id,
        partial_signature: PartialSignature::from(musig2::secp256k1::Scalar::ONE),
    };
    state.session.closing = Some(fiber_types::ClosingSessionV2 {
        peer_wait_started_at: 1_000,
        superseded_by_force_close: true,
        own,
        shutdown: packed_remote.clone(),
        remote_shutdown: Some(packed_remote),
        transaction: None,
        response: Some(response.into()),
        remote_response: None,
    });

    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        store,
        None,
    );
    actor
        .receive_shutdown_v2(&mut state, remote_shutdown)
        .await
        .expect("idempotent replay of ShutdownV2 after force close should return Ok");
    let _ = ractor::call_t!(
        network,
        |reply| FiberActorMessage::new_command(FiberActorCommand::GetInflightPaymentCount(reply)),
        5000
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        *sent_closing_signed.lock().unwrap(),
        0,
        "ShutdownV2 replay must not resend ClosingSignedV2 once superseded_by_force_close"
    );

    network.stop(None);
    net_task.await.unwrap();
}

#[tokio::test]
async fn test_cooperative_close_confirmed_while_awaiting_force_close_closes_channel() {
    let (network, net_task) = Actor::spawn(None, NoopFiberActor, ()).await.unwrap();
    for initial_flags in [
        ShuttingDownFlags::OUR_SHUTDOWN_SENT,
        ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
    ] {
        let mut state =
            channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
        state.state = ChannelState::ShuttingDown(initial_flags);
        state.network = Some(network.clone().into());
        let tx_hash = H256::from([0x5a; 32]);
        state
            .update_close_transaction_confirmed(tx_hash.clone(), false, false)
            .await
            .expect("cooperative close confirmation must be accepted in any ShuttingDown state");
        assert_eq!(state.state, ChannelState::Closed(CloseFlags::COOPERATIVE));
        assert_eq!(state.shutdown_transaction_hash, Some(tx_hash));
    }
    network.stop(None);
    net_task.await.unwrap();
}

#[tokio::test]
async fn test_periodic_reconnect_preserves_peer_response_wait_budget() {
    let (network, net_task) = Actor::spawn(None, NoopFiberActor, ()).await.unwrap();
    let mut state =
        channel_state_with_limits(ChannelConstraints::default(), ChannelConstraints::default());
    state.network = Some(network.clone().into());
    state.state = ChannelState::ShuttingDown(ShuttingDownFlags::OUR_SHUTDOWN_SENT);
    let own = state
        .allocate_v2(
            state.local_channel_public_keys.funding_pubkey,
            0,
            NoncePurposeV2::Closing,
        )
        .unwrap();
    let shutdown = ShutdownV2 {
        channel_id: state.id,
        close_script: state.get_local_shutdown_script(),
        fee_rate: 1_000,
        closing_nonce: own.public_nonce.clone(),
    };
    let prior_elapsed = 20_000u64;
    let now = now_timestamp_as_millis_u64();
    state.session.closing = Some(fiber_types::ClosingSessionV2 {
        peer_wait_started_at: now.saturating_sub(prior_elapsed),
        superseded_by_force_close: false,
        own,
        shutdown: shutdown.into(),
        remote_shutdown: None,
        transaction: None,
        response: None,
        remote_response: None,
    });

    state.on_peer_disconnected();
    assert!(
        state.ephemeral_config.paused_peer_response_wait_ms >= prior_elapsed,
        "disconnect must preserve elapsed online peer-response wait"
    );

    state.refresh_peer_response_wait_v2_after_reestablish();
    let resumed_started = state.session.closing.as_ref().unwrap().peer_wait_started_at;
    let resumed_elapsed = now_timestamp_as_millis_u64().saturating_sub(resumed_started);
    assert!(
        resumed_elapsed >= prior_elapsed,
        "reestablish must not reset accrued online peer-response wait (got {resumed_elapsed}ms < {prior_elapsed}ms)"
    );

    state.session.closing = None;
    state.refresh_peer_response_wait_v2_after_reestablish();
    assert_eq!(state.ephemeral_config.paused_peer_response_wait_ms, 0);

    network.stop(None);
    net_task.await.unwrap();
}
