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
