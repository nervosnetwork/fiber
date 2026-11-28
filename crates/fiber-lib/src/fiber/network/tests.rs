use super::*;
use crate::tests::TestCommand;

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn get_payment_stored_preimage_respects_status_and_namespace() {
    use crate::{
        fiber::payment::SendPaymentDataBuilder, fiber_types::HashAlgorithm, store::NodeNamespace,
        tests::NetworkNode,
    };

    let node = NetworkNode::new_with_node_name("get-payment-preimage").await;
    let store = node.store.namespaced(NodeNamespace::hosted_tenant("payer"));
    let other_store = node.store.namespaced(NodeNamespace::hosted_tenant("other"));
    let core = FiberActorCore::new(
        mpsc::channel(1).0,
        node.chain_actor.clone(),
        store.clone(),
        None,
        node.network_graph.clone(),
        node.chain_client.clone(),
    );
    let preimage = Hash256::from([42; 32]);
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
    let request = SendPaymentDataBuilder::new(node.pubkey, 1_000, payment_hash)
        .max_fee_amount(Some(0))
        .build()
        .unwrap();
    let mut session = PaymentSession::new_session(&store, request, 1);
    session.set_inflight_status();

    other_store.insert_preimage(payment_hash, preimage);
    store.insert_preimage(payment_hash, preimage);
    assert!(core.on_get_payment(&payment_hash.into()).is_err());
    store.remove_preimage(&payment_hash);
    store.insert_payment_session(session.clone());
    let response = core.on_get_payment(&payment_hash.into()).unwrap();
    assert_eq!(response.status, PaymentStatus::Inflight);
    assert_eq!(response.payment_preimage, None);

    store.insert_preimage(payment_hash, preimage);
    let response = core.on_get_payment(&payment_hash.into()).unwrap();
    assert_eq!(response.status, PaymentStatus::Inflight);
    // An unassociated secret is not proof that this payment execution was fulfilled.
    assert_eq!(response.payment_preimage, None);
    assert_eq!(response.last_updated_at, session.last_updated_at);
    assert_eq!(
        store.get_payment_session(payment_hash).unwrap().status,
        PaymentStatus::Inflight
    );

    session.set_failed_status("payment failed");
    store.insert_payment_session(session.clone());
    let response = core.on_get_payment(&payment_hash.into()).unwrap();
    assert_eq!(response.status, PaymentStatus::Failed);
    assert_eq!(response.payment_preimage, None);

    // Completed attempts retain their preimage after channel storage is cleaned up.
    let mut attempt = session.new_attempt(1, node.pubkey, node.pubkey, Vec::new());
    attempt.set_success_status();
    attempt.preimage = Some(preimage);
    store.insert_attempt(attempt);
    session.set_success_status();
    store.insert_payment_session(session);
    store.remove_preimage(&payment_hash);
    let response = core.on_get_payment(&payment_hash.into()).unwrap();
    assert_eq!(response.status, PaymentStatus::Success);
    assert_eq!(response.payment_preimage, Some(preimage));
}

fn expect_admitted(admission: InboundFiberAdmission) -> FiberIngressPermit {
    match admission {
        InboundFiberAdmission::Admitted(permit) => permit,
        other => panic!("expected admitted Fiber message, got {other:?}"),
    }
}

fn test_peer_message_policy(
    max_entries: usize,
    max_in_flight_messages: u32,
    max_in_flight_bytes: u64,
) -> Arc<StdMutex<PeerMessagePolicy>> {
    Arc::new(StdMutex::new(PeerMessagePolicy::with_limits(
        max_entries,
        max_in_flight_messages,
        max_in_flight_bytes,
    )))
}

#[test]
fn fiber_ingress_permit_releases_after_malformed_message() {
    let peer = Privkey::from_slice(&[11u8; 32]).pubkey();
    let policy = test_peer_message_policy(8, 1, 100);

    let permit = expect_admitted(admit_inbound_fiber_message(&policy, &peer, 1, 0));
    assert!(FiberMessage::from_molecule_slice(&[0xff]).is_err());
    drop(permit);
    assert_eq!(
        policy.lock().expect("peer message policy lock").in_flight(),
        (0, 0)
    );
}

#[test]
fn fiber_ingress_permit_releases_when_actor_send_drops_message() {
    let peer = Privkey::from_slice(&[15u8; 32]).pubkey();
    let policy = test_peer_message_policy(8, 1, 100);
    let permit = expect_admitted(admit_inbound_fiber_message(&policy, &peer, 1, 0));
    let message = NetworkActorMessage::new_event(PublicNetworkEvent::FiberMessage(
        peer,
        FiberMessage::init(Init {
            features: FeatureVector::default(),
            chain_hash: get_chain_hash(),
        }),
        Some(permit),
    ));

    drop(message);
    assert_eq!(
        policy.lock().expect("peer message policy lock").in_flight(),
        (0, 0)
    );
}

#[test]
fn channel_ready_retry_debounce_coalesces_pending_trailing_scan() {
    let outpoint = OutPoint::default();
    let mut last_scans = HashMap::from([(outpoint.clone(), 1_000)]);
    let mut pending_trailing_scans = HashSet::new();

    let first_decision = decide_channel_ready_retry_scan(
        &mut last_scans,
        &mut pending_trailing_scans,
        outpoint.clone(),
        2_000,
    );

    assert_eq!(
        first_decision,
        ChannelReadyRetryScanDecision::ScheduleTrailing(Duration::from_millis(59_000))
    );
    assert!(pending_trailing_scans.contains(&outpoint));

    let second_decision = decide_channel_ready_retry_scan(
        &mut last_scans,
        &mut pending_trailing_scans,
        outpoint,
        3_000,
    );

    assert_eq!(
        second_decision,
        ChannelReadyRetryScanDecision::AlreadyScheduled
    );
    assert_eq!(last_scans.get(&OutPoint::default()), Some(&1_000));
}

#[test]
fn completed_force_closed_channel_does_not_need_offline_reconciliation() {
    let waiting = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_REMOTE | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    assert!(should_reconcile_closed_channel_without_live_actor(waiting));

    let completed = ChannelState::Closed(CloseFlags::UNCOOPERATIVE_REMOTE);
    assert!(!should_reconcile_closed_channel_without_live_actor(
        completed
    ));
}

#[test]
fn network_message_envelope_separates_public_runtime_from_fiber_core() {
    assert!(matches!(
        NetworkActorMessage::new_command(PublicNetworkCommand::MaintainConnections),
        NetworkActorMessage::PublicCommand(PublicNetworkCommand::MaintainConnections)
    ));
    assert!(matches!(
        NetworkActorMessage::new_command(FiberActorCommand::CheckChannels),
        NetworkActorMessage::Fiber(FiberActorMessage::Command(FiberActorCommand::CheckChannels))
    ));

    let peer = Privkey::from_slice(&[23u8; 32]).pubkey();
    assert!(matches!(
        NetworkActorMessage::new_event(PublicNetworkEvent::FiberMessage(
            peer,
            FiberMessage::init(Init {
                features: FeatureVector::default(),
                chain_hash: get_chain_hash(),
            }),
            None,
        )),
        NetworkActorMessage::PublicEvent(PublicNetworkEvent::FiberMessage(..))
    ));
    assert!(matches!(
        NetworkActorMessage::new_event(FiberActorEvent::RetryPendingPaymentsForChannel(
            OutPoint::default(),
        )),
        NetworkActorMessage::Fiber(FiberActorMessage::Event(
            FiberActorEvent::RetryPendingPaymentsForChannel(..)
        ))
    ));
}

#[test]
fn public_messages_cannot_be_derived_as_fiber_messages() {
    let message = NetworkActorMessage::new_command(PublicNetworkCommand::MaintainConnections);
    assert!(FiberActorMessage::try_from(message).is_err());

    let message = NetworkActorMessage::new_event(PublicNetworkEvent::GossipMessageUpdates(
        GossipMessageUpdates::new(Vec::new()),
    ));
    assert!(FiberActorMessage::try_from(message).is_err());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn hosted_tenant_activity_ignores_applied_removed_tlcs_and_settled_closed_channels() {
    use crate::store::sample::StoreSample;
    use fiber_types::{
        ChannelState, CloseFlags, CommitmentNumbers, HashAlgorithm, InboundTlcStatus, TLCId,
        TlcInfo, TlcStatus,
    };

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let store = crate::store::open_store(temp_dir.path()).expect("open store");
    let mut channel = ChannelActorState::samples(7)
        .into_iter()
        .next()
        .expect("sample channel");
    channel.state = ChannelState::ChannelReady;
    channel.tlc_state.offered_tlcs.tlcs.clear();
    channel.tlc_state.received_tlcs.tlcs.clear();

    let make_tlc =
        |id: u64, removed_confirmed_at: Option<u64>, applied_flags: AppliedFlags| TlcInfo {
            status: TlcStatus::Inbound(InboundTlcStatus::Committed),
            tlc_id: TLCId::Received(id),
            amount: 1_000,
            payment_hash: Hash256::from([id as u8; 32]),
            total_amount: None,
            payment_secret: None,
            attempt_id: None,
            expiry: 1_000_000,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0u8; 32],
            is_trampoline_hop: false,
            created_at: CommitmentNumbers::default(),
            removed_reason: None,
            forwarding_tlc: None,
            removed_confirmed_at,
            applied_flags,
            upstream_tlc_key: None,
        };

    // 1. Confirmed AND applied failed/cancelled TLCs awaiting next commitment cleanup are idle.
    channel.tlc_state.received_tlcs.tlcs.push(make_tlc(
        1,
        Some(12345),
        AppliedFlags::ADD | AppliedFlags::REMOVE,
    ));
    assert_eq!(count_active_hosted_tlcs(&channel, &store), 0);

    // 2. Confirmed removal that has not been applied yet is still active.
    channel
        .tlc_state
        .received_tlcs
        .tlcs
        .push(make_tlc(2, Some(12345), AppliedFlags::ADD));
    assert_eq!(count_active_hosted_tlcs(&channel, &store), 1);

    // 3. Live uncommitted/active TLC is still active.
    channel
        .tlc_state
        .received_tlcs
        .tlcs
        .push(make_tlc(3, None, AppliedFlags::ADD));
    assert_eq!(count_active_hosted_tlcs(&channel, &store), 2);

    // 4. Force-closed channel awaiting on-chain settlement remains busy.
    channel.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    assert_eq!(count_active_hosted_tlcs(&channel, &store), 2);

    // 5. Cooperatively closed channel is idle.
    channel.state = ChannelState::Closed(CloseFlags::COOPERATIVE);
    assert_eq!(count_active_hosted_tlcs(&channel, &store), 0);
}

fn test_pub_nonce(peer: Pubkey) -> musig2::PubNonce {
    let compressed = peer.serialize();
    let mut bytes = [0u8; 66];
    bytes[..33].copy_from_slice(&compressed);
    bytes[33..].copy_from_slice(&compressed);
    musig2::PubNonce::from_bytes(&bytes).expect("valid test pub nonce")
}

#[test]
fn to_be_accepted_channels_prune_expired_frees_stale_slots() {
    let mut pending = ToBeAcceptedChannels {
        total_number_limit: 2,
        total_bytes_limit: 1024 * 1024,
        ttl_ms: 10_000,
        map: HashMap::default(),
    };
    let peer = Privkey::from_slice(&[31u8; 32]).pubkey();
    let stale_id = Hash256::from([1u8; 32]);
    let fresh_id = Hash256::from([2u8; 32]);
    let dummy_open = OpenChannelV2 {
        chain_hash: get_chain_hash(),
        channel_id: stale_id,
        funding_udt_type_script: None,
        funding_amount: 100_000_000_000,
        shutdown_script: Script::default(),
        reserved_ckb_amount: 0,
        funding_fee_rate: 1_000,
        commitment_fee_rate: 1_000,
        commitment_delay_epoch: 10,
        max_tlc_value_in_flight: 100_000_000_000,
        max_tlc_number_in_flight: 16,
        funding_pubkey: peer,
        tlc_basepoint: peer,
        first_per_commitment_point: peer,
        second_per_commitment_point: peer,
        channel_announcement_nonce: None,
        initial_commitment_nonce: test_pub_nonce(peer),
        channel_flags: crate::fiber::types::ChannelFlags::empty(),
        channel_features: 1,
    };
    pending.map.insert(
        stale_id,
        (
            peer,
            PendingOpenChannel {
                open_channel: dummy_open.clone(),
                created_at: 1_000,
            },
        ),
    );
    pending.map.insert(
        fresh_id,
        (
            peer,
            PendingOpenChannel {
                open_channel: dummy_open,
                created_at: 8_000,
            },
        ),
    );

    let expired = pending.prune_expired(12_000);
    assert_eq!(expired, vec![stale_id]);
    assert!(!pending.map.contains_key(&stale_id));
    assert!(pending.map.contains_key(&fresh_id));
}

#[test]
fn matches_cooperative_close_tx_detects_signed_closing_tx_hash() {
    use crate::store::sample::StoreSample;
    let mut channel = ChannelActorState::samples(11)
        .into_iter()
        .next()
        .expect("sample channel");
    channel.session.closing = None;
    let peer = Privkey::from_slice(&[33u8; 32]).pubkey();
    let tx = ckb_types::core::TransactionBuilder::default().build();
    let expected_hash = tx.hash();
    let other_hash = Byte32::from_slice(&[9u8; 32]).unwrap();
    assert!(!matches_cooperative_close_tx(&channel, &expected_hash));

    channel.session.closing = Some(crate::fiber_types::ClosingSessionV2 {
        peer_wait_started_at: 0,
        superseded_by_force_close: false,
        own: crate::fiber_types::SigningNonceV2 {
            seed: [0u8; 32],
            channel_id: channel.id,
            owner: peer,
            number: 0,
            purpose: crate::fiber_types::NoncePurposeV2::Closing,
            public_nonce: test_pub_nonce(peer),
            context: None,
            signature: None,
        },
        shutdown: crate::fiber_types::gen::fiber::ShutdownV2::default(),
        remote_shutdown: None,
        transaction: Some(tx.data()),
        response: None,
        remote_response: None,
    });
    assert!(matches_cooperative_close_tx(&channel, &expected_hash));
    assert!(!matches_cooperative_close_tx(&channel, &other_hash));
}

#[test]
fn onchain_upstream_relay_already_handled_accepts_closed_upstream_channel() {
    use crate::store::sample::StoreSample;
    let mut upstream = ChannelActorState::samples(13)
        .into_iter()
        .next()
        .expect("sample channel");
    upstream.tlc_state.offered_tlcs.tlcs.clear();
    upstream.tlc_state.received_tlcs.tlcs.clear();
    let reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
        TlcErr::new(TlcErrorCode::PermanentChannelFailure),
        &[0u8; 32],
    ));
    upstream.state = ChannelState::ChannelReady;
    assert!(!onchain_upstream_relay_already_handled(
        &upstream, 1, &reason,
    ));

    upstream.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    assert!(onchain_upstream_relay_already_handled(
        &upstream, 1, &reason,
    ));
}

/// Test-only state carried by `FiberActorState`.
#[derive(Default)]
pub(crate) struct TestState {
    pub(crate) message_hold: Option<TestFiberMessageHold>,
    pub(crate) held_messages: VecDeque<FiberMessageWithTarget>,
    pub(crate) trampoline_settlement_paused: bool,
}

impl<S, C> NetworkActor<S, C>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + ChannelOpenRecordStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    pub(crate) fn handle_test_command(state: &mut FiberActorState<S, C>, command: TestCommand) {
        match command {
            TestCommand::InstallChannelActor(channel_id, actor, reply) => {
                state.channels.insert(channel_id, actor);
                let _ = reply.send(());
            }
            TestCommand::SetMessageHold(hold, reply) => {
                state.test_state.message_hold = Some(hold);
                let _ = reply.send(());
            }
            TestCommand::TakeHeldMessages(reply) => {
                state.test_state.message_hold = None;
                let messages = state.test_state.held_messages.drain(..).collect();
                let _ = reply.send(messages);
            }
            TestCommand::HeldMessageCount(reply) => {
                let _ = reply.send(state.test_state.held_messages.len());
            }
            TestCommand::SetTrampolineSettlementPaused(paused, reply) => {
                state.test_state.trampoline_settlement_paused = paused;
                let _ = reply.send(());
            }
            TestCommand::GetPaymentActor(hash, reply) => {
                let _ = reply.send(state.inflight_payments.get(&hash).cloned());
            }
            TestCommand::SetPaymentActor(hash, actor, reply) => {
                assert!(state.inflight_payments.insert(hash, actor).is_none());
                let _ = reply.send(());
            }
            // Routed to the async command handler instead.
            TestCommand::ReleaseHeldMessages(_) => {}
        }
    }

    pub(crate) async fn release_held_messages(
        state: &mut FiberActorState<S, C>,
        reply: RpcReplyPort<Result<(), String>>,
    ) {
        state.test_state.message_hold = None;
        while let Some(FiberMessageWithTarget { target, message }) =
            state.test_state.held_messages.pop_front()
        {
            if let Err(error) = Box::pin(state.send_fiber_message(&target, message)).await {
                let remaining = state.test_state.held_messages.len();
                let noun = if remaining == 1 {
                    "message"
                } else {
                    "messages"
                };
                let _ = reply.send(Err(format!(
                    "failed to release held Fiber message: {error}; \
                     {remaining} unattempted {noun} remains queued"
                )));
                return;
            }
        }
        let _ = reply.send(Ok(()));
    }
}

/// Hold an outbound Fiber message when the test asked for it. Returns true when held.
pub(crate) fn hold_fiber_message<S, C>(
    state: &mut FiberActorState<S, C>,
    message: &FiberMessageWithTarget,
) -> bool {
    let Some(hold) = state.test_state.message_hold.as_ref() else {
        return false;
    };
    if !hold.matches(message) {
        return false;
    }
    let remaining = hold.remaining.get() - 1;
    match NonZeroUsize::new(remaining) {
        Some(remaining) => {
            state
                .test_state
                .message_hold
                .as_mut()
                .expect("matching test hold")
                .remaining = remaining;
        }
        None => state.test_state.message_hold = None,
    }
    state.test_state.held_messages.push_back(message.clone());
    true
}
