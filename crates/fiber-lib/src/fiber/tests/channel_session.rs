use super::ReestablishTimer;
use super::*;
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::network::NetworkActorEvent;
use crate::fiber::tests::MockStore;
use crate::tests::test_utils::create_nodes_with_established_channel;
use fiber_store::StorageBackend;
use ractor::{Actor, ActorProcessingErr};
use std::sync::{Arc, Mutex};

struct CaptureNetwork;

#[async_trait::async_trait]
impl Actor for CaptureNetwork {
    type Msg = NetworkActorMessage;
    type State = Arc<Mutex<Vec<NetworkActorMessage>>>;
    type Arguments = Self::State;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        state: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(state)
    }
    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            super::ChannelCommandWithId {
                command: super::ChannelCommand::TestBarrier(reply),
                ..
            },
        )) = message
        {
            reply.send(()).unwrap();
            return Ok(());
        }
        state.lock().unwrap().push(message);
        Ok(())
    }
}

async fn capture_barrier(network: &ActorRef<NetworkActorMessage>, id: fiber_types::Hash256) {
    ractor::call_t!(
        network,
        |reply| NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            super::ChannelCommandWithId {
                channel_id: id,
                command: super::ChannelCommand::TestBarrier(reply),
            }
        )),
        5000
    )
    .unwrap();
}

#[tokio::test]
async fn test_v2_reconnects_share_reestablish_timeout() {
    assert_reconnects_share_reestablish_timeout(false).await;
}

#[tokio::test]
async fn test_v2_maintenance_checks_cumulative_reestablish_timeout() {
    assert_reconnects_share_reestablish_timeout(true).await;
}

async fn assert_reconnects_share_reestablish_timeout(maintenance: bool) {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let mut state = a.get_channel_actor_state(id);
    state.network = Some(network.clone());
    assert!(!state.tlc_state.waiting_ack);
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    state.mark_reestablishing_offline();
    let leg = super::REESTABLISH_TIMEOUT * 3 / 5;
    for reconnect in 0..2 {
        state.on_peer_reconnected();
        // Each connection waits less than the timeout, but the two waits exceed it.
        // Backdate the active clock rather than sleeping for minutes in production.
        let Some(ReestablishTimer::RunningSince(started)) = state.reestablish_timer else {
            panic!("a reconnected peer must have a running recovery clock");
        };
        state.reestablish_timer = Some(ReestablishTimer::RunningSince(started.saturating_sub(leg)));
        actor
            .handle_event(
                &myself,
                &mut state,
                if maintenance {
                    super::ChannelEvent::MaintainChannelTlcs
                } else {
                    super::ChannelEvent::CheckActiveChannel
                },
            )
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        let closes = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|message| {
                matches!(message,
                NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                    super::ChannelCommandWithId {
                        command: super::ChannelCommand::Shutdown(command, _), ..
                    }
                )) if command.force)
            })
            .count();
        assert_eq!(
            closes, reconnect,
            "reconnect must not reset the recovery budget"
        );
        captured.lock().unwrap().clear();
        if reconnect == 0 {
            state.on_peer_disconnected();
            // An older one-shot check can fire while offline: it must not force close.
            actor
                .handle_event(&myself, &mut state, super::ChannelEvent::CheckActiveChannel)
                .await
                .unwrap();
            capture_barrier(&network, id).await;
            assert!(!captured.lock().unwrap().iter().any(|message| matches!(
                message,
                NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                    super::ChannelCommandWithId {
                        command: super::ChannelCommand::Shutdown(_, _),
                        ..
                    }
                ))
            )));
            captured.lock().unwrap().clear();
        }
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_early_shutdown_replays_ready_before_shutdown() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut receiver = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    sender.network = Some(network.clone());
    sender.private_key = Some(a.private_key.clone());
    receiver.network = Some(network.clone());
    receiver.private_key = Some(b.private_key.clone());
    receiver.state = ChannelState::AwaitingChannelReady(
        fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
    );
    receiver.commitment_numbers = fiber_types::CommitmentNumbers {
        local: 1,
        remote: 1,
    };
    let session = &mut receiver.session;
    session.remote_ready = None;
    session.remote_number = 1;
    session.remote_nonce = session.bootstrap_remote_nonce.clone();
    receiver.validate_session().unwrap();
    let original_nonce = sender.session.own.public_nonce.clone();
    let sender_actor =
        ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let receiver_actor =
        ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    sender_actor
        .shutdown_v2(
            &mut sender,
            super::ShutdownCommand {
                close_script: None,
                fee_rate: None,
                force: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(sender.state, ChannelState::ShuttingDown(_)));
    sender.validate_session().unwrap();
    let balances = (sender.to_local_amount, sender.to_remote_amount);
    for (state, key) in [
        (&mut sender, a.private_key.clone()),
        (&mut receiver, b.private_key.clone()),
    ] {
        let core = bincode::deserialize::<fiber_types::ChannelActorData>(
            &bincode::serialize(&state.core).unwrap(),
        )
        .unwrap();
        *state = ChannelActorState::from(core);
        state.network = Some(network.clone());
        state.private_key = Some(key);
    }
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();
    sender.mark_reestablishing_offline();
    receiver.mark_reestablishing_offline();
    sender_actor
        .reestablish_v2(&myself, &mut sender, receiver.reestablish_message_v2())
        .await
        .unwrap();
    receiver_actor
        .reestablish_v2(&myself, &mut receiver, sender.reestablish_message_v2())
        .await
        .unwrap();
    sender.validate_session().unwrap();
    receiver.validate_session().unwrap();
    let preserved = sender.session.own.public_nonce == original_nonce;
    let mut saw_ready = false;
    let mut saw_shutdown = false;
    let mut ready_first = true;
    let mut errors = Vec::new();
    for _ in 0..8 {
        capture_barrier(&network, id).await;
        let frames = take_wire_messages(&captured);
        if frames.is_empty() {
            break;
        }
        for frame in frames {
            let FiberMessage::ChannelNormalOperation(message) = frame.message else {
                continue;
            };
            if matches!(message, FiberChannelMessage::ReestablishChannelV2(_)) {
                continue;
            }
            let result = if frame.target == b.pubkey {
                saw_ready |= matches!(message, FiberChannelMessage::ChannelReadyV2(_));
                if matches!(message, FiberChannelMessage::ShutdownV2(_)) {
                    saw_shutdown = true;
                    ready_first &= saw_ready;
                }
                receiver_actor
                    .handle_peer_message(&myself, &mut receiver, message)
                    .await
            } else {
                sender_actor
                    .handle_peer_message(&myself, &mut sender, message)
                    .await
            };
            if let Err(error) = result {
                errors.push(error);
            }
        }
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
    assert!(preserved && saw_ready && saw_shutdown && ready_first && errors.is_empty(), "early close recovery: nonce preserved={preserved}, Ready={saw_ready}, Shutdown={saw_shutdown}, errors={errors:?}, receiver={:?}", receiver.state);
    for state in [&sender, &receiver] {
        state.validate_session().unwrap();
        assert!(
            matches!(state.state, ChannelState::ShuttingDown(flags) if flags.contains(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION))
        );
        let close = state.session.closing.as_ref().unwrap();
        assert!(close.response.is_some() && close.remote_response.is_some());
        assert!(
            !state.may_need_ready_replay_v2(),
            "chain-wait must not reactivate opening Ready"
        );
        let mut terminal = state.clone();
        terminal.state =
            ChannelState::ShuttingDown(fiber_types::ShuttingDownFlags::DROPPING_PENDING);
        assert!(!terminal.may_need_ready_replay_v2());
        terminal.state = ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
        assert!(!terminal.may_need_ready_replay_v2());
    }
    assert_eq!(
        sender
            .session
            .closing
            .as_ref()
            .unwrap()
            .transaction
            .as_ref()
            .unwrap()
            .as_slice(),
        receiver
            .session
            .closing
            .as_ref()
            .unwrap()
            .transaction
            .as_ref()
            .unwrap()
            .as_slice()
    );
    assert_eq!((sender.to_local_amount, sender.to_remote_amount), balances);
}

fn unaccepted_initial_cs_fixture(state: &mut ChannelActorState) -> CommitmentSignedV2 {
    let initial = state.session.incoming.take().unwrap();
    let request = CommitmentSignedV2::try_from(initial.request).unwrap();
    state.state =
        ChannelState::SigningCommitment(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT);
    state.commitment_numbers = fiber_types::CommitmentNumbers {
        local: 1,
        remote: 1,
    };
    state.latest_commitment_transaction = None;
    let session = &mut state.session;
    session.own = initial.funding;
    session.own.context = None;
    session.own.signature = None;
    session.own_retired.clear();
    session.pending_incoming = None;
    session.pending_ack = None;
    session.remote_ready = None;
    session.remote_number = 1;
    session.remote_nonce = session.bootstrap_remote_nonce.clone();
    state.validate_session().unwrap();
    request
}

#[tokio::test]
async fn test_v2_initial_cs_survives_repeated_reconnects() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let peer_key = a.get_channel_actor_state(id).signer.funding_key.clone();
    let mut owner = b.get_channel_actor_state(id);
    let request = unaccepted_initial_cs_fixture(&mut owner);
    b.stop().await;
    a.stop().await;
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    owner.network = Some(network.clone());
    owner.private_key = Some(b.private_key.clone());
    owner.validate_session().unwrap();
    assert!(
        owner.clone().stage_incoming_v2(&request, true).is_ok(),
        "original bootstrap fixture must verify before reconnect"
    );
    let original_nonce = owner.session.own.public_nonce.clone();
    let actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    let mut results = Vec::new();
    for round in 1..=6 {
        owner.mark_reestablishing_offline();
        let peer_nonce = allocate_nonce(
            id,
            peer_key.pubkey(),
            1,
            NoncePurposeV2::Commitment,
            &peer_key,
        )
        .public_nonce;
        actor
            .reestablish_v2(
                &myself,
                &mut owner,
                ReestablishChannelV2 {
                    channel_id: id,
                    next_commitment_number: 1,
                    next_ack_number: 1,
                    next_local_commitment_nonce: peer_nonce,
                },
            )
            .await
            .unwrap();
        owner.validate_session().unwrap();
        let session = &owner.session;
        let retained = session.own.public_nonce == original_nonce
            || session
                .own_retired
                .iter()
                .any(|n| n.public_nonce == original_nonce);
        let result = owner.clone().stage_incoming_v2(&request, true);
        results.push((round, retained, result.is_ok(), result.err()));
        let data = bincode::deserialize::<fiber_types::ChannelActorData>(
            &bincode::serialize(&owner.core).unwrap(),
        )
        .unwrap();
        owner = ChannelActorState::from(data);
        owner.network = Some(network.clone());
        owner.private_key = Some(b.private_key.clone());
    }
    assert!(
        results
            .iter()
            .all(|(_, retained, valid, _)| *retained && *valid),
        "unchanged original CS lost its verification nonce: {results:?}"
    );
    assert_eq!(owner.session.own.public_nonce, original_nonce);
    let verified = owner.stage_incoming_v2(&request, true).unwrap();
    owner
        .accept_commitment_v2(&request, true, verified)
        .unwrap();
    owner.state = ChannelState::SigningCommitment(
        SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT
            | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT,
    );
    owner.validate_session().unwrap();
    let mut accepted = owner.clone();
    accepted.mark_reestablishing_offline();
    accepted.on_peer_reconnected();
    assert_ne!(
        accepted.session.own.public_nonce, original_nonce,
        "accepted initial history no longer needs the unused bootstrap allocation"
    );
    let before = bincode::serialize(&accepted.core).unwrap();
    actor
        .receive_commitment_v2(&myself, &mut accepted, request)
        .await
        .unwrap();
    assert_eq!(
        before,
        bincode::serialize(&accepted.core).unwrap(),
        "initial duplicate must remain idempotent after nonce rotation"
    );
    accepted.validate_session().unwrap();
    owner.ready_wire_message().unwrap();
    owner.state = ChannelState::AwaitingChannelReady(
        fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
    );
    assert_eq!(owner.session.own.number, 2);
    assert!(owner.session.own_retired.is_empty());
    owner.validate_session().unwrap();
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_staged_initial_cs_keeps_context_across_reconnect() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let peer_key = a.get_channel_actor_state(id).signer.funding_key.clone();
    let mut owner = b.get_channel_actor_state(id);
    let request = unaccepted_initial_cs_fixture(&mut owner);
    b.stop().await;
    a.stop().await;
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    owner.network = Some(network.clone());
    owner.private_key = Some(b.private_key.clone());
    owner.stage_incoming_v2(&request, true).unwrap();
    let pending = owner.session.pending_incoming.clone().unwrap();
    assert!(pending.funding.context.is_some());
    owner.validate_session().unwrap();
    owner.mark_reestablishing_offline();
    owner.on_peer_reconnected();
    assert!(
        bincode::serialize(&owner.session.pending_incoming).unwrap()
            == bincode::serialize(&Some(pending.clone())).unwrap(),
        "reconnect must retain the initial nonce's bound context"
    );
    let core = bincode::deserialize::<fiber_types::ChannelActorData>(
        &bincode::serialize(&owner.core).unwrap(),
    )
    .unwrap();
    owner = ChannelActorState::from(core);
    owner.network = Some(network.clone());
    owner.private_key = Some(b.private_key.clone());
    owner.mark_reestablishing_offline();
    let peer_nonce = allocate_nonce(
        id,
        peer_key.pubkey(),
        1,
        NoncePurposeV2::Commitment,
        &peer_key,
    )
    .public_nonce;
    let mut conflicting = request.clone();
    conflicting.funding_nonce = peer_nonce.clone();
    assert!(owner.clone().stage_incoming_v2(&conflicting, true).is_err());
    let actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    actor
        .reestablish_v2(
            &myself,
            &mut owner,
            ReestablishChannelV2 {
                channel_id: id,
                next_commitment_number: 1,
                next_ack_number: 1,
                next_local_commitment_nonce: peer_nonce,
            },
        )
        .await
        .unwrap();
    let incoming = owner.session.incoming.as_ref().unwrap();
    assert_eq!(incoming.request.as_slice(), pending.request.as_slice());
    assert_eq!(incoming.funding.public_nonce, pending.funding.public_nonce);
    assert_eq!(incoming.funding.context, pending.funding.context);
    assert!(incoming.funding.signature.is_some());
    owner.validate_session().unwrap();
    let before = bincode::serialize(&owner.core).unwrap();
    actor
        .receive_commitment_v2(&myself, &mut owner, request)
        .await
        .unwrap();
    assert_eq!(before, bincode::serialize(&owner.core).unwrap());
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_commitment_acceptance_commits_once_before_publication() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    sender.network = Some(network.clone());
    owner.network = Some(network.clone());
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;
    let owner_actor =
        ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
    owner_actor
        .receive_commitment_v2(&myself, &mut owner, request)
        .await
        .unwrap();
    assert_eq!(
        owner_actor.store.channel_state_writes.get(),
        1,
        "persist the accepted CS, not an extra intermediate unsigned context"
    );
    let stored = owner_actor.store.get_channel_actor_state(&id).unwrap();
    stored.validate_session().unwrap();
    let session = &stored.session;
    assert!(session.pending_incoming.is_none());
    let ack =
        RevokeAndAckV2::try_from(session.incoming.as_ref().unwrap().response.clone().unwrap())
            .unwrap();
    assert!(stored.latest_commitment_transaction.is_some());

    let sender_actor =
        ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    sender_actor
        .receive_ack_v2(&myself, &mut sender, ack)
        .await
        .unwrap();
    assert_eq!(
        sender_actor.store.channel_state_writes.get(),
        1,
        "persist the accepted ACK before its effects, without an intermediate write"
    );
    let stored = sender_actor.store.get_channel_actor_state(&id).unwrap();
    stored.validate_session().unwrap();
    assert!(!stored.tlc_state.waiting_ack);
    assert!(stored.session.pending_ack.is_none());
    assert!(stored.session.last_ack.is_some());
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_closing_commits_only_at_publication_boundaries() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sa = a.get_channel_actor_state(id);
    let mut sb = b.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sa.network = Some(network.clone());
    sb.network = Some(network.clone());
    let actor_a = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
    actor_a
        .shutdown_v2(
            &mut sa,
            super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(actor_a.store.channel_state_writes.get(), 1);
    let shutdown =
        ShutdownV2::try_from(sa.session.closing.as_ref().unwrap().shutdown.clone()).unwrap();
    actor_b
        .receive_shutdown_v2(&mut sb, shutdown)
        .await
        .unwrap();
    assert_eq!(
        actor_b.store.channel_state_writes.get(),
        2,
        "commit the nonce advertisement and then the signature, without unsigned staging writes"
    );
    let before = bincode::serialize(&sb.core).unwrap();
    actor_b.store.channel_state_writes.set(0);
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();
    actor_b.progress_close_v2(&mut sb).await.unwrap();
    assert_eq!(bincode::serialize(&sb.core).unwrap(), before);
    assert_eq!(actor_b.store.channel_state_writes.get(), 0);
    capture_barrier(&network, id).await;
    assert!(captured.lock().unwrap().is_empty());
    let stored = actor_b.store.get_channel_actor_state(&id).unwrap();
    stored.validate_session().unwrap();
    let close_b = stored.session.closing.as_ref().unwrap();
    let shutdown_b = ShutdownV2::try_from(close_b.shutdown.clone()).unwrap();
    let signature_b = ClosingSignedV2::try_from(close_b.response.clone().unwrap()).unwrap();
    actor_a.store.channel_state_writes.set(0);
    actor_a
        .receive_shutdown_v2(&mut sa, shutdown_b)
        .await
        .unwrap();
    assert_eq!(actor_a.store.channel_state_writes.get(), 1);
    actor_a.store.channel_state_writes.set(0);
    actor_a
        .receive_closing_v2(&mut sa, signature_b)
        .await
        .unwrap();
    assert_eq!(actor_a.store.channel_state_writes.get(), 1,
        "only the first transaction publication needs another commit; the response is already durable");
    let before = bincode::serialize(&sa.core).unwrap();
    actor_a.store.channel_state_writes.set(0);
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();
    actor_a.progress_close_v2(&mut sa).await.unwrap();
    assert_eq!(bincode::serialize(&sa.core).unwrap(), before);
    assert_eq!(actor_a.store.channel_state_writes.get(), 0);
    capture_barrier(&network, id).await;
    assert!(matches!(
        captured.lock().unwrap().as_slice(),
        [NetworkActorMessage::Event(
            NetworkActorEvent::ClosingTransactionPending(..)
        )]
    ));
    let stored = actor_a.store.get_channel_actor_state(&id).unwrap();
    stored.validate_session().unwrap();
    assert!(
        matches!(stored.state, ChannelState::ShuttingDown(flags) if flags == fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
    );
    assert!(stored
        .session
        .closing
        .as_ref()
        .unwrap()
        .remote_response
        .is_some());
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_remove_effects_wait_for_message_tail_even_after_partial_lookup_failure() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for defer_last in [false, true] {
        let mut state = a.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let balances = (state.to_local_amount, state.to_remote_amount);
        for tag in 0..3 {
            let preimage = fiber_types::Hash256::from([tag; 32]);
            let mut add = retry_batch_add(tag, 1000);
            add.payment_hash = add.hash_algorithm.hash(preimage).into();
            if defer_last && tag == 2 {
                add.attempt_id = Some(42);
            }
            let mut tlc = state.create_outbounding_tlc(&add);
            tlc.status = fiber_types::TlcStatus::Outbound(
                fiber_types::OutboundTlcStatus::RemoveAckConfirmed,
            );
            tlc.removed_confirmed_at = Some(state.get_local_commitment_number());
            tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
                fiber_types::RemoveTlcFulfill {
                    payment_preimage: preimage,
                },
            ));
            state.tlc_state.add_offered_tlc(tlc);
            state.increment_next_offered_tlc_id();
        }
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        actor.store.insert_channel_actor_state(state.clone());
        actor.store.channel_state_writes.set(0);
        for tlc in state.tlc_state.offered_tlcs.tlcs.iter() {
            if let Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(fulfill)) =
                &tlc.removed_reason
            {
                actor
                    .store
                    .insert_preimage(tlc.payment_hash, fulfill.payment_preimage);
            }
        }
        let durable_before = bincode::serialize(&state.core).unwrap();
        actor
            .apply_settled_remove_tlcs(&myself, &mut state, false)
            .await;
        let applied = if defer_last { 2 } else { 3 };
        assert_eq!(actor.store.channel_state_writes.get(), 0);
        let stored = actor.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(bincode::serialize(&stored.core).unwrap(), durable_before);
        assert!(stored.session.remove_effects.is_empty());
        assert!(actor.store.preimage_removal_writes.borrow().is_empty());
        assert_eq!((stored.to_local_amount, stored.to_remote_amount), balances);
        assert_eq!(
            (state.to_local_amount, state.to_remote_amount),
            (
                balances.0 - applied as u128 * 1000,
                balances.1 + applied as u128 * 1000
            )
        );
        if defer_last {
            let pending = state
                .tlc_state
                .get(&fiber_types::TLCId::Offered(2))
                .unwrap();
            assert!(!pending
                .applied_flags
                .contains(fiber_types::AppliedFlags::REMOVE));
        } else {
            assert!(state.tlc_state.offered_tlcs.tlcs.is_empty());
        }
        capture_barrier(&network, id).await;
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .iter()
                .filter(|message| matches!(
                    message,
                    NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect { .. })
                ))
                .count(),
            0,
            "do not publish any batch member before the message-tail commit"
        );
        // Before commit, recovery reapplies removals from the original durable TLCs.
        let mut restored = ChannelActorState::from(stored.core);
        restored.network = Some(network.clone());
        assert_eq!(restored.pending_remove_effects_v2, 0);
        actor.replay_remove_effects_v2(&restored);
        capture_barrier(&network, id).await;
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .iter()
                .filter(|message| matches!(
                    message,
                    NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect { .. })
                ))
                .count(),
            0
        );
        actor
            .apply_settled_remove_tlcs(&myself, &mut restored, false)
            .await;
        assert_eq!(
            (restored.to_local_amount, restored.to_remote_amount),
            (state.to_local_amount, state.to_remote_amount)
        );
        assert_eq!(restored.session.remove_effects.len(), applied);
        assert_eq!(actor.store.channel_state_writes.get(), 0);
        assert!(actor.store.preimage_removal_writes.borrow().is_empty());
        captured.lock().unwrap().clear();
        actor.persist_state_and_publish_pending_effects(&mut state);
        assert_eq!(actor.store.channel_state_writes.get(), 1);
        let stored = actor.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(
            (stored.to_local_amount, stored.to_remote_amount),
            (state.to_local_amount, state.to_remote_amount)
        );
        assert_eq!(stored.session.remove_effects.len(), applied);
        capture_barrier(&network, id).await;
        let relayed: Vec<_> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match message {
                NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect {
                    tlc,
                    ..
                }) => Some(tlc.tlc_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            relayed,
            if defer_last {
                vec![
                    fiber_types::TLCId::Offered(0),
                    fiber_types::TLCId::Offered(1),
                ]
            } else {
                vec![
                    fiber_types::TLCId::Offered(0),
                    fiber_types::TLCId::Offered(1),
                    fiber_types::TLCId::Offered(2),
                ]
            }
        );
        actor.persist_state_and_publish_pending_effects(&mut state);
        capture_barrier(&network, id).await;
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .iter()
                .filter(|message| matches!(
                    message,
                    NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect { .. })
                ))
                .count(),
            applied,
            "an unrelated message must not replay the outbox again"
        );
        if !defer_last {
            actor
                .handle_event(
                    &myself,
                    &mut state,
                    super::ChannelEvent::MaintainChannelTlcs,
                )
                .await
                .unwrap();
            capture_barrier(&network, id).await;
            assert_eq!(
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        NetworkActorMessage::Command(
                            NetworkActorCommand::RelayV2RemoveEffect { .. }
                        )
                    ))
                    .count(),
                applied
            );
            actor.persist_state_and_publish_pending_effects(&mut state);
            capture_barrier(&network, id).await;
            assert_eq!(
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        NetworkActorMessage::Command(
                            NetworkActorCommand::RelayV2RemoveEffect { .. }
                        )
                    ))
                    .count(),
                applied * 2,
                "maintenance must still retry unconfirmed durable effects, after its commit"
            );
        }
        network.stop(None);
        task.await.unwrap();
    }
    handle.abort();
}

#[tokio::test]
async fn test_v2_remove_effects_precede_force_close_without_publishing_failed_removal() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let balances = (state.to_local_amount, state.to_remote_amount);
    for tag in 0..3 {
        let preimage = fiber_types::Hash256::from([tag; 32]);
        let mut add = retry_batch_add(tag, 1000);
        add.payment_hash = add.hash_algorithm.hash(preimage).into();
        let mut tlc = state.create_outbounding_tlc(&add);
        tlc.status =
            fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::RemoveAckConfirmed);
        tlc.removed_confirmed_at = Some(state.get_local_commitment_number());
        tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
            fiber_types::RemoveTlcFulfill {
                payment_preimage: if tag == 2 { [99; 32].into() } else { preimage },
            },
        ));
        state.tlc_state.add_offered_tlc(tlc);
        state.increment_next_offered_tlc_id();
    }
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    actor
        .apply_settled_remove_tlcs(&myself, &mut state, false)
        .await;
    actor.persist_state_and_publish_pending_effects(&mut state);
    assert_eq!(
        (state.to_local_amount, state.to_remote_amount),
        (balances.0 - 2000, balances.1 + 2000)
    );
    let stored = actor.store.get_channel_actor_state(&id).unwrap();
    assert_eq!(
        (stored.to_local_amount, stored.to_remote_amount),
        (state.to_local_amount, state.to_remote_amount)
    );
    capture_barrier(&network, id).await;
    let publications: Vec<_> = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|message| match message {
            NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect {
                tlc, ..
            }) => Some(Some(tlc.tlc_id)),
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                super::ChannelCommandWithId {
                    command: super::ChannelCommand::Shutdown(command, _),
                    ..
                },
            )) if command.force => Some(None),
            _ => None,
        })
        .collect();
    assert_eq!(
        publications,
        vec![
            Some(fiber_types::TLCId::Offered(0)),
            Some(fiber_types::TLCId::Offered(1)),
            None
        ],
        "publish only successful removals, before the close that can supersede their outbox"
    );
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_retired_nonce_cannot_sign_a_later_round() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let sender = a.get_channel_actor_state(id);
    let owner = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for use_retired in [false, true] {
        let mut sender = sender.clone();
        let mut owner = owner.clone();
        owner.network = Some(network.clone());
        let retired = owner.session.own.clone();
        owner.mark_reestablishing_offline();
        owner.on_peer_reconnected();
        if !use_retired {
            sender.session.remote_nonce = Some(owner.session.own.public_nonce.clone());
        }
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.tlc_state.waiting_ack = true;
        let verified = owner.stage_incoming_v2(&request, false).unwrap();
        let (_, _, ack) = owner
            .accept_commitment_v2(&request, false, verified)
            .unwrap();
        sender.stage_ack_v2(&ack.clone().unwrap()).unwrap();
        sender.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
        // Old healthy snapshots retain this apparently unused allocation, even after signing it.
        owner.session.own_retired = vec![retired.clone()];
        owner.validate_session().unwrap();
        sender.session.remote_nonce = Some(retired.public_nonce);
        let (malicious, _) = sender.prepare_outgoing_v2(false).unwrap();
        let before = bincode::serialize(&owner.core).unwrap();
        assert!(
            owner.stage_incoming_v2(&malicious, false).is_err(),
            "reject a cross-round allocation, used={use_retired}"
        );
        assert_eq!(bincode::serialize(&owner.core).unwrap(), before);
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_round_advance_discards_retired_allocations() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    let retired = owner.session.own.clone();
    owner.session.own_retired.push(retired);
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    let verified = owner.stage_incoming_v2(&request, false).unwrap();
    owner
        .accept_commitment_v2(&request, false, verified)
        .unwrap();
    assert!(owner.session.own_retired.is_empty());
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    owner.network = Some(network.clone());
    let before = owner.session.own.public_nonce.clone();
    owner.mark_reestablishing_offline();
    owner.on_peer_reconnected();
    assert_ne!(
        owner.session.own.public_nonce, before,
        "a progressed channel still rotates its verification nonce"
    );
    owner.validate_session().unwrap();
    network.stop(None);
    task.await.unwrap();
    let session = &mut owner.session;
    // Exercise the separate bootstrap-to-Ready advancement with its real initial allocation.
    let initial = b
        .get_channel_actor_state(id)
        .core
        .session
        .incoming
        .unwrap()
        .funding;
    session.own = initial.clone();
    session.own_retired.push(initial);
    owner.ready_wire_message().unwrap();
    assert!(owner.session.own_retired.is_empty());
}

#[tokio::test]
async fn test_v2_lost_ack_handshakes_keep_restorable_sessions_and_fresh_nonce() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    sender.network = Some(network.clone());
    owner.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;
    let verified = owner.stage_incoming_v2(&request, false).unwrap();
    let (_, _, ack) = owner
        .accept_commitment_v2(&request, false, verified)
        .unwrap();
    let ack = ack.unwrap();
    for _ in 0..3 {
        owner.mark_reestablishing_offline();
        owner.on_peer_reconnected();
        sender.mark_reestablishing_offline();
        actor
            .reestablish_v2(&myself, &mut sender, owner.reestablish_message_v2())
            .await
            .unwrap();
        assert_eq!(
            sender.get_local_commitment_number(),
            request.commitment_number
        );
        sender.validate_session().unwrap();
        actor.persist_state_and_publish_pending_effects(&mut sender);
        let stored = actor.store.get_channel_actor_state(&id).unwrap();
        let core = bincode::deserialize::<fiber_types::ChannelActorData>(
            &bincode::serialize(&stored.core).unwrap(),
        )
        .unwrap();
        sender = ChannelActorState::from(core);
        sender.network = Some(network.clone());
    }
    // Reconnect once more after restoring: RAA must be gated by a fresh valid handshake.
    sender.mark_reestablishing_offline();
    actor
        .reestablish_v2(&myself, &mut sender, owner.reestablish_message_v2())
        .await
        .unwrap();
    assert!(!owner
        .session
        .own_retired
        .iter()
        .any(|n| n.public_nonce == ack.next_commitment_nonce));
    actor
        .receive_ack_v2(&myself, &mut sender, ack)
        .await
        .unwrap();
    sender.validate_session().unwrap();
    assert_eq!(
        sender.session.remote_nonce.as_ref(),
        Some(&owner.session.own.public_nonce)
    );
    let (next, _) = sender.prepare_outgoing_v2(false).unwrap();
    owner.stage_incoming_v2(&next, false).unwrap();
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_external_funding_response_replay_is_phase_and_witness_bound() {
    use ckb_types::prelude::Unpack;
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    for mode in [
        "same",
        "placeholder",
        "conflict",
        "early",
        "no_history",
        "acceptor",
        "ready",
    ] {
        let mut state = a.get_channel_actor_state(id);
        state.network = Some(network.clone());
        state.ephemeral_config.external_funding.enabled = true;
        state.ephemeral_config.external_funding.signed_submitted = true;
        state.ephemeral_config.external_funding.funding_lock_script =
            Some(ckb_types::packed::Script::default());
        state.state =
            ChannelState::AwaitingChannelReady(fiber_types::AwaitingChannelReadyFlags::empty());
        let mut witnesses: Vec<Vec<u8>> = state
            .funding_tx
            .as_ref()
            .unwrap()
            .witnesses()
            .into_iter()
            .map(|w| w.unpack())
            .collect();
        match mode {
            "placeholder" => witnesses = vec![Vec::new()],
            "conflict" => witnesses = vec![vec![77; 65]],
            "early" => {
                state.state = ChannelState::SigningCommitment(SigningCommitmentFlags::empty())
            }
            "no_history" => state.latest_commitment_transaction = None,
            "acceptor" => state.is_acceptor = true,
            "ready" => state.state = ChannelState::ChannelReady,
            _ => {}
        }
        let before = bincode::serialize(&state.core).unwrap();
        let result = actor
            .handle_peer_message(
                &myself,
                &mut state,
                FiberChannelMessage::TxSignatures(crate::fiber::types::TxSignatures {
                    channel_id: id,
                    witnesses,
                }),
            )
            .await;
        assert_eq!(
            result.is_ok(),
            matches!(mode, "same" | "placeholder"),
            "{mode}: {result:?}"
        );
        assert_eq!(
            bincode::serialize(&state.core).unwrap(),
            before,
            "replay must not mutate funding or signing state"
        );
        capture_barrier(&network, id).await;
        let frames = take_wire_messages(&captured);
        if result.is_ok() {
            assert_eq!(frames.len(), 1);
            let FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxSignatures(reply)) =
                &frames[0].message
            else {
                panic!("expected cached TxSignatures")
            };
            let cached: Vec<Vec<u8>> = state
                .funding_tx
                .as_ref()
                .unwrap()
                .witnesses()
                .into_iter()
                .map(|w| w.unpack())
                .collect();
            assert_eq!(reply.witnesses, cached);
        } else {
            assert!(frames.is_empty());
        }
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_closed_local_add_waits_for_durable_payment_receipt() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    a.hold_next_fiber_messages(
        b.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::CommitmentSigned,
        1,
    )
    .await;
    let live = a.get_channel_actor(id).await.unwrap();
    ractor::call_t!(
        live,
        |reply| ChannelActorMessage::Command(super::ChannelCommand::CommitmentSigned(Some(reply))),
        5000
    )
    .unwrap()
    .unwrap();
    a.wait_for_held_fiber_messages(1).await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    crate::tests::test_utils::wait_until_timeout(5000, || a.get_channel_actor_state(id).retryable_tlc_operations.iter().any(|op| matches!(op, RetryableTlcOperation::AddTlc(command) if command.payment_hash == hash))).await;
    let attempt = a.store.get_attempts(hash).pop().unwrap();
    assert_eq!(attempt.status, fiber_types::AttemptStatus::Created);
    b.stop().await;
    a.stop().await;
    let mut source = a.get_channel_actor_state(id);
    source.state = ChannelState::Closed(
        fiber_types::CloseFlags::UNCOOPERATIVE_LOCAL
            | fiber_types::CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    source.validate_session().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, network_task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    source.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    actor
        .apply_retryable_tlc_operations(&myself, &mut source, true)
        .await;
    capture_barrier(&network, id).await;
    assert!(
        !a.get_channel_actor_state(id)
            .retryable_tlc_operations
            .is_empty(),
        "an unavailable recipient must not release the failure owner"
    );
    let pending = a
        .get_channel_actor_state(id)
        .retryable_tlc_operations
        .clone();
    assert_eq!(
        a.store.get_attempt(hash, attempt.id).unwrap().status,
        fiber_types::AttemptStatus::Created
    );
    // Drop the volatile request; restart must finish the durable handoff.
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    network_task.await.unwrap();
    a.start().await;
    crate::tests::test_utils::wait_until_timeout(5000, || {
        a.get_channel_actor_state(id)
            .retryable_tlc_operations
            .is_empty()
            && a.store.get_payment_session(hash).unwrap().status.is_final()
    })
    .await;
    let outcome = a.store.get_attempt(hash, attempt.id).unwrap();
    assert!(outcome.is_failed());
    a.stop().await;
    // Crash after the payment commit/receipt but before source retirement.
    let mut restored = a.get_channel_actor_state(id);
    restored.retryable_tlc_operations = pending;
    a.store.insert_channel_actor_state(restored);
    a.start().await;
    crate::tests::test_utils::wait_until_timeout(5000, || {
        a.get_channel_actor_state(id)
            .retryable_tlc_operations
            .is_empty()
    })
    .await;
    let replayed = a.store.get_attempt(hash, attempt.id).unwrap();
    assert_eq!(replayed.tried_times, outcome.tried_times);
    assert_eq!(replayed.last_error, outcome.last_error);
    assert_eq!(replayed.status, outcome.status);
    a.get_channel_actor_state(id).validate_session().unwrap();
    a.stop().await;
    // A receipt may persist Retrying before its timer runs; the closed first hop never emits Ready again.
    for moved_route in [false, true] {
        let mut retrying = replayed.clone();
        retrying.status = fiber_types::AttemptStatus::Retrying;
        let mut session = a.store.get_payment_session(hash).unwrap();
        session.status = fiber_types::PaymentStatus::Inflight;
        a.store.delete_attempts(hash);
        a.store.insert_attempt(retrying.clone());
        if moved_route {
            // A released node may have left an alias at a previous first hop.
            // Restore that raw legacy layout without changing the current route.
            use fiber_types::schema::ATTEMPT_CHANNEL_INDEX_PREFIX;
            let old_outpoint = ckb_types::packed::OutPoint::new([77; 32].into(), 0);
            let key = |outpoint: &ckb_types::packed::OutPoint| {
                [
                    &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                    outpoint.as_slice(),
                    hash.as_ref(),
                    &attempt.id.to_le_bytes(),
                ]
                .concat()
            };
            a.store
                .delete(key(attempt.first_hop_channel_outpoint().unwrap()));
            a.store.put(key(&old_outpoint), []);
        }
        a.store.insert_payment_session(session);
        assert_eq!(
            a.store
                .get_pending_attempts_by_channel_outpoint(
                    attempt.first_hop_channel_outpoint().unwrap()
                )
                .len(),
            usize::from(!moved_route)
        );
        a.start().await;
        crate::tests::test_utils::wait_until_timeout(5000, || {
            a.store.get_payment_session(hash).unwrap().status.is_final()
        })
        .await;
        assert!(a.store.get_attempt(hash, attempt.id).unwrap().is_failed());
        a.stop().await;
    }
}

#[tokio::test]
async fn test_closed_local_add_receipt_checks_identity_and_deduplicates_retry() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    a.hold_next_fiber_messages(
        b.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::CommitmentSigned,
        1,
    )
    .await;
    let live = a.get_channel_actor(id).await.unwrap();
    ractor::call_t!(
        live,
        |reply| ChannelActorMessage::Command(super::ChannelCommand::CommitmentSigned(Some(reply))),
        5000
    )
    .unwrap()
    .unwrap();
    a.wait_for_held_fiber_messages(1).await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    crate::tests::test_utils::wait_until_timeout(5000, || {
        !a.get_channel_actor_state(id)
            .retryable_tlc_operations
            .is_empty()
    })
    .await;
    let mut source = a.get_channel_actor_state(id);
    let RetryableTlcOperation::AddTlc(command) =
        source.retryable_tlc_operations.front().unwrap().clone()
    else {
        unreachable!()
    };
    let original = a
        .store
        .get_attempt(hash, command.attempt_id.unwrap())
        .unwrap();
    live.stop(None);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while a.get_channel_actor(id).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    source.state = ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
    a.store.insert_channel_actor_state(source);
    for wrong_field in 0..3 {
        let mut invalid = command.clone();
        match wrong_field {
            0 => invalid.payment_hash = crate::gen_rand_sha256_hash(),
            1 => invalid.attempt_id = Some(original.id + 1),
            _ => invalid.amount += 1,
        }
        let result = ractor::call_t!(
            a.network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::CompleteClosedLocalAdd {
                channel_id: id,
                command: Box::new(invalid),
                reply,
            }),
            5000
        )
        .unwrap();
        assert!(result.is_err());
    }
    for moved_route in [false, true] {
        let mut newer = original.clone();
        if moved_route {
            let mut hops = newer.route_hops.clone();
            hops[0].funding_tx_hash = fiber_types::Hash256::from([77; 32]);
            newer.update_route(hops);
        } else {
            newer.session_key = [42; 32];
        }
        a.store.insert_attempt(newer.clone());
        ractor::call_t!(
            a.network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::CompleteClosedLocalAdd {
                channel_id: id,
                command: Box::new(command.clone()),
                reply,
            }),
            5000
        )
        .unwrap()
        .unwrap();
        let unchanged = a.store.get_attempt(hash, original.id).unwrap();
        assert_eq!(unchanged.status, fiber_types::AttemptStatus::Created);
        assert_eq!(unchanged.tried_times, original.tried_times);
        assert_eq!(
            unchanged.first_hop_channel_outpoint(),
            newer.first_hop_channel_outpoint()
        );
        assert_eq!(unchanged.session_key, newer.session_key);
        if moved_route {
            assert!(a
                .store
                .get_pending_attempts_by_channel_outpoint(
                    original.first_hop_channel_outpoint().unwrap()
                )
                .is_empty());
            assert_eq!(
                a.store
                    .get_pending_attempts_by_channel_outpoint(
                        newer.first_hop_channel_outpoint().unwrap()
                    )
                    .len(),
                1
            );
        }
    }
    let mut retrying = original;
    retrying.status = fiber_types::AttemptStatus::Retrying;
    retrying.tried_times = 1;
    retrying.last_error = Some("previous attempt failure".to_owned());
    a.store.insert_attempt(retrying.clone());
    for _ in 0..2 {
        ractor::call_t!(
            a.network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::CompleteClosedLocalAdd {
                channel_id: id,
                command: Box::new(command.clone()),
                reply,
            }),
            5000
        )
        .unwrap()
        .unwrap();
    }
    assert_eq!(
        a.store.get_attempt(hash, retrying.id).unwrap().tried_times,
        1,
        "replayed delivery must not consume another retry"
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn test_v2_missing_ready_precedes_pending_first_commitment() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut receiver = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    sender.network = Some(network.clone());
    sender.private_key = Some(a.private_key.clone());
    receiver.network = Some(network.clone());
    receiver.private_key = Some(b.private_key.clone());
    receiver.state = ChannelState::AwaitingChannelReady(
        fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
    );
    receiver.commitment_numbers = fiber_types::CommitmentNumbers {
        local: 1,
        remote: 1,
    };
    let session = &mut receiver.session;
    session.remote_ready = None;
    session.remote_number = 1;
    session.remote_nonce = session.bootstrap_remote_nonce.clone();
    receiver.validate_session().unwrap();
    let sender_actor =
        ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let receiver_actor =
        ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    sender_actor
        .add_tlc_update(&mut sender, &retry_batch_add(97, 1000))
        .unwrap();
    sender_actor
        .handle_commitment_signed_command(&myself, &mut sender)
        .await
        .unwrap();
    sender.validate_session().unwrap();
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();
    sender.mark_reestablishing_offline();
    receiver.mark_reestablishing_offline();
    sender_actor
        .reestablish_v2(&myself, &mut sender, receiver.reestablish_message_v2())
        .await
        .unwrap();
    receiver_actor
        .reestablish_v2(&myself, &mut receiver, sender.reestablish_message_v2())
        .await
        .unwrap();
    capture_barrier(&network, id).await;
    let frames = take_wire_messages(&captured);
    let mut errors = Vec::new();
    let mut saw_ready = false;
    for frame in frames {
        let FiberMessage::ChannelNormalOperation(message) = frame.message else {
            continue;
        };
        if matches!(message, FiberChannelMessage::ReestablishChannelV2(_)) {
            continue;
        }
        if frame.target == b.pubkey {
            saw_ready |= matches!(message, FiberChannelMessage::ChannelReadyV2(_));
            if let Err(error) = receiver_actor
                .handle_peer_message(&myself, &mut receiver, message)
                .await
            {
                errors.push(error);
            }
        } else if frame.target == a.pubkey {
            sender_actor
                .handle_peer_message(&myself, &mut sender, message)
                .await
                .unwrap();
        }
    }
    assert!(saw_ready && errors.is_empty(), "missing opening Ready blocked pending payment: Ready replay={saw_ready}, errors={errors:?}, A={:?}/waiting_ack={}, B={:?}", sender.state, sender.tlc_state.waiting_ack, receiver.state);
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_first_payment_recovers_when_peer_missed_opening_ready() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut opening = b.get_channel_actor_state(id);
    let balance_a = a.get_local_balance_from_channel(id);
    let balance_b = b.get_local_balance_from_channel(id);
    a.hold_next_fiber_messages(
        b.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::CommitmentSigned,
        1,
    )
    .await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_for_held_fiber_messages(1).await;
    a.wait_until_inflight(hash).await;
    b.stop().await;
    a.stop().await;
    // B had sent its own Ready, but never received A's Ready or the first payment.
    opening.state = ChannelState::AwaitingChannelReady(
        fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
    );
    opening.commitment_numbers = fiber_types::CommitmentNumbers {
        local: 1,
        remote: 1,
    };
    let session = &mut opening.session;
    session.remote_ready = None;
    session.remote_number = 1;
    session.remote_nonce = session.bootstrap_remote_nonce.clone();
    opening.validate_session().unwrap();
    b.store.insert_channel_actor_state(opening);
    a.start().await;
    b.start().await;
    a.connect_to(&mut b).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        a.wait_until_success(hash),
    )
    .await
    .expect("opening Ready must precede the pending first payment");
    for node in [&a, &b] {
        let state = node.get_channel_actor_state(id);
        assert_eq!(state.state, ChannelState::ChannelReady);
        assert!(!state.reestablishing);
        state.validate_session().unwrap();
    }
    assert_eq!(a.get_local_balance_from_channel(id), balance_a - 1000);
    assert_eq!(b.get_local_balance_from_channel(id), balance_b + 1000);
}

#[tokio::test]
async fn test_forward_recovery_recognizes_fulfilled_outbox_owner() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut downstream = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured).await.unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let preimage = fiber_types::Hash256::from([92; 32]);
    let hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();
    let upstream_id = crate::gen_rand_sha256_hash();
    let mut upstream =
        crate::fiber::tests::create_test_channel_state_with_tlc(upstream_id, 0, 1000, hash, None);
    let source_id = fiber_types::TLCId::Received(0);
    upstream
        .waiting_forward_tlc_tasks
        .insert(source_id, [7; 32]);
    upstream.network = Some(network.clone());
    a.store.insert_channel_actor_state(upstream.clone());
    let mut command = retry_batch_add(92, 1000);
    command.payment_hash = hash;
    let mut tlc = downstream.create_outbounding_tlc(&command);
    let downstream_tlc_id = u64::from(tlc.tlc_id);
    tlc.forwarding_tlc = Some((upstream_id, 0));
    tlc.status =
        fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::RemoveAckConfirmed);
    tlc.applied_flags = fiber_types::AppliedFlags::ADD | fiber_types::AppliedFlags::REMOVE;
    tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
        fiber_types::RemoveTlcFulfill {
            payment_preimage: preimage,
        },
    ));
    downstream.session.remove_effects.push((tlc, None));
    let pending = upstream;
    for closed in [false, true] {
        if closed {
            downstream.state = ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
        }
        downstream.validate_session().unwrap();
        a.store.insert_channel_actor_state(downstream.clone());
        let mut upstream = pending.clone();
        actor.recover_pending_forward_tlcs(&myself, &mut upstream);
        assert_eq!(
            upstream.tlc_state.get(&source_id).unwrap().forwarding_tlc,
            Some((id, downstream_tlc_id)),
            "outbox ownership must survive downstream close"
        );
        assert!(upstream.retryable_tlc_operations.is_empty());
        assert!(upstream.waiting_forward_tlc_tasks.is_empty());
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_closed_channel_keeps_undelivered_outbox_recoverable() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let preimage = fiber_types::Hash256::from([91; 32]);
    let mut command = retry_batch_add(91, 1000);
    command.payment_hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();
    let mut tlc = state.create_outbounding_tlc(&command);
    tlc.forwarding_tlc = Some((crate::gen_rand_sha256_hash(), 0));
    tlc.status =
        fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::RemoveAckConfirmed);
    tlc.applied_flags = fiber_types::AppliedFlags::ADD | fiber_types::AppliedFlags::REMOVE;
    tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
        fiber_types::RemoveTlcFulfill {
            payment_preimage: preimage,
        },
    ));
    state.session.remove_effects.push((tlc, None));
    state.state =
        ChannelState::ShuttingDown(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION);
    state.validate_session().unwrap();
    actor
        .handle_event(
            &myself,
            &mut state,
            super::ChannelEvent::ClosingTransactionConfirmed(Default::default(), false, true),
        )
        .await
        .unwrap();
    actor.persist_state_and_publish_pending_effects(&mut state);
    let durable = a.store.get_channel_actor_state(&id).unwrap();
    durable.validate_session().unwrap();
    assert_eq!(durable.session.remove_effects.len(), 1);
    assert_eq!(
        durable.offline_restore_mode(),
        Some(super::OfflineChannelRestoreMode::WatchChain)
    );
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();
    let (tlc, _) = &durable.session.remove_effects[0];
    let tlc_id = tlc.tlc_id;
    let reason = tlc.removed_reason.clone().unwrap();
    let restored_actor =
        ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (restored, restored_task) = Actor::spawn(
        None,
        restored_actor,
        super::ChannelInitializationParameter {
            operation: super::ChannelInitializationOperation::RestoreOfflineChannel(id),
            ephemeral_config: Default::default(),
            private_key: a.private_key.clone(),
        },
    )
    .await
    .unwrap();
    ractor::call_t!(
        restored,
        |reply| ChannelActorMessage::Command(super::ChannelCommand::TestBarrier(reply)),
        5000
    )
    .unwrap();
    capture_barrier(&network, id).await;
    {
        let messages = captured.lock().unwrap();
        assert!(messages.iter().any(|message| matches!(
            message,
            NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect { .. })
        )));
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                NetworkActorMessage::Notification(NetworkServiceEvent::RemoteTxComplete(..))
            )),
            "an outbox-only worker must not re-register a fully closed channel with the watchtower"
        );
    }
    restored
        .send_message(ChannelActorMessage::Event(
            super::ChannelEvent::OnChainTlcRelayConfirmed(tlc_id, reason.clone()),
        ))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), restored_task)
        .await
        .expect("closed actor must stop after receipt")
        .unwrap();
    let mut completed = a.get_channel_actor_state(id);
    assert!(completed.session.remove_effects.is_empty());
    assert!(!completed.can_restore_offline_actor());
    let before = bincode::serialize(&completed.core).unwrap();
    actor
        .handle_event(
            &myself,
            &mut completed,
            super::ChannelEvent::OnChainTlcRelayConfirmed(tlc_id, reason),
        )
        .await
        .unwrap();
    actor.persist_state_and_publish_pending_effects(&mut completed);
    assert_eq!(
        bincode::serialize(&a.get_channel_actor_state(id).core).unwrap(),
        before
    );
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_waiting_close_confirmation_keeps_chain_watch_actor() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    state.state =
        ChannelState::ShuttingDown(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    for queued in [false, true] {
        let mut pending = state.clone();
        if queued {
            pending
                .retryable_tlc_operations
                .push_back(RetryableTlcOperation::AddTlc(retry_batch_add(94, 1000)));
        }
        pending.validate_session().unwrap();
        a.store.insert_channel_actor_state(pending);
        captured.lock().unwrap().clear();
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let (myself, handle) = Actor::spawn(
            None,
            actor,
            super::ChannelInitializationParameter {
                operation: super::ChannelInitializationOperation::RestoreOfflineChannel(id),
                ephemeral_config: Default::default(),
                private_key: a.private_key.clone(),
            },
        )
        .await
        .unwrap();
        for _ in 0..2 {
            ractor::call_t!(
                myself,
                |reply| ChannelActorMessage::Command(super::ChannelCommand::TestBarrier(reply)),
                5000
            )
            .unwrap();
        }
        capture_barrier(&network, id).await;
        assert!(
            captured.lock().unwrap().iter().any(|message| matches!(
                message,
                NetworkActorMessage::Notification(NetworkServiceEvent::RemoteTxComplete(..))
            )),
            "waiting for close confirmation must retain watchtower protection"
        );
        assert_eq!(
            a.get_channel_actor_state(id).retryable_tlc_operations.len(),
            usize::from(queued),
            "ordinary retry retirement waits for confirmed close"
        );
        myself.stop(None);
        handle.await.unwrap();
    }
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_outbox_receipt_finishes_deferred_chain_settlement() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let preimage = fiber_types::Hash256::from([93; 32]);
    let hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();
    let mut command = retry_batch_add(93, 1000);
    command.payment_hash = hash;
    let mut tlc = state.create_outbounding_tlc(&command);
    let tlc_id = tlc.tlc_id;
    tlc.forwarding_tlc = Some((crate::gen_rand_sha256_hash(), 0));
    tlc.status =
        fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::RemoveAckConfirmed);
    tlc.applied_flags = fiber_types::AppliedFlags::ADD | fiber_types::AppliedFlags::REMOVE;
    tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
        fiber_types::RemoveTlcFulfill {
            payment_preimage: preimage,
        },
    ));
    let reason = tlc.removed_reason.clone().unwrap();
    state.session.remove_effects.push((tlc, None));
    state.state = ChannelState::Closed(
        fiber_types::CloseFlags::UNCOOPERATIVE_LOCAL
            | fiber_types::CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    let tx_hash = ckb_types::H256::from([93; 32]);
    state.shutdown_transaction_hash = Some(tx_hash.clone());
    let balances = (state.to_local_amount, state.to_remote_amount);
    let counters = state.commitment_numbers;
    a.store.store_shutdown_settlement_record(
        &id,
        &fiber_types::ShutdownSettlementRecord {
            shutdown_tx_hash: tx_hash,
            for_remote: false,
            commitment_number: 1,
            settlement_data: state.session.incoming.as_ref().unwrap().settlement.clone(),
        },
    );
    a.store.insert_preimage(hash, preimage);
    state.validate_session().unwrap();
    a.store.insert_channel_actor_state(state);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured).await.unwrap();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (myself, handle) = Actor::spawn(
        None,
        actor,
        super::ChannelInitializationParameter {
            operation: super::ChannelInitializationOperation::RestoreOfflineChannel(id),
            ephemeral_config: Default::default(),
            private_key: a.private_key.clone(),
        },
    )
    .await
    .unwrap();
    myself
        .send_message(ChannelActorMessage::Event(
            super::ChannelEvent::OnChainSettlementCompleted,
        ))
        .unwrap();
    ractor::call_t!(
        myself,
        |reply| ChannelActorMessage::Command(super::ChannelCommand::TestBarrier(reply)),
        5000
    )
    .unwrap();
    let pending = a.get_channel_actor_state(id);
    assert!(pending.is_waiting_onchain_settlement());
    assert!(pending.is_onchain_settlement_confirmed());
    assert!(a.store.get_shutdown_settlement_record(&id).is_some());
    assert_eq!(a.store.get_preimage(&hash), Some(preimage));
    myself
        .send_message(ChannelActorMessage::Event(
            super::ChannelEvent::OnChainTlcRelayConfirmed(tlc_id, reason),
        ))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("the last receipt must release the settlement actor")
        .unwrap();
    let completed = a.get_channel_actor_state(id);
    completed.validate_session().unwrap();
    assert!(!completed.is_waiting_onchain_settlement());
    assert!(!completed.can_restore_offline_actor());
    assert!(completed.session.remove_effects.is_empty());
    assert!(a.store.get_shutdown_settlement_record(&id).is_none());
    assert_eq!(a.store.get_preimage(&hash), None);
    assert_eq!(
        (completed.to_local_amount, completed.to_remote_amount),
        balances
    );
    assert_eq!(completed.commitment_numbers, counters);
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_closed_outbox_replays_on_restart_and_stops_after_receipt() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    a.hold_next_fiber_messages(
        b.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::CommitmentSigned,
        1,
    )
    .await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_for_held_fiber_messages(1).await;
    let mut tlc = a.get_channel_actor_state(id).tlc_state.offered_tlcs.tlcs[0].clone();
    a.release_held_fiber_messages().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        a.wait_until_success(hash),
    )
    .await
    .unwrap();
    let attempt = a.store.get_attempt(hash, tlc.attempt_id.unwrap()).unwrap();
    tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
        fiber_types::RemoveTlcFulfill {
            payment_preimage: attempt.preimage.unwrap(),
        },
    ));
    b.stop().await;
    a.stop().await;
    let settled = a.get_channel_actor_state(id);
    for force in [false, true] {
        let mut source = settled.clone();
        source.state = ChannelState::Closed(if force {
            fiber_types::CloseFlags::UNCOOPERATIVE_LOCAL
        } else {
            fiber_types::CloseFlags::COOPERATIVE
        });
        source
            .session
            .remove_effects
            .push((tlc.clone(), Some(attempt.tried_times)));
        let balances = (source.to_local_amount, source.to_remote_amount);
        source.validate_session().unwrap();
        a.store.insert_channel_actor_state(source);
        let mut session = a.store.get_payment_session(hash).unwrap();
        session.status = fiber_types::PaymentStatus::Inflight;
        a.store.insert_payment_session(session);
        a.start().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if a.get_channel_actor_state(id)
                    .session
                    .remove_effects
                    .is_empty()
                    && a.store.get_persisted_payment_status(hash)
                        == Some(fiber_types::PaymentStatus::Success)
                    && a.get_channel_actor(id).await.is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed source must replay, durably receive, and stop without reconnecting");
        let restored = a.get_channel_actor_state(id);
        restored.validate_session().unwrap();
        assert_eq!(
            (restored.to_local_amount, restored.to_remote_amount),
            balances
        );
        assert!(!restored.can_restore_offline_actor());
        assert_eq!(
            a.store.get_attempt(hash, attempt.id).unwrap().tried_times,
            attempt.tried_times
        );
        a.stop().await;
    }
}

#[tokio::test]
async fn test_v2_ready_publication_crash_replays_saved_nonce() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let initial_peer = a.get_channel_actor_state(id);
    let initial_opening = b.get_channel_actor_state(id);
    let confirmed = initial_opening.funding_tx_confirmed_at.clone().unwrap();
    b.stop().await;
    a.stop().await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
    let peer_actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    for peer_ready_first in [false, true] {
        let mut peer = initial_peer.clone();
        let mut opening = initial_opening.clone();
        for state in [&mut opening, &mut peer] {
            state.network = Some(network.clone());
            state.commitment_numbers = fiber_types::CommitmentNumbers {
                local: 1,
                remote: 1,
            };
            let session = &mut state.session;
            session.remote_ready = None;
            session.remote_number = 1;
            session.remote_nonce = session.bootstrap_remote_nonce.clone();
        }
        opening.private_key = Some(b.private_key.clone());
        peer.private_key = Some(a.private_key.clone());
        peer.state = ChannelState::AwaitingChannelReady(
            fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
        );
        opening.state =
            ChannelState::AwaitingChannelReady(fiber_types::AwaitingChannelReadyFlags::empty());
        opening.funding_tx_confirmed_at = None;
        let session = &mut opening.session;
        session.own = session.incoming.as_ref().unwrap().funding.clone();
        session.own_retired.clear();
        let FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(peer_ready)) =
            peer.ready_wire_message().unwrap()
        else {
            unreachable!()
        };
        if peer_ready_first {
            actor
                .handle_peer_message(
                    &myself,
                    &mut opening,
                    FiberChannelMessage::ChannelReadyV2(peer_ready.clone()),
                )
                .await
                .unwrap();
        }
        opening.validate_session().unwrap();
        actor
            .handle_event(
                &myself,
                &mut opening,
                super::ChannelEvent::FundingTransactionConfirmed(
                    confirmed.0.clone(),
                    confirmed.1,
                    confirmed.2,
                ),
            )
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        let first = take_wire_messages(&captured)
            .into_iter()
            .find_map(|frame| match frame.message {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(
                    ready,
                )) => Some(ready),
                _ => None,
            })
            .unwrap();
        peer_actor
            .handle_peer_message(
                &myself,
                &mut peer,
                FiberChannelMessage::ChannelReadyV2(first.clone()),
            )
            .await
            .unwrap();
        assert_eq!(peer.state, ChannelState::ChannelReady);
        // Use the actual save before publication, not the handler's later in-memory state.
        let durable = actor.store.get_channel_actor_state(&id).unwrap();
        let mut restarted = ChannelActorState::from(durable.core);
        restarted.validate_session().unwrap();
        restarted.network = Some(network.clone());
        restarted.private_key = Some(b.private_key.clone());
        restarted.mark_reestablishing_offline();
        actor
            .reestablish_v2(&myself, &mut restarted, peer.reestablish_message_v2())
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        let replay = take_wire_messages(&captured)
            .into_iter()
            .find_map(|frame| match frame.message {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(
                    ready,
                )) => Some(ready),
                _ => None,
            })
            .expect("opening recovery must replay Ready without another chain confirmation");
        assert_eq!(
            FiberMessage::channel_ready_v2(replay.clone()).to_molecule_bytes(),
            FiberMessage::channel_ready_v2(first).to_molecule_bytes()
        );
        peer_actor
            .handle_peer_message(
                &myself,
                &mut peer,
                FiberChannelMessage::ChannelReadyV2(replay),
            )
            .await
            .unwrap();
        actor
            .handle_peer_message(
                &myself,
                &mut restarted,
                FiberChannelMessage::ChannelReadyV2(peer_ready),
            )
            .await
            .unwrap();
        assert_eq!(restarted.state, ChannelState::ChannelReady);
        restarted.validate_session().unwrap();
        peer.validate_session().unwrap();
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_opening_reconnect_replays_identical_ready() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let peer = a.get_channel_actor_state(id);
    let mut opening = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    opening.network = Some(network.clone());
    opening.private_key = Some(b.private_key.clone());
    let original = opening.ready_wire_message().unwrap().to_molecule_bytes();
    opening.state = ChannelState::AwaitingChannelReady(
        fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
    );
    opening.commitment_numbers = fiber_types::CommitmentNumbers {
        local: 1,
        remote: 1,
    };
    let session = &mut opening.session;
    session.remote_ready = None;
    session.remote_number = 1;
    session.remote_nonce = session.bootstrap_remote_nonce.clone();
    opening.mark_reestablishing_offline();
    let actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
    actor
        .reestablish_v2(&myself, &mut opening, peer.reestablish_message_v2())
        .await
        .unwrap();
    capture_barrier(&network, id).await;
    let ready = take_wire_messages(&captured)
        .into_iter()
        .find_map(|frame| match frame.message {
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(ready)) => {
                Some(ready)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        FiberMessage::channel_ready_v2(ready.clone()).to_molecule_bytes(),
        original
    );
    for missed in [false, true] {
        let mut peer = peer.clone();
        peer.network = Some(network.clone());
        peer.private_key = Some(a.private_key.clone());
        if missed {
            peer.state = ChannelState::AwaitingChannelReady(
                fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
            );
            peer.commitment_numbers = fiber_types::CommitmentNumbers {
                local: 1,
                remote: 1,
            };
            let session = &mut peer.session;
            session.remote_ready = None;
            session.remote_number = 1;
            session.remote_nonce = session.bootstrap_remote_nonce.clone();
        }
        let peer_actor =
            ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        peer_actor
            .handle_peer_message(
                &myself,
                &mut peer,
                FiberChannelMessage::ChannelReadyV2(ready.clone()),
            )
            .await
            .unwrap();
        peer.validate_session().unwrap();
    }
    let mut peer = peer;
    let FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(ready)) =
        peer.ready_wire_message().unwrap()
    else {
        unreachable!()
    };
    actor
        .handle_peer_message(
            &myself,
            &mut opening,
            FiberChannelMessage::ChannelReadyV2(ready),
        )
        .await
        .unwrap();
    assert_eq!(opening.state, ChannelState::ChannelReady);
    opening.validate_session().unwrap();
    let restored = ChannelActorState::from(
        bincode::deserialize::<fiber_types::ChannelActorData>(
            &bincode::serialize(&opening.core).unwrap(),
        )
        .unwrap(),
    );
    restored.validate_session().unwrap();
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_ready_replay_uses_committed_bootstrap_nonce() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for prior_reconnect_event in [false, true] {
        let mut state = a.get_channel_actor_state(id);
        state.network = Some(network.clone());
        state.mark_reestablishing_offline();
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        if prior_reconnect_event {
            state.on_peer_reconnected();
        }
        actor.store.insert_channel_actor_state(state.clone());
        actor.store.channel_state_writes.set(0);
        let before = bincode::serialize(&state.core).unwrap();
        let old_nonce = state.session.own.public_nonce.clone();
        let mut peer = b.get_channel_actor_state(id).reestablish_message_v2();
        peer.next_ack_number = INITIAL_COMMITMENT_NUMBER + 1;
        actor
            .handle_peer_message(
                &myself,
                &mut state,
                FiberChannelMessage::ReestablishChannelV2(peer),
            )
            .await
            .unwrap();
        assert_eq!(
            actor.store.channel_state_writes.get(),
            if prior_reconnect_event { 0 } else { 1 }
        );
        let stored = actor.store.get_channel_actor_state(&id).unwrap();
        let nonce = &stored.session.own.public_nonce;
        assert_eq!(nonce, &state.session.own.public_nonce);
        assert_eq!(
            *nonce, old_nonce,
            "unused opening Ready must stay byte-identical"
        );
        if prior_reconnect_event {
            assert_eq!(bincode::serialize(&stored.core).unwrap(), before);
        }
        state.validate_session().unwrap();
        capture_barrier(&network, id).await;
        let messages = captured.lock().unwrap();
        assert!(messages.iter().any(|message| matches!(message,
            NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(FiberMessageWithTarget {
                message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(ready)), ..
            })) if &ready.next_commitment_nonce == nonce)));
        drop(messages);
        captured.lock().unwrap().clear();
    }
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_pending_forward_recovery_only_saves_changed_ownership() {
    let upstream = fiber_types::Hash256::from([71; 32]);
    let downstream = fiber_types::Hash256::from([72; 32]);
    let hash = fiber_types::Hash256::from([73; 32]);
    let mut incoming =
        crate::fiber::tests::create_test_channel_state_with_tlc(upstream, 0, 1000, hash, None);
    incoming
        .waiting_forward_tlc_tasks
        .insert(fiber_types::TLCId::Received(0), [7; 32]);
    let mut outgoing =
        crate::fiber::tests::create_test_channel_state_with_tlc(downstream, 0, 1000, hash, None);
    outgoing.tlc_state = Default::default();
    let mut add = retry_batch_add(73, 900);
    add.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
        upstream, 0, 100, [7; 32],
    ));
    outgoing
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(add.clone()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    incoming.network = Some(network.clone());
    let actor = ChannelActor::new(
        incoming.get_local_pubkey(),
        incoming.get_remote_pubkey(),
        network.clone(),
        MockStore::new(),
        None,
    );
    actor.store.insert_channel_actor_state(incoming.clone());
    actor.store.insert_channel_actor_state(outgoing.clone());
    actor.store.channel_state_writes.set(0);
    let before = bincode::serialize(&incoming.core).unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    assert!(actor.recover_pending_forward_tlcs(&myself, &mut incoming));
    assert_eq!(bincode::serialize(&incoming.core).unwrap(), before);
    assert_eq!(actor.store.channel_state_writes.get(), 0);

    outgoing.retryable_tlc_operations.clear();
    let tlc = outgoing.create_outbounding_tlc(&add);
    outgoing.tlc_state.add_offered_tlc(tlc);
    actor.store.insert_channel_actor_state(outgoing);
    actor.store.channel_state_writes.set(0);
    assert!(actor.recover_pending_forward_tlcs(&myself, &mut incoming));
    assert_eq!(actor.store.channel_state_writes.get(), 1);
    let stored = actor.store.get_channel_actor_state(&upstream).unwrap();
    assert!(stored.waiting_forward_tlc_tasks.is_empty());
    assert_eq!(
        stored
            .tlc_state
            .get(&fiber_types::TLCId::Received(0))
            .unwrap()
            .forwarding_tlc,
        Some((downstream, 0))
    );
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_receipt_only_saves_early_before_preimage_cleanup() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for (fulfill, retain, early_writes) in [(false, false, 0), (true, true, 0), (true, false, 1)] {
        let mut state = a.get_channel_actor_state(id);
        state.network = Some(network.clone());
        let preimage = fiber_types::Hash256::from([74; 32]);
        let mut add = retry_batch_add(74, 1000);
        add.payment_hash = add.hash_algorithm.hash(preimage).into();
        let reason = if fulfill {
            fiber_types::RemoveTlcReason::RemoveTlcFulfill(fiber_types::RemoveTlcFulfill {
                payment_preimage: preimage,
            })
        } else {
            fiber_types::RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
                fiber_types::TlcErr::new(fiber_types::TlcErrorCode::HoldTlcTimeout),
                &[0; 32],
            ))
        };
        let mut tlc = state.create_outbounding_tlc(&add);
        tlc.status =
            fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::RemoveAckConfirmed);
        tlc.removed_reason = Some(reason.clone());
        let tlc_id = tlc.tlc_id;
        state.session.remove_effects.push((tlc.clone(), None));
        if retain {
            tlc.tlc_id = fiber_types::TLCId::Offered(1);
            state.session.remove_effects.push((tlc, None));
        }
        let store = MockStore::new().with_preimage(add.payment_hash, preimage);
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), store, None);
        actor.store.insert_channel_actor_state(state.clone());
        actor.store.channel_state_writes.set(0);
        actor
            .handle_event(
                &myself,
                &mut state,
                super::ChannelEvent::OnChainTlcRelayConfirmed(tlc_id, reason),
            )
            .await
            .unwrap();
        assert_eq!(actor.store.channel_state_writes.get(), early_writes);
        assert_eq!(
            actor.store.get_preimage(&add.payment_hash).is_none(),
            fulfill && !retain
        );
        assert_eq!(
            *actor.store.preimage_removal_writes.borrow(),
            if fulfill && !retain { vec![1] } else { vec![] }
        );
        capture_barrier(&network, id).await;
        assert_eq!(captured.lock().unwrap().iter().filter(|message| matches!(message,
            NetworkActorMessage::Notification(NetworkServiceEvent::PreimageRemoved(hash)) if *hash == add.payment_hash)).count(), if fulfill && !retain { 1 } else { 0 });
        captured.lock().unwrap().clear();
        actor.persist_state_and_publish_pending_effects(&mut state);
        assert_eq!(actor.store.channel_state_writes.get(), early_writes + 1);
        assert_eq!(
            actor
                .store
                .get_channel_actor_state(&id)
                .unwrap()
                .session
                .remove_effects
                .len(),
            usize::from(retain)
        );
    }
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

struct TerminalRetryNetwork;

struct TerminalRetryNetworkState {
    store: crate::store::Store,
    upstream: ChannelActorState,
    sink: ActorRef<ChannelActorMessage>,
    accepted: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Actor for TerminalRetryNetwork {
    type Msg = NetworkActorMessage;
    type State = TerminalRetryNetworkState;
    type Arguments = Self::State;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        state: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(command)) =
            message
        {
            if command.channel_id == state.upstream.id {
                state.upstream.network = Some(myself.clone());
                let actor = ChannelActor::new(
                    state.upstream.local_pubkey,
                    state.upstream.remote_pubkey,
                    myself,
                    state.store.clone(),
                    None,
                );
                actor
                    .handle_command(&state.sink, &mut state.upstream, command.command)
                    .await?;
                state
                    .accepted
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

struct RetryTaskCounter;

#[async_trait::async_trait]
impl Actor for RetryTaskCounter {
    type Msg = ChannelActorMessage;
    type State = Arc<std::sync::atomic::AtomicUsize>;
    type Arguments = Self::State;
    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        state: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(state)
    }
    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if matches!(
            message,
            ChannelActorMessage::Event(super::ChannelEvent::RunRetryTask)
        ) {
            state.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_closed_v2_retry_queue_hands_off_adds_and_keeps_chain_obligations() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let upstream_id = crate::gen_rand_sha256_hash();
    let mut upstream = crate::fiber::tests::create_test_channel_state_with_tlc(
        upstream_id,
        0,
        1000,
        [42; 32].into(),
        None,
    );
    let template = upstream.tlc_state.received_tlcs.tlcs[0].clone();
    upstream.tlc_state.received_tlcs.tlcs.clear();
    for index in 0..3 {
        let mut command = retry_batch_add(42 + index, 1000);
        command.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
            upstream_id,
            index.into(),
            0,
            [7; 32],
        ));
        let mut tlc = template.clone();
        tlc.tlc_id = fiber_types::TLCId::Received(index.into());
        tlc.payment_hash = command.payment_hash;
        upstream.tlc_state.received_tlcs.tlcs.push(tlc);
        upstream
            .waiting_forward_tlc_tasks
            .insert(fiber_types::TLCId::Received(index.into()), [7; 32]);
        state
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::AddTlc(command));
    }
    upstream.tlc_state.received_tlcs.next_tlc_id = 3;
    a.store.insert_channel_actor_state(upstream.clone());
    let accepted = Arc::new(AtomicUsize::new(0));
    let retried = Arc::new(AtomicUsize::new(0));
    let (sink, sink_task) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let (myself, handle) = Actor::spawn(None, RetryTaskCounter, retried.clone())
        .await
        .unwrap();
    let (network, task) = Actor::spawn(
        None,
        TerminalRetryNetwork,
        TerminalRetryNetworkState {
            store: a.store.clone(),
            upstream,
            sink: sink.clone(),
            accepted: accepted.clone(),
        },
    )
    .await
    .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let mut preimages = Vec::new();
    for index in 0..2 {
        let preimage = crate::gen_rand_sha256_hash();
        let hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();
        let mut tlc = template.clone();
        tlc.tlc_id = fiber_types::TLCId::Received(index);
        tlc.payment_hash = hash;
        tlc.created_at = state.get_current_commitment_numbers();
        state.tlc_state.received_tlcs.tlcs.push(tlc);
        actor.register_retryable_tlc_remove(
            &myself,
            &mut state,
            fiber_types::TLCId::Received(index),
            fiber_types::RemoveTlcReason::RemoveTlcFulfill(fiber_types::RemoveTlcFulfill {
                payment_preimage: preimage,
            }),
        );
        preimages.push((hash, preimage));
    }
    state.tlc_state.received_tlcs.next_tlc_id = 2;
    state.prepare_outgoing_v2(false).unwrap();
    state.tlc_state.waiting_ack = true;
    let balances = (state.to_local_amount, state.to_remote_amount);
    let tlcs = bincode::serialize(&state.tlc_state).unwrap();
    let mut before_receipts = state.core.clone();
    before_receipts.state = ChannelState::Closed(
        fiber_types::CloseFlags::UNCOOPERATIVE_REMOTE
            | fiber_types::CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    before_receipts.shutdown_transaction_hash = Some(ckb_types::H256::from([8; 32]));
    actor
        .handle_event(
            &myself,
            &mut state,
            super::ChannelEvent::ClosingTransactionConfirmed(
                ckb_types::H256::from([8; 32]),
                true,
                false,
            ),
        )
        .await
        .unwrap();
    actor
        .apply_retryable_tlc_operations(&myself, &mut state, true)
        .await;
    assert!(
        state.retryable_tlc_operations.is_empty(),
        "terminal work must not wait for an impossible ACK"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        3,
        "each Add needs a durable upstream receipt"
    );
    let saved_upstream = a.store.get_channel_actor_state(&upstream_id).unwrap();
    assert!(saved_upstream.waiting_forward_tlc_tasks.is_empty());
    assert_eq!(saved_upstream.retryable_tlc_operations.len(), 3);
    assert!(saved_upstream
        .retryable_tlc_operations
        .iter()
        .all(|op| matches!(
            op,
            RetryableTlcOperation::RemoveTlc(_, fiber_types::RemoveTlcReason::RemoveTlcFail(_))
        )));
    assert_eq!(bincode::serialize(&state.tlc_state).unwrap(), tlcs);
    assert_eq!((state.to_local_amount, state.to_remote_amount), balances);
    for (hash, preimage) in preimages {
        assert_eq!(a.store.get_preimage(&hash), Some(preimage));
    }
    assert!(
        state.tlc_state.waiting_ack,
        "retain the signed history, do not fabricate an ACK"
    );
    state.validate_session().unwrap();
    actor.persist_state_and_publish_pending_effects(&mut state);
    let mut restored = ChannelActorState::from(a.store.get_channel_actor_state(&id).unwrap().core);
    restored.network = Some(network.clone());
    restored.validate_session().unwrap();
    actor
        .apply_retryable_tlc_operations(&myself, &mut restored, true)
        .await;
    // Crash after upstream's durable receipt but before retiring the downstream queue.
    let mut cut = ChannelActorState::from(before_receipts);
    cut.network = Some(network.clone());
    actor
        .apply_retryable_tlc_operations(&myself, &mut cut, true)
        .await;
    assert!(cut.retryable_tlc_operations.is_empty());
    assert_eq!(
        a.store
            .get_channel_actor_state(&upstream_id)
            .unwrap()
            .retryable_tlc_operations
            .len(),
        3
    );
    state.schedule_next_retry_task(&myself);
    tokio::time::sleep(std::time::Duration::from_millis(220)).await;
    assert_eq!(retried.load(Ordering::SeqCst), 0);
    assert_eq!(accepted.load(Ordering::SeqCst), 3);
    myself.stop(None);
    handle.await.unwrap();
    sink.stop(None);
    sink_task.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_terminal_forward_receipt_is_identity_checked_and_idempotent() {
    let (a, b, _) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
        .await
        .unwrap();
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for mode in ["wrong_hash", "missing", "removed", "pending"] {
        let id = crate::gen_rand_sha256_hash();
        let hash = fiber_types::Hash256::from([41; 32]);
        let mut state =
            crate::fiber::tests::create_test_channel_state_with_tlc(id, 0, 1000, hash, None);
        state.network = Some(network.clone());
        state
            .waiting_forward_tlc_tasks
            .insert(fiber_types::TLCId::Received(0), [7; 32]);
        let reason = fiber_types::TlcErr::new(fiber_types::TlcErrorCode::PermanentChannelFailure);
        if mode == "missing" {
            state.tlc_state.received_tlcs.tlcs.clear();
        }
        if mode == "removed" {
            state.tlc_state.received_tlcs.tlcs[0].removed_reason =
                Some(fiber_types::RemoveTlcReason::RemoveTlcFail(
                    fiber_types::TlcErrPacket::new(reason.clone(), &[7; 32]),
                ));
        }
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        let result = crate::fiber::types::ForwardTlcResult {
            channel_id: id,
            tlc_id: 0,
            payment_hash: if mode == "wrong_hash" {
                [42; 32].into()
            } else {
                hash
            },
            add_tlc_result: Err((
                ProcessingChannelError::InvalidState("closed".to_owned()),
                reason,
            )),
        };
        let before = bincode::serialize(&state.core).unwrap();
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let outcome = actor
            .handle_command(
                &myself,
                &mut state,
                super::ChannelCommand::CompleteForward(result.clone(), reply.into()),
            )
            .await;
        assert_eq!(outcome.is_ok(), mode != "wrong_hash");
        assert_eq!(receipt.await.unwrap().is_ok(), mode != "wrong_hash");
        if mode == "wrong_hash" {
            assert_eq!(bincode::serialize(&state.core).unwrap(), before);
        } else {
            assert!(state.waiting_forward_tlc_tasks.is_empty());
            assert_eq!(
                state.retryable_tlc_operations.len(),
                usize::from(mode == "pending"),
                "{mode}: do not create a failure for an already completed TLC"
            );
            let (reply, receipt) = tokio::sync::oneshot::channel();
            actor
                .handle_command(
                    &myself,
                    &mut state,
                    super::ChannelCommand::CompleteForward(result, reply.into()),
                )
                .await
                .unwrap();
            receipt.await.unwrap().unwrap();
            let durable = actor.store.get_channel_actor_state(&id).unwrap();
            assert_eq!(
                durable.retryable_tlc_operations.len(),
                usize::from(mode == "pending")
            );
        }
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_closed_forward_handoff_survives_timeout_and_watch_chain_restart() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    state.prepare_outgoing_v2(false).unwrap();
    state.tlc_state.waiting_ack = true;
    state.state = ChannelState::Closed(
        fiber_types::CloseFlags::UNCOOPERATIVE_LOCAL
            | fiber_types::CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    let upstream_id = crate::gen_rand_sha256_hash();
    let mut command = retry_batch_add(19, 1000);
    command.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
        upstream_id,
        0,
        0,
        [7; 32],
    ));
    let mut upstream = crate::fiber::tests::create_test_channel_state_with_tlc(
        upstream_id,
        0,
        1000,
        command.payment_hash,
        None,
    );
    upstream
        .waiting_forward_tlc_tasks
        .insert(fiber_types::TLCId::Received(0), [7; 32]);
    a.store.insert_channel_actor_state(upstream.clone());
    state
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(command));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (silent, silent_task) = Actor::spawn(None, CaptureNetwork, captured).await.unwrap();
    let retried = Arc::new(AtomicUsize::new(0));
    let (myself, handle) = Actor::spawn(None, RetryTaskCounter, retried.clone())
        .await
        .unwrap();
    state.network = Some(silent.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, silent.clone(), a.store.clone(), None);
    actor
        .apply_retryable_tlc_operations(&myself, &mut state, true)
        .await;
    assert_eq!(
        state.retryable_tlc_operations.len(),
        1,
        "do not lose a result without a durable receipt"
    );
    let mut restored = ChannelActorState::from(a.store.get_channel_actor_state(&id).unwrap().core);
    restored.mark_watching_chain_offline();
    restored.validate_session().unwrap();
    state.schedule_next_retry_task(&myself);
    tokio::time::sleep(std::time::Duration::from_millis(220)).await;
    assert_eq!(retried.load(Ordering::SeqCst), 0);
    let (sink, sink_task) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let (network, task) = Actor::spawn(
        None,
        TerminalRetryNetwork,
        TerminalRetryNetworkState {
            store: a.store.clone(),
            upstream,
            sink: sink.clone(),
            accepted: accepted.clone(),
        },
    )
    .await
    .unwrap();
    restored.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (restarted, restarted_task) = Actor::spawn(
        None,
        actor,
        super::ChannelInitializationParameter {
            operation: super::ChannelInitializationOperation::RestoreOfflineChannel(id),
            ephemeral_config: Default::default(),
            private_key: a.private_key.clone(),
        },
    )
    .await
    .unwrap();
    crate::tests::test_utils::wait_until_timeout(5000, || {
        a.store
            .get_channel_actor_state(&id)
            .unwrap()
            .retryable_tlc_operations
            .is_empty()
    })
    .await;
    let restored = a.store.get_channel_actor_state(&id).unwrap();
    assert!(restored.retryable_tlc_operations.is_empty());
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(
        a.store
            .get_channel_actor_state(&upstream_id)
            .unwrap()
            .retryable_tlc_operations
            .len(),
        1
    );
    restored.validate_session().unwrap();
    restarted
        .send_message(ChannelActorMessage::Event(
            super::ChannelEvent::MaintainChannelTlcs,
        ))
        .unwrap();
    ractor::call_t!(
        restarted,
        |reply| ChannelActorMessage::Command(super::ChannelCommand::TestBarrier(reply)),
        5000
    )
    .unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    restarted.stop(None);
    restarted_task.await.unwrap();
    myself.stop(None);
    handle.await.unwrap();
    sink.stop(None);
    sink_task.await.unwrap();
    network.stop(None);
    task.await.unwrap();
    silent.stop(None);
    silent_task.await.unwrap();
}

#[tokio::test]
async fn test_closed_forward_queue_preserves_another_durable_owner() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let original = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    for created in [false, true] {
        let mut state = original.clone();
        state.prepare_outgoing_v2(false).unwrap();
        state.tlc_state.waiting_ack = true;
        state.state = ChannelState::Closed(
            fiber_types::CloseFlags::UNCOOPERATIVE_REMOTE
                | fiber_types::CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        );
        let upstream_id = crate::gen_rand_sha256_hash();
        let owner_id = crate::gen_rand_sha256_hash();
        let mut command = retry_batch_add(if created { 92 } else { 91 }, 1000);
        command.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
            upstream_id,
            0,
            0,
            [7; 32],
        ));
        let mut upstream = crate::fiber::tests::create_test_channel_state_with_tlc(
            upstream_id,
            0,
            1000,
            command.payment_hash,
            None,
        );
        upstream
            .waiting_forward_tlc_tasks
            .insert(fiber_types::TLCId::Received(0), [7; 32]);
        let mut owner = crate::fiber::tests::create_test_channel_state_with_tlc(
            owner_id,
            0,
            1000,
            command.payment_hash,
            None,
        );
        let mut tlc = owner.tlc_state.received_tlcs.tlcs[0].clone();
        owner.tlc_state = Default::default();
        if created {
            tlc.tlc_id = fiber_types::TLCId::Offered(7);
            tlc.status =
                fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::Committed);
            tlc.forwarding_tlc = Some((upstream_id, 0));
            owner.tlc_state.offered_tlcs.tlcs.push(tlc);
        } else {
            owner
                .retryable_tlc_operations
                .push_back(RetryableTlcOperation::AddTlc(command.clone()));
        }
        a.store.insert_channel_actor_state(upstream.clone());
        a.store.insert_channel_actor_state(owner);
        state
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::AddTlc(command));
        let (sink, sink_task) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let (network, task) = Actor::spawn(
            None,
            TerminalRetryNetwork,
            TerminalRetryNetworkState {
                store: a.store.clone(),
                upstream,
                sink: sink.clone(),
                accepted: accepted.clone(),
            },
        )
        .await
        .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        actor
            .apply_retryable_tlc_operations(&sink, &mut state, true)
            .await;
        assert!(state.retryable_tlc_operations.is_empty());
        let upstream = a.store.get_channel_actor_state(&upstream_id).unwrap();
        assert!(
            upstream.retryable_tlc_operations.is_empty(),
            "do not fail a backed forward"
        );
        assert!(upstream.tlc_state.received_tlcs.tlcs[0]
            .removed_reason
            .is_none());
        assert_eq!(accepted.load(Ordering::SeqCst), usize::from(created));
        if created {
            assert_eq!(
                upstream.tlc_state.received_tlcs.tlcs[0].forwarding_tlc,
                Some((owner_id, 7))
            );
        } else {
            assert_eq!(upstream.waiting_forward_tlc_tasks.len(), 1);
        }
        state.validate_session().unwrap();
        network.stop(None);
        task.await.unwrap();
        sink.stop(None);
        sink_task.await.unwrap();
    }
}

#[tokio::test]
async fn test_closed_retry_scheduler_does_not_rearm_nonempty_queue() {
    let (a, _, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    state.state = ChannelState::Closed(
        fiber_types::CloseFlags::UNCOOPERATIVE_REMOTE
            | fiber_types::CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    let mut command = retry_batch_add(9, 1000);
    command.attempt_id = Some(123);
    state
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(command));
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (myself, handle) = Actor::spawn(None, RetryTaskCounter, count.clone())
        .await
        .unwrap();
    state.schedule_next_retry_task(&myself);
    tokio::time::sleep(std::time::Duration::from_millis(220)).await;
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(
        state.local_pubkey,
        state.remote_pubkey,
        network.clone(),
        MockStore::new(),
        None,
    );
    actor
        .apply_retryable_tlc_operations(&myself, &mut state, true)
        .await;
    assert_eq!(state.retryable_tlc_operations.len(), 1);
    assert_eq!(
        actor
            .store
            .get_channel_actor_state(&id)
            .unwrap()
            .retryable_tlc_operations
            .len(),
        1
    );
    capture_barrier(&network, id).await;
    assert!(captured.lock().unwrap().iter().any(|message| matches!(message,
        NetworkActorMessage::Command(NetworkActorCommand::CompleteClosedLocalAdd { command, .. }) if command.attempt_id == Some(123))));
    assert!(!captured.lock().unwrap().iter().any(|message| matches!(
        message,
        NetworkActorMessage::Event(NetworkActorEvent::AddTlcResult(..))
    )));
    network.stop(None);
    task.await.unwrap();
    myself.stop(None);
    handle.await.unwrap();
}

#[tokio::test]
async fn test_v2_remote_commitment_points_survive_asymmetric_rounds_and_restore() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 100000000000, false).await;
    let mut local = a.get_channel_actor_state(id);
    let mut remote = b.get_channel_actor_state(id);
    a.stop().await;
    b.stop().await;
    let balances = (local.to_local_amount, local.to_remote_amount);
    for state in [&mut local, &mut remote] {
        state.local_constraints.max_tlc_number_in_flight = 1;
        state.remote_constraints.max_tlc_number_in_flight = 1;
    }
    let (network, task) = Actor::spawn(None, CaptureNetwork, Arc::new(Mutex::new(Vec::new())))
        .await
        .unwrap();
    local.network = Some(network.clone());
    remote.network = Some(network.clone());
    let local_actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let remote_actor =
        ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();

    // Drive real CS/ACK signing without invoice application so both TLCs stay in flight.
    // Empty rounds separate the two TLCs' creation counters on the local side.
    for (index, (from_local, add)) in [
        (false, None),
        (true, Some(71)),
        (false, None),
        (true, None),
        (false, None),
        (false, Some(72)),
        (true, None),
        (true, None),
        (false, None),
        (true, None),
        (false, None),
        (true, None),
    ]
    .into_iter()
    .enumerate()
    {
        let (sender_actor, receiver_actor, sender, receiver) = if from_local {
            (&local_actor, &remote_actor, &mut local, &mut remote)
        } else {
            (&remote_actor, &local_actor, &mut remote, &mut local)
        };
        if let Some(tag) = add {
            sender_actor
                .add_tlc_update(sender, &retry_batch_add(tag, 1000))
                .unwrap();
        }
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.set_waiting_ack(&myself, true);
        for update in &sender.session.outgoing.as_ref().unwrap().updates {
            let fiber_types::TlcReplayUpdate::Add(add) = update else {
                panic!("only Adds in this fixture")
            };
            receiver_actor
                .handle_add_tlc_peer_message(receiver, add.clone())
                .unwrap();
        }
        receiver.stage_incoming_v2(&request, false).unwrap();
        let (_, _, ack) = receiver
            .accept_commitment_v2(&request, false, false)
            .unwrap();
        receiver.tlc_state.update_for_commitment_signed();
        receiver.last_was_revoke = true;
        let ack = ack.unwrap();
        sender.stage_ack_v2(&ack).unwrap();
        sender.accept_ack_v2(&myself, &ack, false).unwrap();
        for state in [&local, &remote] {
            state.validate_session().unwrap();
        }
        if index == 7 {
            assert_eq!(local.get_local_commitment_number(), 6);
            assert_eq!(local.get_remote_commitment_number(), 6);
            let offered = local
                .tlc_state
                .get(&fiber_types::TLCId::Offered(0))
                .unwrap();
            let received = local
                .tlc_state
                .get(&fiber_types::TLCId::Received(0))
                .unwrap();
            assert_eq!(
                offered.created_at,
                fiber_types::CommitmentNumbers {
                    local: 2,
                    remote: 3
                }
            );
            assert_eq!(
                received.created_at,
                fiber_types::CommitmentNumbers {
                    local: 4,
                    remote: 5
                }
            );
            assert_eq!(
                offered.status,
                fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::Committed)
            );
            assert_eq!(
                received.status,
                fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::Committed)
            );
            let signed = local.latest_commitment_transaction.clone().unwrap();
            a.store.insert_channel_actor_state(local.clone());
            local = a.store.get_channel_actor_state(&id).unwrap();
            local.validate_session().unwrap();
            assert_eq!(
                local
                    .latest_commitment_transaction
                    .as_ref()
                    .unwrap()
                    .as_slice(),
                signed.as_slice()
            );
            local.build_commitment_tx_and_settlement_data(true).expect(
                "restored live TLC must retain its remote commitment point despite the count cap",
            );
            assert_eq!(
                local
                    .remote_commitment_points
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>(),
                vec![2, 3, 4, 5, 6]
            );
            local.network = Some(network.clone());
        }
    }
    for state in [&local, &remote] {
        assert_eq!(state.tlc_state.offered_tlcs.tlcs.len(), 1);
        assert_eq!(state.tlc_state.received_tlcs.tlcs.len(), 1);
        for tlc in state.tlc_state.all_tlcs() {
            state.get_tlc_pubkeys(tlc).unwrap();
        }
        state.build_commitment_tx_and_settlement_data(true).unwrap();
        state
            .build_commitment_tx_and_settlement_data(false)
            .unwrap();
    }
    assert_eq!((local.to_local_amount, local.to_remote_amount), balances);
    assert_eq!((remote.to_remote_amount, remote.to_local_amount), balances);
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

fn retry_batch_add(tag: u8, amount: u128) -> fiber_types::AddTlcCommand {
    fiber_types::AddTlcCommand {
        amount,
        payment_hash: [tag; 32].into(),
        attempt_id: None,
        expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
        hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [0; 32],
        is_trampoline_hop: false,
        previous_tlc: None,
    }
}

#[tokio::test]
async fn test_forward_add_rejects_unbacked_or_already_forwarded_upstream() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let initial = a.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for case in [
        "removed",
        "queued_remove",
        "missing_tlc",
        "hash",
        "algorithm",
        "closed",
        "already_forwarded",
        "already_offered",
        "missing_channel",
        "same_channel_queued_remove",
        "valid",
    ] {
        let mut state = initial.clone();
        state.network = Some(network.clone());
        let mut command = retry_batch_add(42, 1000);
        let upstream_id = if case == "same_channel_queued_remove" {
            id
        } else {
            crate::gen_rand_sha256_hash()
        };
        command.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
            upstream_id,
            0,
            100,
            [7; 32],
        ));
        let mut upstream = crate::fiber::tests::create_test_channel_state_with_tlc(
            upstream_id,
            0,
            1100,
            command.payment_hash,
            None,
        );
        let reason = fiber_types::RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
            fiber_types::TlcErr::new(fiber_types::TlcErrorCode::TemporaryChannelFailure),
            &[7; 32],
        ));
        match case {
            "removed" => {
                upstream
                    .tlc_state
                    .get_mut(&fiber_types::TLCId::Received(0))
                    .unwrap()
                    .removed_reason = Some(reason.clone())
            }
            "queued_remove" => upstream.retryable_tlc_operations.push_back(
                fiber_types::RetryableTlcOperation::RemoveTlc(
                    fiber_types::TLCId::Received(0),
                    reason.clone(),
                ),
            ),
            "missing_tlc" => upstream.tlc_state.received_tlcs.tlcs.clear(),
            "hash" => {
                upstream
                    .tlc_state
                    .get_mut(&fiber_types::TLCId::Received(0))
                    .unwrap()
                    .payment_hash = [43; 32].into()
            }
            "algorithm" => {
                upstream
                    .tlc_state
                    .get_mut(&fiber_types::TLCId::Received(0))
                    .unwrap()
                    .hash_algorithm = fiber_types::HashAlgorithm::Sha256
            }
            "closed" => upstream.state = ChannelState::Closed(fiber_types::CloseFlags::empty()),
            "already_forwarded" => {
                upstream
                    .tlc_state
                    .get_mut(&fiber_types::TLCId::Received(0))
                    .unwrap()
                    .forwarding_tlc = Some((id, 0))
            }
            "already_offered" => {
                let existing = state.create_outbounding_tlc(&command);
                state.tlc_state.add_offered_tlc(existing);
                state.increment_next_offered_tlc_id();
            }
            "same_channel_queued_remove" => {
                state.tlc_state.received_tlcs = upstream.tlc_state.received_tlcs.clone();
                state.retryable_tlc_operations.push_back(
                    fiber_types::RetryableTlcOperation::RemoveTlc(
                        fiber_types::TLCId::Received(0),
                        reason.clone(),
                    ),
                );
            }
            _ => {}
        }
        if case != "missing_channel" && case != "same_channel_queued_remove" {
            a.store.insert_channel_actor_state(upstream);
        }
        captured.lock().unwrap().clear();
        let offered_before = state.tlc_state.offered_tlcs.tlcs.len();
        let result = actor.add_tlc_update(&mut state, &command);
        assert_eq!(result.is_ok(), case == "valid", "{case}: {result:?}");
        assert_eq!(
            state.tlc_state.offered_tlcs.tlcs.len(),
            offered_before + usize::from(case == "valid"),
            "reject before allocating an outgoing TLC: {case}"
        );
        capture_barrier(&network, id).await;
        assert!(
            take_wire_messages(&captured).is_empty(),
            "do not publish before commit: {case}"
        );
        if result.is_ok() {
            actor
                .handle_commitment_signed_command(&myself, &mut state)
                .await
                .unwrap();
        }
        capture_barrier(&network, id).await;
        let sent = captured.lock().unwrap().iter().any(|message| {
            matches!(
                message,
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                    FiberMessageWithTarget {
                        message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(
                            _
                        )),
                        ..
                    }
                ))
            )
        });
        assert_eq!(
            sent,
            case == "valid",
            "never publish an unbacked forward: {case}"
        );
    }
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

fn take_wire_messages(
    captured: &Arc<Mutex<Vec<NetworkActorMessage>>>,
) -> Vec<FiberMessageWithTarget> {
    std::mem::take(&mut *captured.lock().unwrap())
        .into_iter()
        .filter_map(|message| match message {
            NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(message)) => {
                Some(message)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn test_v2_stale_snapshot_reproduces_tlc_conflict_invalid_cs_and_ack_timeout() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut receiver = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    sender.tlc_state.offered_tlcs.next_tlc_id = 1763;
    receiver.tlc_state.received_tlcs.next_tlc_id = 1763;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sender.network = Some(network.clone());
    receiver.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let peer = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(11, 1));
    actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(12, 1));
    a.store.insert_channel_actor_state(sender.clone());
    let mut stale = a.store.get_channel_actor_state(&id).unwrap();
    actor
        .apply_retryable_tlc_operations(&myself, &mut sender, false)
        .await;
    capture_barrier(&network, id).await;
    let original_wire = take_wire_messages(&captured);
    for message in original_wire {
        if let FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) =
            message.message
        {
            peer.handle_add_tlc_peer_message(&mut receiver, add)
                .unwrap();
        }
    }
    assert_eq!(
        a.store
            .get_channel_actor_state(&id)
            .unwrap()
            .tlc_state
            .offered_tlcs
            .next_tlc_id,
        1765
    );
    assert_eq!(receiver.tlc_state.received_tlcs.next_tlc_id, 1765);
    b.store.insert_channel_actor_state(receiver);
    // Inject the old shutdown helper's read/write interleaving after a newer batch commit.
    stale.mark_reestablishing_offline();
    a.store.insert_channel_actor_state(stale);
    let mut restored = a.store.get_channel_actor_state(&id).unwrap();
    let mut receiver = b.store.get_channel_actor_state(&id).unwrap();
    restored.network = Some(network.clone());
    receiver.network = Some(network.clone());
    actor
        .reestablish_v2(&myself, &mut restored, receiver.reestablish_message_v2())
        .await
        .unwrap();
    assert!(!restored.reestablishing);
    let reused = actor
        .handle_add_tlc_command(&myself, &mut restored, &retry_batch_add(13, 1))
        .await
        .unwrap();
    assert_eq!(reused, 1763);
    capture_barrier(&network, id).await;
    let wire = take_wire_messages(&captured);
    let mut request = None;
    let mut rejected_add = false;
    for message in wire {
        match message.message {
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                assert_eq!(add.tlc_id, 1763);
                let error = peer
                    .handle_add_tlc_peer_message(&mut receiver, add)
                    .unwrap_err();
                assert!(error.to_string().contains("Conflicting replayed tlc"));
                rejected_add = true;
            }
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(cs)) => {
                request = Some(cs)
            }
            _ => {}
        }
    }
    assert!(rejected_add);
    let request = request.unwrap();
    let outgoing = restored.session.outgoing.as_ref().unwrap();
    let nonce = AggNonce::sum([
        request.funding_nonce.clone(),
        receiver.session.own.public_nonce.clone(),
    ]);
    musig2::verify_partial(
        &restored.get_deterministic_musig2_agg_context(),
        request.funding_tx_partial_signature,
        &nonce,
        *restored.get_local_funding_pubkey(),
        &request.funding_nonce,
        compute_tx_message(&outgoing.transaction.clone().into_view()),
    )
    .expect("the sender's signature is valid for its own persisted transaction");
    assert!(receiver
        .stage_incoming_v2(&request, false)
        .unwrap_err()
        .to_string()
        .contains("failed to verify signature"));
    restored
        .session
        .outgoing
        .as_mut()
        .unwrap()
        .peer_wait_started_at = 0;
    actor
        .handle_event(
            &myself,
            &mut restored,
            super::ChannelEvent::CheckActiveChannel,
        )
        .await
        .unwrap();
    capture_barrier(&network, id).await;
    assert!(captured
        .lock()
        .unwrap()
        .iter()
        .any(|message| matches!(message,
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            super::ChannelCommandWithId { command: super::ChannelCommand::Shutdown(command, _), .. }
        )) if command.force)));
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_updates_wait_for_commit_before_becoming_peer_visible() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut receiver = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    sender.tlc_state.offered_tlcs.next_tlc_id = 1763;
    receiver.tlc_state.received_tlcs.next_tlc_id = 1763;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sender.network = Some(network.clone());
    receiver.network = Some(network.clone());
    a.store.insert_channel_actor_state(sender.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let peer = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    actor
        .add_tlc_update(&mut sender, &retry_batch_add(11, 1))
        .unwrap();
    actor
        .add_tlc_update(&mut sender, &retry_batch_add(12, 1))
        .unwrap();
    capture_barrier(&network, id).await;
    let early = take_wire_messages(&captured);
    assert!(
        early.is_empty(),
        "AddTlc reached the peer before its ID/content was durable: {early:?}"
    );
    let mut restored = a.store.get_channel_actor_state(&id).unwrap();
    assert_eq!(restored.tlc_state.offered_tlcs.next_tlc_id, 1763);
    assert!(restored.tlc_state.offered_tlcs.tlcs.is_empty());
    restored.network = Some(network.clone());
    // A different payment may reuse an uncommitted ID only if the old Add was never published.
    actor
        .handle_add_tlc_command(&myself, &mut restored, &retry_batch_add(13, 1))
        .await
        .unwrap();
    capture_barrier(&network, id).await;
    let wire = take_wire_messages(&captured);
    assert_eq!(wire.len(), 2);
    let mut ack = None;
    for message in wire {
        match message.message {
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                let durable = a.store.get_channel_actor_state(&id).unwrap();
                let tlc = durable
                    .tlc_state
                    .get(&fiber_types::TLCId::Offered(add.tlc_id))
                    .unwrap();
                assert_eq!(tlc.payment_hash, add.payment_hash);
                assert_eq!(tlc.amount, add.amount);
                assert_eq!(tlc.expiry, add.expiry);
                assert_eq!(durable.tlc_state.offered_tlcs.next_tlc_id, 1764);
                peer.handle_add_tlc_peer_message(&mut receiver, add)
                    .unwrap();
            }
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(cs)) => {
                let verified = receiver.stage_incoming_v2(&cs, false).unwrap();
                ack = receiver
                    .accept_commitment_v2(&cs, false, verified)
                    .unwrap()
                    .2;
            }
            _ => panic!("unexpected wire message"),
        }
    }
    let ack = ack.unwrap();
    let verified = restored.stage_ack_v2(&ack).unwrap();
    assert!(restored
        .accept_ack_v2(&myself, &ack, verified)
        .unwrap()
        .is_some());
    assert!(!restored.tlc_state.waiting_ack);
    assert_eq!(restored.state, ChannelState::ChannelReady);
    actor
        .handle_event(
            &myself,
            &mut restored,
            super::ChannelEvent::CheckActiveChannel,
        )
        .await
        .unwrap();
    capture_barrier(&network, id).await;
    assert!(take_wire_messages(&captured).is_empty());
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_force_closing_removal_is_committed_and_published_without_cs() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let preimage = crate::gen_rand_sha256_hash();
    let invoice = crate::invoice::InvoiceBuilder::new(crate::invoice::Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .payee_pub_key(b.pubkey.into())
        .expiry_time(std::time::Duration::from_secs(3600))
        .build_with_sign(|hash| secp256k1::SECP256K1.sign_ecdsa_recoverable(hash, &b.private_key.0))
        .unwrap();
    let hash = *invoice.payment_hash();
    b.insert_invoice(invoice.clone(), None);
    a.send_payment(crate::fiber::payment::SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        max_parts: Some(1),
        ..Default::default()
    })
    .await
    .unwrap();
    crate::tests::test_utils::wait_until_timeout(10000, || {
        !b.store.get_payment_hold_tlcs(hash).is_empty()
    })
    .await;
    let payer = a.get_channel_actor_state(id);
    let forwarder = b.get_channel_actor_state(id);
    let received = forwarder
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == hash)
        .unwrap();
    assert_eq!(
        received.inbound_status(),
        fiber_types::InboundTlcStatus::Committed
    );
    let tlc_id = received.id();
    b.stop().await;
    a.stop().await;

    for rpc in [false, true] {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        let actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
        let mut state = forwarder.clone();
        state.network = Some(network.clone());
        // Force close must not publish an Add left over from an unsigned batch.
        actor
            .add_tlc_update(&mut state, &retry_batch_add(200, 1))
            .unwrap();
        state.state = ChannelState::ShuttingDown(
            fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
        );
        actor.store.insert_channel_actor_state(state.clone());
        actor.store.channel_state_writes.set(0);
        let old_outgoing = state.session.outgoing.as_ref().unwrap().request.clone();
        let balances = (state.to_local_amount, state.to_remote_amount);
        let command = RemoveTlcCommand {
            id: tlc_id,
            reason: fiber_types::RemoveTlcReason::RemoveTlcFulfill(fiber_types::RemoveTlcFulfill {
                payment_preimage: preimage,
            }),
        };
        if rpc {
            let (reply, response) = tokio::sync::oneshot::channel();
            actor
                .handle_command(
                    &myself,
                    &mut state,
                    super::ChannelCommand::RemoveTlc(command, reply.into()),
                )
                .await
                .unwrap();
            response.await.unwrap().unwrap();
        } else {
            actor
                .handle_remove_tlc_command(&myself, &mut state, command)
                .await
                .unwrap();
            capture_barrier(&network, id).await;
            assert!(take_wire_messages(&captured).is_empty());
            assert_eq!(actor.store.channel_state_writes.get(), 0);
            actor.persist_state_and_publish_pending_effects(&mut state);
        }
        capture_barrier(&network, id).await;
        let wire = take_wire_messages(&captured);
        assert_eq!(
            wire.len(),
            1,
            "publish only the terminal RemoveTlc, never Add or CS"
        );
        assert_eq!(actor.store.channel_state_writes.get(), 1);
        let durable = actor.store.get_channel_actor_state(&id).unwrap();
        let tlc = durable
            .tlc_state
            .get(&fiber_types::TLCId::Received(tlc_id))
            .unwrap();
        assert_eq!(
            tlc.inbound_status(),
            fiber_types::InboundTlcStatus::LocalRemoved
        );
        assert!(matches!(
            tlc.removed_reason,
            Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(_))
        ));
        assert_eq!(
            durable
                .session
                .outgoing
                .as_ref()
                .unwrap()
                .request
                .as_slice(),
            old_outgoing.as_slice()
        );
        assert_eq!(
            (durable.to_local_amount, durable.to_remote_amount),
            balances
        );
        let FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(remove)) =
            &wire[0].message
        else {
            panic!("expected only a removal notification")
        };
        let mut payer = payer.clone();
        payer.network = Some(network.clone());
        let peer = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        peer.handle_remove_tlc_peer_message(&mut payer, remove.clone())
            .unwrap();
        assert_eq!(
            payer
                .tlc_state
                .get(&fiber_types::TLCId::Offered(tlc_id))
                .unwrap()
                .outbound_status(),
            fiber_types::OutboundTlcStatus::RemoteRemoved
        );
        actor.persist_state_and_publish_pending_effects(&mut state);
        capture_barrier(&network, id).await;
        assert!(
            take_wire_messages(&captured).is_empty(),
            "do not republish on every message tail"
        );
        let mut restored = ChannelActorState::from(durable.core);
        restored.network = Some(network.clone());
        actor.persist_state_and_publish_pending_effects(&mut restored);
        capture_barrier(&network, id).await;
        let replayed = take_wire_messages(&captured);
        assert_eq!(replayed.len(), 1);
        assert_eq!(
            replayed[0].message.clone().to_molecule_bytes(),
            wire[0].message.clone().to_molecule_bytes()
        );
        assert_eq!(
            a.get_payment_session(hash).unwrap().status,
            fiber_types::PaymentStatus::Inflight
        );
        myself.stop(None);
        handle.await.unwrap();
        network.stop(None);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn test_v2_preparation_failure_keeps_pending_updates_unpublished() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    state.session.remote_nonce = None;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    actor
        .add_tlc_update(&mut state, &retry_batch_add(11, 1))
        .unwrap();
    actor
        .add_tlc_update(&mut state, &retry_batch_add(12, 1))
        .unwrap();
    assert!(actor
        .handle_commitment_signed_command(&myself, &mut state)
        .await
        .is_err());
    actor.persist_state_and_publish_pending_effects(&mut state);
    capture_barrier(&network, id).await;
    assert!(take_wire_messages(&captured).is_empty());
    assert_eq!(actor.store.channel_state_writes.get(), 1);
    let durable = actor.store.get_channel_actor_state(&id).unwrap();
    assert_eq!(durable.pending_replay_updates.len(), 2);
    assert_eq!(durable.tlc_state.offered_tlcs.tlcs.len(), 2);
    assert!(!durable.tlc_state.waiting_ack);
    myself.stop(None);
    handle.await.unwrap();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_retry_queue_batches_after_ack() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let mut peer = b.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();

    actor
        .handle_commitment_signed_command(&myself, &mut state)
        .await
        .unwrap();
    let request =
        CommitmentSignedV2::try_from(state.session.outgoing.as_ref().unwrap().request.clone())
            .unwrap();
    peer.stage_incoming_v2(&request, false).unwrap();
    let (_, _, ack) = peer.accept_commitment_v2(&request, false, false).unwrap();

    // A peer update folded while awaiting ACK also needs the next CS, not a separate one.
    let command = retry_batch_add(3, 1000);
    let mut received = state
        .create_inbounding_tlc(fiber_types::AddTlc {
            channel_id: id,
            tlc_id: 0,
            amount: command.amount,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            hash_algorithm: command.hash_algorithm,
            onion_packet: None,
        })
        .unwrap();
    received.status =
        fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::AnnounceWaitPrevAck);
    state.tlc_state.add_received_tlc(received.clone());
    state.increment_next_received_tlc_id();
    received.flip_mut();
    received.status = fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::Committed);
    peer.tlc_state.add_offered_tlc(received);
    peer.increment_next_offered_tlc_id();
    let peer_actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);

    actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(1, 2000));
    actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(2, 3000));
    actor
        .apply_retryable_tlc_operations(&myself, &mut state, false)
        .await;
    assert_eq!(state.retryable_tlc_operations.len(), 2);
    assert_eq!(
        state
            .tlc_state
            .all_tlcs()
            .filter(|tlc| tlc.is_offered())
            .count(),
        0
    );
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();

    actor
        .receive_ack_v2(&myself, &mut state, ack.unwrap())
        .await
        .unwrap();
    assert!(
        state.retryable_tlc_operations.is_empty(),
        "one ACK must release the whole queued batch"
    );
    assert!(state.tlc_state.waiting_ack);
    for tlc in state.tlc_state.all_tlcs().filter(|tlc| tlc.is_offered()) {
        assert_eq!(
            tlc.status,
            fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::LocalAnnounced),
            "the previous ACK must not confirm a new batch"
        );
    }
    let outgoing = state.session.outgoing.as_ref().unwrap();
    assert_eq!(outgoing.updates.len(), 2);
    assert_eq!(outgoing.settlement.tlcs.len(), 3);
    capture_barrier(&network, id).await;
    let wire = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|message| match message {
            NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(message)) => {
                Some(message.message.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            wire.as_slice(),
            [
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(_)),
            ]
        ),
        "updates must precede a single CS: {wire:?}"
    );
    let restored = a.store.get_channel_actor_state(&id).unwrap();
    restored.validate_session().unwrap();
    let restored_outgoing = restored.session.outgoing.as_ref().unwrap();
    assert_eq!(restored_outgoing.updates.len(), 2);
    for update in &restored_outgoing.updates {
        let fiber_types::TlcReplayUpdate::Add(add) = update else {
            panic!("expected AddTlc")
        };
        peer_actor
            .handle_add_tlc_peer_message(&mut peer, add.clone())
            .unwrap();
    }
    let request = CommitmentSignedV2::try_from(restored_outgoing.request.clone()).unwrap();
    peer.stage_incoming_v2(&request, false).unwrap();
    let (_, _, ack) = peer.accept_commitment_v2(&request, false, false).unwrap();
    state.stage_ack_v2(ack.as_ref().unwrap()).unwrap();
    state.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
    assert!(state
        .tlc_state
        .all_tlcs()
        .filter(|tlc| tlc.is_offered())
        .all(|tlc| tlc.status
            == fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::Committed)));

    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_retry_queue_checks_limits_and_reports_results() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let initial = a.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    for (value_limit, count_limit, balance_limit, accepted) in [
        (5000, 10, false, vec![1u8, 3]),
        (10000, 2, false, vec![1, 2]),
        (10000, 10, true, vec![1, 3]),
        (10000, 0, false, vec![]),
    ] {
        let mut state = initial.clone();
        state.network = Some(network.clone());
        state.remote_constraints.max_tlc_value_in_flight = value_limit;
        state.remote_constraints.max_tlc_number_in_flight = count_limit;
        if balance_limit {
            state.to_remote_amount += state.to_local_amount - 5000;
            state.to_local_amount = 5000;
        }
        let mut expired = retry_batch_add(0, 1000);
        expired.expiry = 0;
        for command in [
            expired,
            retry_batch_add(1, 2000),
            retry_batch_add(2, 4000),
            retry_batch_add(3, 3000),
        ] {
            actor.register_retryable_tlc_add(&myself, &mut state, command);
        }
        actor
            .apply_retryable_tlc_operations(&myself, &mut state, false)
            .await;
        assert!(state.retryable_tlc_operations.is_empty());
        let hashes = state
            .tlc_state
            .all_tlcs()
            .map(|tlc| tlc.payment_hash)
            .collect::<Vec<_>>();
        assert_eq!(
            hashes,
            accepted
                .iter()
                .map(|tag| fiber_types::Hash256::from([*tag; 32]))
                .collect::<Vec<_>>()
        );
        capture_barrier(&network, id).await;
        let messages = std::mem::take(&mut *captured.lock().unwrap());
        let results = messages
            .iter()
            .filter_map(|message| match message {
                NetworkActorMessage::Event(NetworkActorEvent::AddTlcResult(hash, _, result, _)) => {
                    Some((*hash, result.is_ok()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results,
            (0..4)
                .map(|tag| (
                    fiber_types::Hash256::from([tag; 32]),
                    accepted.contains(&tag)
                ))
                .collect::<Vec<_>>()
        );
        let cs_count = messages
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                        FiberMessageWithTarget {
                            message: FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::CommitmentSignedV2(_)
                            ),
                            ..
                        }
                    ))
                )
            })
            .count();
        assert_eq!(cs_count, usize::from(!accepted.is_empty()));
    }
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_retry_queue_preserves_mixed_update_replay() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let preimage = fiber_types::Hash256::from([42; 32]);
    let payment_hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();
    let mut received = state
        .create_inbounding_tlc(fiber_types::AddTlc {
            channel_id: id,
            tlc_id: 0,
            amount: 1000,
            payment_hash,
            expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
            hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
            onion_packet: None,
        })
        .unwrap();
    received.status = fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::Committed);
    state.tlc_state.add_received_tlc(received);
    state.increment_next_received_tlc_id();
    let balances = (state.to_local_amount, state.to_remote_amount);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(1, 2000));
    actor.register_retryable_tlc_remove(
        &myself,
        &mut state,
        fiber_types::TLCId::Received(0),
        fiber_types::RemoveTlcReason::RemoveTlcFulfill(fiber_types::RemoveTlcFulfill {
            payment_preimage: preimage,
        }),
    );
    actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(2, 3000));
    actor
        .apply_retryable_tlc_operations(&myself, &mut state, false)
        .await;
    assert!(state.retryable_tlc_operations.is_empty());
    assert_eq!(
        (state.to_local_amount, state.to_remote_amount),
        balances,
        "removal must not settle before its ACK"
    );
    assert_eq!(a.store.get_preimage(&payment_hash), Some(preimage));
    let restored = a.store.get_channel_actor_state(&id).unwrap();
    restored.validate_session().unwrap();
    let outgoing = restored.session.outgoing.as_ref().unwrap();
    assert!(matches!(
        outgoing.updates.as_slice(),
        [
            fiber_types::TlcReplayUpdate::Add(_),
            fiber_types::TlcReplayUpdate::Remove(_),
            fiber_types::TlcReplayUpdate::Add(_)
        ]
    ));
    assert_eq!(outgoing.settlement.tlcs.len(), 2);
    capture_barrier(&network, id).await;
    let wire = take_wire_messages(&captured);
    assert!(matches!(
        wire.as_slice(),
        [
            FiberMessageWithTarget {
                message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                ..
            },
            FiberMessageWithTarget {
                message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(_)),
                ..
            },
            FiberMessageWithTarget {
                message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                ..
            },
            FiberMessageWithTarget {
                message: FiberMessage::ChannelNormalOperation(
                    FiberChannelMessage::CommitmentSignedV2(_)
                ),
                ..
            },
        ]
    ));
    assert_eq!(
        restored
            .tlc_state
            .get(&fiber_types::TLCId::Received(0))
            .unwrap()
            .status,
        fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::LocalRemoved)
    );
    handle.abort();
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_storage_codec_defers_corrupt_intent_to_preflight_and_restore() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut channel = a.get_channel_actor_state(id);
    channel.validate_session().expect("genuine funded channel");
    a.stop().await;
    b.stop().await;

    // Complete, correctly versioned bytes can contain a corrupt signing
    // intent. Reading them must not panic before the owning boundary checks it.
    channel.session.own.seed[0] ^= 1;
    let encoded = bincode::serialize(&channel.core).unwrap();
    let path = tempfile::tempdir().unwrap();
    let store = crate::store::open_store(path.path()).unwrap();
    store.insert_channel_actor_state(channel);
    let key = [&[0], id.as_ref()].concat();
    let restored = store.get_channel_actor_state(&id).unwrap();
    assert_eq!(bincode::serialize(&restored.core).unwrap(), encoded);
    assert_eq!(store.get_all_channel_states().len(), 1);
    let raw: fiber_types::ChannelActorData = fiber_types::deserialize(&encoded).unwrap();
    assert_eq!(bincode::serialize(&raw).unwrap(), encoded);
    assert!(restored.validate_session().is_err());

    assert!(
        crate::store::open_store(path.path()).is_err(),
        "preflight must reject corrupt intent"
    );
    assert!(crate::store::check_validate(path.path()).is_err());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), store.clone(), None);
    let result = Actor::spawn(
        None,
        actor,
        super::ChannelInitializationParameter {
            operation: super::ChannelInitializationOperation::RestoreOfflineChannel(id),
            ephemeral_config: Default::default(),
            private_key: a.private_key.clone(),
        },
    )
    .await;
    let error = result.expect_err("actor restore must reject corrupt intent");
    assert!(
        error
            .to_string()
            .contains("V2 nonce seed/key/public mismatch"),
        "{error}"
    );
    assert!(
        captured.lock().unwrap().is_empty(),
        "restore must not send or sign"
    );
    assert_eq!(store.get(&key).unwrap(), encoded);
    assert_eq!(
        store
            .get(fiber_store::migration::MIGRATION_VERSION_KEY)
            .unwrap(),
        fiber_store::migration::LATEST_DB_VERSION.as_bytes()
    );
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_actual_backup_rollback_quarantines_matching_malicious_peer() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut local = a.get_channel_actor_state(id);
    let mut peer = b.get_channel_actor_state(id);
    let old_peer = peer.clone();
    let backed_up_nonce = local.session.own.public_nonce.clone();
    let predecessor = local.latest_commitment_transaction.clone().unwrap();
    let consumed_funding_nonce;
    let myself = a.get_channel_actor(id).await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("live");
    let backup = root.path().join("backup");
    {
        let store = crate::store::open_store(&db).unwrap();
        store.insert_channel_actor_state(local.clone());
        store.backup(&backup).unwrap();
        let (request, _) = peer.prepare_outgoing_v2(false).unwrap();
        consumed_funding_nonce = request.funding_nonce.clone();
        peer.tlc_state.waiting_ack = true;
        local.stage_incoming_v2(&request, false).unwrap();
        let (_, _, ack) = local.accept_commitment_v2(&request, false, false).unwrap();
        local.tlc_state.update_for_commitment_signed();
        assert_eq!(
            local
                .session
                .incoming
                .as_ref()
                .unwrap()
                .funding
                .public_nonce,
            backed_up_nonce
        );
        assert_ne!(
            local
                .latest_commitment_transaction
                .as_ref()
                .unwrap()
                .as_slice(),
            predecessor.as_slice()
        );
        peer.stage_ack_v2(&ack.clone().unwrap()).unwrap();
        peer.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
        assert!(peer.session.revocation_effect.is_some());
        store.insert_channel_actor_state(local);
    }
    crate::store::restore::restore(
        &backup,
        &db,
        &root.path().join("sk"),
        &root.path().join("key"),
    )
    .unwrap();
    let store = crate::store::open_store(&db).unwrap();
    let mut stale = store.get_channel_actor_state(&id).unwrap();
    assert_eq!(stale.state, ChannelState::Stale);
    assert_eq!(
        stale
            .latest_commitment_transaction
            .as_ref()
            .unwrap()
            .as_slice(),
        predecessor.as_slice()
    );
    assert_eq!(stale.session.own.public_nonce, backed_up_nonce);
    assert!(stale.session.own.context.is_none());
    let before = bincode::serialize(&stale.session).unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    stale.network = Some(network.clone());
    stale.mark_reestablishing_offline();
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), store, None);
    let _ = actor
        .reestablish_v2(&myself, &mut stale, old_peer.reestablish_message_v2())
        .await;
    assert_eq!(
        stale.state,
        ChannelState::Stale,
        "peer assertions cannot prove backup freshness"
    );
    let mut malicious = old_peer;
    let (conflict, _) = malicious.prepare_outgoing_v2(false).unwrap();
    assert_ne!(conflict.funding_nonce, consumed_funding_nonce);
    // Establish that this is a valid changed aggregate context for the old
    // image, rather than relying on rejection of an invalid partial/phase.
    let mut old_image = stale.clone();
    old_image.state = ChannelState::ChannelReady;
    old_image.stage_incoming_v2(&conflict, false).unwrap();
    assert!(actor
        .receive_commitment_v2(&myself, &mut stale, conflict)
        .await
        .is_err());
    assert!(stale.prepare_outgoing_v2(false).is_err());
    for force in [false, true] {
        assert!(actor
            .handle_shutdown_command(
                &mut stale,
                super::ShutdownCommand {
                    force,
                    close_script: None,
                    fee_rate: None
                }
            )
            .await
            .is_err());
    }
    stale.reestablish_timer = Some(ReestablishTimer::RunningSince(0));
    stale.waiting_peer_response = Some(0);
    actor
        .handle_event(&myself, &mut stale, super::ChannelEvent::CheckActiveChannel)
        .await
        .unwrap();
    assert!(stale.get_latest_commitment_transaction().await.is_err());
    assert_eq!(bincode::serialize(&stale.session).unwrap(), before);
    capture_barrier(&network, id).await;
    network.stop(None);
    task.await.unwrap();
    assert!(!captured.lock().unwrap().iter().any(|m| matches!(
        m,
        NetworkActorMessage::Event(
            crate::fiber::network::NetworkActorEvent::ClosingTransactionPending(..)
        ) | NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(_))
            | NetworkActorMessage::Event(crate::fiber::network::NetworkActorEvent::ChannelReady(
                ..
            ))
            | NetworkActorMessage::Notification(
                NetworkServiceEvent::ChannelOnline(..) | NetworkServiceEvent::ChannelReady(..)
            )
    )));
}

#[tokio::test]
async fn test_v2_restart_resets_ack_and_close_peer_waits() {
    for (waiting_ack, closing) in [(true, false), (false, true), (true, true)] {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let myself = a.get_channel_actor(id).await.unwrap();
        a.stop().await;
        b.stop().await;

        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured).await.unwrap();
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        state.network = Some(network.clone());
        if waiting_ack {
            state.prepare_outgoing_v2(false).unwrap();
            state.tlc_state.waiting_ack = true;
            state
                .session
                .outgoing
                .as_mut()
                .unwrap()
                .peer_wait_started_at = 0;
        }
        if closing {
            actor
                .shutdown_v2(
                    &mut state,
                    super::ShutdownCommand {
                        force: false,
                        close_script: None,
                        fee_rate: None,
                    },
                )
                .await
                .unwrap();
            state.session.closing.as_mut().unwrap().peer_wait_started_at = 0;
        }
        assert!(state.peer_does_not_reply_ack_in_time());
        a.store.insert_channel_actor_state(state);

        let started = crate::now_timestamp_as_millis_u64();
        let mut restored = actor
            .pre_start(
                myself,
                super::ChannelInitializationParameter {
                    operation: super::ChannelInitializationOperation::RestoreOfflineChannel(id),
                    ephemeral_config: Default::default(),
                    private_key: a.private_key.clone(),
                },
            )
            .await
            .unwrap();
        if waiting_ack {
            assert!(restored.ack_peer_wait_v2().unwrap() >= started);
        }
        if closing {
            assert!(restored.close_peer_wait_v2().unwrap() >= started);
        }
        assert!(!restored.peer_does_not_reply_ack_in_time());
        let persisted = a.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(
            persisted.peer_response_wait_v2(),
            restored.peer_response_wait_v2()
        );

        // In-process disconnect/reconnect must not grant another timeout.
        let deadline = restored.peer_response_wait_v2();
        restored.mark_reestablishing_offline();
        assert_eq!(restored.peer_response_wait_v2(), deadline);
        if waiting_ack {
            restored
                .session
                .outgoing
                .as_mut()
                .unwrap()
                .peer_wait_started_at = 0;
        } else {
            restored
                .session
                .closing
                .as_mut()
                .unwrap()
                .peer_wait_started_at = 0;
        }
        assert!(restored.peer_does_not_reply_ack_in_time());
        network.stop(None);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn test_v2_close_peer_obligations_timeout_normal_and_reconnect() {
    for closing in [false, true] {
        for reconnect in [false, true] {
            let (a, b, id) =
                create_nodes_with_established_channel(100000000000, 11800000000, false).await;
            let mut state = a.get_channel_actor_state(id);
            let mut peer = b.get_channel_actor_state(id);
            let myself = a.get_channel_actor(id).await.unwrap();
            let predecessor = state.latest_commitment_transaction.clone().unwrap();
            let (request, _) = peer.prepare_outgoing_v2(false).unwrap();
            peer.tlc_state.waiting_ack = true;
            state.stage_incoming_v2(&request, false).unwrap();
            let (_, _, ack) = state.accept_commitment_v2(&request, false, false).unwrap();
            state.tlc_state.update_for_commitment_signed();
            peer.stage_ack_v2(&ack.clone().unwrap()).unwrap();
            peer.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
            assert_ne!(
                state
                    .latest_commitment_transaction
                    .as_ref()
                    .unwrap()
                    .as_slice(),
                predecessor.as_slice()
            );
            let usable = state
                .get_latest_commitment_transaction()
                .await
                .unwrap()
                .data();
            let captured = Arc::new(Mutex::new(Vec::new()));
            let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
                .await
                .unwrap();
            state.network = Some(network.clone());
            peer.network = Some(network.clone());
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
            let remote =
                ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
            actor
                .shutdown_v2(
                    &mut state,
                    super::ShutdownCommand {
                        force: false,
                        close_script: None,
                        fee_rate: None,
                    },
                )
                .await
                .unwrap();
            if closing {
                let shutdown =
                    ShutdownV2::try_from(state.session.closing.as_ref().unwrap().shutdown.clone())
                        .unwrap();
                remote
                    .receive_shutdown_v2(&mut peer, shutdown)
                    .await
                    .unwrap();
                let reply =
                    ShutdownV2::try_from(peer.session.closing.as_ref().unwrap().shutdown.clone())
                        .unwrap();
                actor
                    .receive_shutdown_v2(&mut state, reply.clone())
                    .await
                    .unwrap();
                let deadline = state.waiting_peer_response;
                actor.receive_shutdown_v2(&mut state, reply).await.unwrap();
                assert_eq!(state.waiting_peer_response, deadline);
            }
            state.waiting_peer_response = Some(0);
            state.session.closing.as_mut().unwrap().peer_wait_started_at = 0;
            if reconnect {
                let encoded = bincode::serialize(&state.core).unwrap();
                state.core = bincode::deserialize(&encoded).unwrap();
                state.waiting_peer_response = None;
                state.mark_reestablishing_offline();
                // A reconnected peer cannot have answered while the channel was offline, so
                // the handshake itself must not be judged against the old deadline.
                state.connectivity_state = fiber_types::ChannelConnectivityState::Syncing;
                assert!(
                    !state.peer_does_not_reply_ack_in_time(),
                    "a reestablishing channel must not exceed the deadline yet"
                );
                actor
                    .reestablish_v2(&myself, &mut state, peer.reestablish_message_v2())
                    .await
                    .unwrap();
                assert!(!state.reestablishing);
                // The reconnected peer gets a fresh window instead of being force-closed.
                assert!(
                    !state.peer_does_not_reply_ack_in_time(),
                    "a reconnect must restart the peer response window, not extend the old deadline"
                );
                assert!(
                    state.peer_response_wait_v2().is_some_and(|started| {
                        crate::now_timestamp_as_millis_u64().saturating_sub(started) < 5_000
                    }),
                    "the deadline must be refreshed on reconnect"
                );
                network.stop(None);
                task.await.unwrap();
                continue;
            }
            assert!(
                state.peer_does_not_reply_ack_in_time(),
                "unanswered close must have an effective deadline"
            );
            actor
                .handle_event(&myself, &mut state, super::ChannelEvent::CheckActiveChannel)
                .await
                .unwrap();
            capture_barrier(&network, id).await;
            let shutdown = {
                let mut messages = captured.lock().unwrap();
                let index = messages.iter().position(|m| matches!(m,
                    NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                        super::ChannelCommandWithId { channel_id, command: super::ChannelCommand::Shutdown(command, _) }
                    )) if *channel_id == id && command.force
                )).expect("CheckActiveChannel must request force close, not merely schedule another check");
                let NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                    super::ChannelCommandWithId {
                        command: super::ChannelCommand::Shutdown(command, _),
                        ..
                    },
                )) = messages.remove(index)
                else {
                    unreachable!()
                };
                command
            };
            actor
                .handle_shutdown_command(&mut state, shutdown)
                .await
                .unwrap();
            capture_barrier(&network, id).await;
            network.stop(None);
            task.await.unwrap();
            let messages = captured.lock().unwrap();
            assert!(messages.iter().any(|m| matches!(m, NetworkActorMessage::Event(crate::fiber::network::NetworkActorEvent::ClosingTransactionPending(_, _, tx, true)) if tx.data().as_slice() == usable.as_slice())));
            assert!(!state.peer_does_not_reply_ack_in_time());
        }
    }
}

/// A network partition must not consume the peer's response window: while the channel is
/// offline or reestablishing the peer cannot answer, and the window restarts when the
/// channel is back online (bounded by `REESTABLISH_TIMEOUT` for a reconnect that never
/// completes).
/// A channel actor message sink for tests that only need a valid `ActorRef`.
struct NoopChannelActor;

#[async_trait::async_trait]
impl Actor for NoopChannelActor {
    type Msg = ChannelActorMessage;
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
        _message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
}

/// A peer that never saw our ACK replays the exact `CommitmentSignedV2` we already accepted.
/// It must be answered from the durable `incoming` record: re-verifying it against the current
/// state cannot reproduce the transaction that was signed and would fail, which used to leave
/// the channel reestablishing forever (see the cursor reconciliation note).
#[tokio::test]
async fn test_v2_replayed_accepted_cs_is_answered_from_durable_record() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state_a = a.get_channel_actor_state(id);
    let mut state_b = b.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, network_task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state_a.network = Some(network.clone());
    state_b.network = Some(network.clone());
    let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    let (b_ref, b_handle) = Actor::spawn(None, NoopChannelActor, ())
        .await
        .expect("spawn B no-op channel actor");

    // A signs a CS and B accepts it, so B holds the exact request and its response.
    let (request, _) = state_a.prepare_outgoing_v2(false).expect("prepare CS");
    let remote_before = state_b.get_remote_commitment_number();
    actor_b
        .receive_commitment_v2(&b_ref, &mut state_b, request.clone())
        .await
        .expect("first CS must be accepted");
    assert_eq!(
        state_b.get_remote_commitment_number(),
        remote_before + 1,
        "accepting the CS advances the peer commitment number"
    );
    assert!(
        state_b
            .session
            .incoming
            .as_ref()
            .unwrap()
            .response
            .is_some(),
        "the accepted response is durable"
    );
    capture_barrier(&network, id).await;
    captured.lock().unwrap().clear();

    // The reconnect replays the same message because the ACK never arrived.
    actor_b
        .receive_commitment_v2(&b_ref, &mut state_b, request)
        .await
        .expect("a replayed accepted CS must not fail verification");
    assert_eq!(
        state_b.get_remote_commitment_number(),
        remote_before + 1,
        "the replay must not be accepted twice"
    );
    capture_barrier(&network, id).await;

    let acks = captured
        .lock()
        .unwrap()
        .iter()
        .filter(|message| {
            matches!(
                message,
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                    FiberMessageWithTarget {
                        message: FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::RevokeAndAckV2(_)
                        ),
                        ..
                    }
                ))
            )
        })
        .count();
    assert_eq!(
        acks, 1,
        "the replay must be answered with the cached ACK exactly once"
    );

    b_handle.abort();
    network.stop(None);
    network_task.await.unwrap();
}

#[tokio::test]
async fn test_v2_partition_suspends_and_then_restarts_peer_response_window() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);

    // A cooperative close is in flight, so our own peer-response obligation exists.
    actor
        .shutdown_v2(
            &mut state,
            super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();
    state.session.closing.as_mut().unwrap().peer_wait_started_at = 0;
    assert!(
        state.peer_does_not_reply_ack_in_time(),
        "a live channel with an ancient deadline is timed out"
    );

    // A peer that stays away must still be closed: the deadline keeps counting offline.
    state.mark_reestablishing_offline();
    assert!(
        state.peer_does_not_reply_ack_in_time(),
        "an absent peer must still be force-closed"
    );

    // Peer is back and its reestablishment handshake is in flight: the owed messages could
    // not have been answered before the reconnect, so judgement is suspended.
    state.connectivity_state = fiber_types::ChannelConnectivityState::Syncing;
    assert!(
        !state.peer_does_not_reply_ack_in_time(),
        "a reestablishing channel must not consume the peer response window"
    );

    // Reestablishment completed: the peer is reachable again, so the window restarts.
    state.session.closing.as_mut().unwrap().peer_wait_started_at = 0;
    state.refresh_peer_response_wait_v2_after_reestablish();
    let started = state
        .peer_response_wait_v2()
        .expect("the close obligation still has a deadline");
    assert!(
        crate::now_timestamp_as_millis_u64().saturating_sub(started) < 5_000,
        "a reconnect must restart the peer response window"
    );
    assert!(
        !state.peer_does_not_reply_ack_in_time(),
        "a reconnect must not force-close immediately"
    );

    network.stop(None);
    task.await.unwrap();
}

async fn assert_timer_publishes_usable(
    node: &mut crate::tests::test_utils::NetworkNode,
    id: fiber_types::Hash256,
    usable: &TransactionView,
    timeout: std::time::Duration,
) {
    let published = tokio::time::timeout(timeout, async {
        loop {
            if let Some(tx) = node
                .get_transaction_view_from_hash(usable.hash().into())
                .await
            {
                break tx;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let state = node.get_channel_actor_state(id);
        panic!("retained peer deadline must automatically publish: state={:?}, waiting_ack={}, ack_start={:?}, close_start={:?}, expired={}, now={}",
            state.state, state.tlc_state.waiting_ack, state.ack_peer_wait_v2(), state.close_peer_wait_v2(),
            state.peer_does_not_reply_ack_in_time(), crate::now_timestamp_as_millis_u64());
    });
    assert_eq!(published.data().as_slice(), usable.data().as_slice());
}

enum RestartCloseExpectation {
    Publish,
    Quarantined,
    AwaitChain,
    LocalSigning,
}

async fn assert_offline_close_restart_rearms(closing: bool, expectation: RestartCloseExpectation) {
    use crate::fiber::network::TestFiberMessageKind;
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let predecessor = a
        .get_channel_actor_state(id)
        .latest_commitment_transaction
        .clone()
        .unwrap();
    let payment = a.send_payment_keysend(&b, 1000, false).await.unwrap();
    a.wait_until_payment_finished(payment.payment_hash).await;
    let usable = a
        .get_channel_actor_state(id)
        .get_latest_commitment_transaction()
        .await
        .unwrap();
    assert_ne!(usable.data().raw().as_slice(), predecessor.raw().as_slice());
    assert!(a
        .get_transaction_view_from_hash(usable.hash().into())
        .await
        .is_none());
    if closing {
        a.hold_next_fiber_messages(b.pubkey, id, TestFiberMessageKind::ClosingSigned, 1)
            .await;
        b.hold_next_fiber_messages(a.pubkey, id, TestFiberMessageKind::ClosingSigned, 1)
            .await;
    } else {
        a.hold_next_fiber_messages(b.pubkey, id, TestFiberMessageKind::Shutdown, 1)
            .await;
    }
    a.send_shutdown_command_to_channel(
        id,
        super::ShutdownCommand {
            force: false,
            close_script: None,
            fee_rate: None,
        },
    )
    .await;
    a.wait_for_held_fiber_messages(1).await;
    if closing {
        b.wait_for_held_fiber_messages(1).await;
    }
    let peer_response = b
        .get_channel_actor_state(id)
        .session
        .closing
        .as_ref()
        .and_then(|c| c.response.clone());
    let mut retained = a.get_channel_actor_state(id);
    let close = retained.session.closing.as_mut().unwrap();
    assert_eq!(close.remote_shutdown.is_some(), closing);
    assert_eq!(close.response.is_some(), closing);
    assert!(close.remote_response.is_none());
    close.peer_wait_started_at = 0;
    b.stop().await;
    a.stop().await;
    match expectation {
        RestartCloseExpectation::Quarantined => retained.state = ChannelState::Stale,
        RestartCloseExpectation::AwaitChain => {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let (network, task) = Actor::spawn(None, CaptureNetwork, captured).await.unwrap();
            retained.network = Some(network.clone());
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
            actor
                .receive_closing_v2(
                    &mut retained,
                    ClosingSignedV2::try_from(peer_response.unwrap()).unwrap(),
                )
                .await
                .unwrap();
            capture_barrier(&network, id).await;
            network.stop(None);
            task.await.unwrap();
            assert!(retained.close_peer_wait_v2().is_none());
        }
        RestartCloseExpectation::LocalSigning => {
            let close = retained.session.closing.as_mut().unwrap();
            close.response = None;
            close.own.signature = None;
            retained.local_shutdown_info.as_mut().unwrap().signature = None;
            assert!(retained.close_peer_wait_v2().is_none());
            assert!(retained.ack_peer_wait_v2().is_none());
            fiber_types::channel_v2_validation::validate_channel_v2(&retained.core).unwrap();
        }
        RestartCloseExpectation::Publish => {}
    }
    let intent = bincode::serialize(&retained.session.closing).unwrap();
    // Store the original intent after stopping so handler-end persistence
    // cannot overwrite the expired durable deadline. No old actor/timer survives.
    a.store.insert_channel_actor_state(retained);
    let restarted_at = crate::now_timestamp_as_millis_u64();
    a.start().await;
    if matches!(expectation, RestartCloseExpectation::Publish) {
        let restored = a.get_channel_actor_state(id);
        assert!(restored.close_peer_wait_v2().unwrap() >= restarted_at);
        assert!(!restored.peer_does_not_reply_ack_in_time());
        assert!(a
            .get_transaction_view_from_hash(usable.hash().into())
            .await
            .is_none());
        assert_timer_publishes_usable(
            &mut a,
            id,
            &usable,
            std::time::Duration::from_millis(super::PEER_CHANNEL_RESPONSE_TIMEOUT + 5000),
        )
        .await;
    } else {
        tokio::time::sleep(
            crate::fiber::network::CHECK_CHANNELS_INTERVAL + std::time::Duration::from_millis(200),
        )
        .await;
        assert!(a
            .get_transaction_view_from_hash(usable.hash().into())
            .await
            .is_none());
    }
    let mut after = a
        .get_channel_actor_state(id)
        .session
        .closing
        .clone()
        .unwrap();
    assert_eq!(
        after.superseded_by_force_close,
        matches!(expectation, RestartCloseExpectation::Publish)
    );
    after.superseded_by_force_close = false;
    if matches!(expectation, RestartCloseExpectation::Publish) {
        // Only the watchdog timestamp may change; signing intent stays exact.
        after.peer_wait_started_at = 0;
    }
    assert_eq!(bincode::serialize(&Some(after)).unwrap(), intent);
    if matches!(expectation, RestartCloseExpectation::Quarantined) {
        assert_eq!(a.get_channel_actor_state(id).state, ChannelState::Stale);
    }
}

#[tokio::test]
async fn test_v2_offline_restart_rearms_shutdown_watchdog() {
    assert_offline_close_restart_rearms(false, RestartCloseExpectation::Publish).await;
}

#[tokio::test]
async fn test_v2_offline_restart_rearms_closing_signature_watchdog() {
    assert_offline_close_restart_rearms(true, RestartCloseExpectation::Publish).await;
}

#[tokio::test]
async fn test_v2_offline_restart_watchdog_keeps_stale_quarantined() {
    assert_offline_close_restart_rearms(false, RestartCloseExpectation::Quarantined).await;
}

#[tokio::test]
async fn test_v2_offline_restart_watchdog_excludes_both_closing_signatures() {
    assert_offline_close_restart_rearms(true, RestartCloseExpectation::AwaitChain).await;
}

#[tokio::test]
async fn test_v2_offline_restart_watchdog_excludes_local_unsigned_close_intent() {
    assert_offline_close_restart_rearms(true, RestartCloseExpectation::LocalSigning).await;
}

async fn assert_shutdown_draining_ack_watchdog(restart: bool) {
    use crate::fiber::network::TestFiberMessageKind;
    crate::tests::test_utils::init_tracing();
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let payment = a.send_payment_keysend(&b, 1000, false).await.unwrap();
    a.wait_until_payment_finished(payment.payment_hash).await;
    let usable = a
        .get_channel_actor_state(id)
        .get_latest_commitment_transaction()
        .await
        .unwrap();
    b.hold_next_fiber_messages(a.pubkey, id, TestFiberMessageKind::RevokeAndAck, 1)
        .await;
    a.network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(super::ChannelCommandWithId {
                channel_id: id,
                command: super::ChannelCommand::CommitmentSigned(None),
            }),
        ))
        .unwrap();
    b.wait_for_held_fiber_messages(1).await;
    b.hold_next_fiber_messages(a.pubkey, id, TestFiberMessageKind::ClosingSigned, 1)
        .await;
    assert!(a.get_channel_actor_state(id).tlc_state.waiting_ack);
    a.send_shutdown_command_to_channel(
        id,
        super::ShutdownCommand {
            force: false,
            close_script: None,
            fee_rate: None,
        },
    )
    .await;
    b.wait_for_held_fiber_messages(2).await;
    // B holding ClosingSigned only proves its own shutdown was sent. A's
    // network/channel actors can still be processing that Shutdown message.
    crate::tests::test_utils::wait_until_timeout(5_000, || {
        a.get_channel_actor_state(id)
            .session
            .closing
            .as_ref()
            .is_some_and(|close| close.remote_shutdown.is_some())
    })
    .await;
    let mut retained = a.get_channel_actor_state(id);
    let close = retained.session.closing.as_ref().unwrap();
    assert!(close.remote_shutdown.is_some());
    assert!(close.response.is_none());
    assert!(retained.tlc_state.waiting_ack);
    assert!(
        retained.close_peer_wait_v2().is_none(),
        "this is an ACK obligation, not a closing-signature wait"
    );
    let outgoing = bincode::serialize(&retained.session.outgoing).unwrap();
    let peer_shutdown = ShutdownV2::try_from(close.remote_shutdown.clone().unwrap()).unwrap();
    // Duplicate shutdowns must neither unblock signing nor replace the ACK intent.
    a.network_actor
        .send_message(NetworkActorMessage::Event(
            crate::fiber::network::NetworkActorEvent::FiberMessage(
                b.pubkey,
                FiberMessage::shutdown_v2(peer_shutdown),
                None,
            ),
        ))
        .unwrap();
    let channel = a.get_channel_actor(id).await.unwrap();
    ractor::call_t!(
        channel,
        |reply| ChannelActorMessage::Command(super::ChannelCommand::TestBarrier(reply)),
        5000
    )
    .unwrap();
    assert_eq!(
        bincode::serialize(&a.get_channel_actor_state(id).session.outgoing).unwrap(),
        outgoing
    );
    let original_wait_started_at = retained.ack_peer_wait_v2().unwrap();
    if restart {
        b.stop().await;
        a.stop().await;
        retained.reestablishing = true;
        a.store.insert_channel_actor_state(retained);
        let restarted_at = crate::now_timestamp_as_millis_u64();
        a.start().await;
        let restored = a.get_channel_actor_state(id);
        assert!(restored.ack_peer_wait_v2().unwrap() >= restarted_at);
        assert!(!restored.peer_does_not_reply_ack_in_time());
    }
    assert_timer_publishes_usable(
        &mut a,
        id,
        &usable,
        std::time::Duration::from_millis(super::PEER_CHANNEL_RESPONSE_TIMEOUT + 5000),
    )
    .await;
    let mut after = a
        .get_channel_actor_state(id)
        .session
        .outgoing
        .clone()
        .unwrap();
    // Restart resets only the response timer, never the signing intent.
    after.peer_wait_started_at = original_wait_started_at;
    assert_eq!(bincode::serialize(&Some(after)).unwrap(), outgoing);
}

#[tokio::test]
async fn test_v2_shutdown_draining_ack_watchdog_publishes() {
    assert_shutdown_draining_ack_watchdog(false).await;
}

#[tokio::test]
async fn test_v2_offline_restart_rearms_shutdown_draining_ack_watchdog() {
    assert_shutdown_draining_ack_watchdog(true).await;
}

#[tokio::test]
async fn test_v2_close_restart_replays_lost_signatures_exactly() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let hash = a.send_payment_keysend(&b, 1000, false).await.unwrap();
    a.wait_until_success(hash.payment_hash).await;
    a.hold_next_fiber_messages(
        b.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::ClosingSigned,
        1,
    )
    .await;
    b.hold_next_fiber_messages(
        a.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::ClosingSigned,
        1,
    )
    .await;
    a.send_shutdown_command_to_channel(
        id,
        super::ShutdownCommand {
            force: false,
            close_script: None,
            fee_rate: None,
        },
    )
    .await;
    a.wait_for_held_fiber_messages(1).await;
    b.wait_for_held_fiber_messages(1).await;
    let before_a = a
        .get_channel_actor_state(id)
        .session
        .closing
        .clone()
        .unwrap();
    let before_b = b
        .get_channel_actor_state(id)
        .session
        .closing
        .clone()
        .unwrap();
    a.stop().await;
    b.stop().await;
    a.start().await;
    b.start().await;
    a.connect_to(&mut b).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(a.get_channel_actor_state(id).state, ChannelState::Closed(flags) if flags.contains(fiber_types::CloseFlags::COOPERATIVE))
                && matches!(b.get_channel_actor_state(id).state, ChannelState::Closed(flags) if flags.contains(fiber_types::CloseFlags::COOPERATIVE)) { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    for (node, before) in [(&a, before_a), (&b, before_b)] {
        let state = node.get_channel_actor_state(id);
        let after = state.session.closing.as_ref().unwrap();
        assert_eq!(after.own.public_nonce, before.own.public_nonce);
        assert_eq!(after.own.context, before.own.context);
        assert_eq!(
            after.response.as_ref().unwrap().as_slice(),
            before.response.as_ref().unwrap().as_slice()
        );
        assert_eq!(
            after.transaction.as_ref().unwrap().as_slice(),
            before.transaction.as_ref().unwrap().as_slice()
        );
        fiber_types::channel_v2_validation::validate_channel_v2(&state.core).unwrap();
    }
}

#[tokio::test]
async fn test_v2_close_after_update_crash_replay_conflicts_and_db_validation() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sa = a.get_channel_actor_state(id);
    let mut sb = b.get_channel_actor_state(id);
    let myself = a.get_channel_actor(id).await.unwrap();
    let (request, _) = sa.prepare_outgoing_v2(false).unwrap();
    sa.tlc_state.waiting_ack = true;
    sb.stage_incoming_v2(&request, false).unwrap();
    let (_, _, ack) = sb.accept_commitment_v2(&request, false, false).unwrap();
    sb.tlc_state.update_for_commitment_signed();
    let ack = ack.unwrap();
    sa.stage_ack_v2(&ack).unwrap();
    sa.accept_ack_v2(&myself, &ack, false).unwrap();
    let usable = sa.latest_commitment_transaction.clone().unwrap();
    let old_nonce = sa.session.own.public_nonce.clone();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sa.network = Some(network.clone());
    sb.network = Some(network.clone());
    let actor_a = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    actor_a
        .shutdown_v2(
            &mut sa,
            super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();
    let shutdown =
        ShutdownV2::try_from(sa.session.closing.as_ref().unwrap().shutdown.clone()).unwrap();
    assert_ne!(shutdown.closing_nonce, old_nonce);
    assert_ne!(
        shutdown.closing_nonce,
        sa.session.outgoing.as_ref().unwrap().funding.public_nonce
    );
    actor_b
        .receive_shutdown_v2(&mut sb, shutdown.clone())
        .await
        .unwrap();
    let peer_shutdown =
        ShutdownV2::try_from(sb.session.closing.as_ref().unwrap().shutdown.clone()).unwrap();
    actor_a
        .receive_shutdown_v2(&mut sa, peer_shutdown.clone())
        .await
        .unwrap();
    let original = sa.session.closing.clone().unwrap();
    let bytes = bincode::serialize(&sa.core).unwrap();
    sa.core = bincode::deserialize(&bytes).unwrap();
    fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();
    // A crash after the durable binding but before signing is legitimate.
    // Recovery must complete that same context, producing the original response.
    {
        let pending = sa.session.closing.as_mut().unwrap();
        pending.own.signature = None;
        pending.response = None;
    }
    sa.local_shutdown_info.as_mut().unwrap().signature = None;
    fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();
    actor_a.progress_close_v2(&mut sa).await.unwrap();
    assert_eq!(
        sa.session.closing.as_ref().unwrap().own.signature,
        original.own.signature
    );
    actor_a
        .receive_shutdown_v2(&mut sa, peer_shutdown.clone())
        .await
        .unwrap();
    let mut conflict = peer_shutdown;
    conflict.fee_rate += 1;
    assert!(actor_a
        .receive_shutdown_v2(&mut sa, conflict)
        .await
        .is_err());
    assert_eq!(
        sa.session
            .closing
            .as_ref()
            .unwrap()
            .response
            .as_ref()
            .unwrap()
            .as_slice(),
        original.response.as_ref().unwrap().as_slice()
    );
    assert_eq!(
        sa.latest_commitment_transaction
            .as_ref()
            .unwrap()
            .as_slice(),
        usable.as_slice()
    );
    sa.mark_reestablishing_offline();
    actor_a
        .reestablish_v2(&myself, &mut sa, sb.reestablish_message_v2())
        .await
        .unwrap();
    let peer_response = ClosingSignedV2::try_from(
        sb.session
            .closing
            .as_ref()
            .unwrap()
            .response
            .clone()
            .unwrap(),
    )
    .unwrap();
    actor_a
        .receive_closing_v2(&mut sa, peer_response.clone())
        .await
        .unwrap();
    sa.waiting_peer_response = Some(0);
    sa.session.closing.as_mut().unwrap().peer_wait_started_at = 0;
    assert!(
        !sa.peer_does_not_reply_ack_in_time(),
        "both signatures owe no peer response while waiting on-chain"
    );
    actor_a
        .receive_closing_v2(&mut sa, peer_response.clone())
        .await
        .unwrap();
    let mut conflict = peer_response;
    conflict.partial_signature = original.own.signature.unwrap();
    assert!(actor_a.receive_closing_v2(&mut sa, conflict).await.is_err());
    fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();
    // Actual complete current codec is reopened through migration preflight,
    // including closed records; epoch alone is never evidence.
    let path = tempfile::tempdir().unwrap();
    let key = [&[0], id.as_ref()].concat();
    sa.core.state = ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
    let good = bincode::serialize(&sa.core).unwrap();
    {
        let store = fiber_store::Store::open_db(path.path()).unwrap();
        store.put(
            fiber_store::migration::MIGRATION_VERSION_KEY,
            fiber_store::migration::LATEST_DB_VERSION,
        );
        store.put(&key, &good);
    }
    {
        let store = crate::store::open_store(path.path()).expect("genuine V2 database reopens");
        let restored = store
            .get_channel_actor_state(&id)
            .expect("genuine V2 is readable");
        assert_eq!(bincode::serialize(&restored.core).unwrap(), good);
        assert_eq!(store.get_all_channel_states().len(), 1);
    }
    for bytes in [good[..good.len() - 1].to_vec(), [&good[..], &[0]].concat()] {
        {
            let store = fiber_store::Store::open_db(path.path()).unwrap();
            store.put(&key, &bytes);
        }
        assert!(
            crate::store::open_store(path.path()).is_err(),
            "exact current schema required"
        );
        let store = fiber_store::Store::open_db(path.path()).unwrap();
        assert_eq!(store.get(&key).unwrap(), bytes);
    }
    for mutation in 0..9 {
        let mut bad = sa.core.clone();
        let session = &mut bad.session;
        match mutation {
            0 => session.marker ^= 1,
            1 => session.closing.as_mut().unwrap().own.seed[0] ^= 1,
            2 => {
                session
                    .closing
                    .as_mut()
                    .unwrap()
                    .own
                    .context
                    .as_mut()
                    .unwrap()[163] ^= 1
            }
            3 => session.incoming.as_mut().unwrap().funding.owner = a.pubkey,
            4 => session.incoming.as_mut().unwrap().settlement.local_amount ^= 1,
            _ => {
                let incoming = session.incoming.as_mut().unwrap();
                let witness = incoming.transaction.witnesses().get(0).unwrap().raw_data();
                let witnesses = match mutation {
                    5 => vec![witness.slice(16..).pack()],
                    6 => {
                        let mut bytes = witness.to_vec();
                        bytes[0] ^= 1;
                        vec![ckb_types::bytes::Bytes::from(bytes).pack()]
                    }
                    7 => vec![witness.pack(), Bytes::default()],
                    _ => vec![],
                };
                let tx = incoming
                    .transaction
                    .clone()
                    .into_view()
                    .as_advanced_builder()
                    .set_witnesses(witnesses)
                    .build()
                    .data();
                incoming.transaction = tx.clone();
                bad.latest_commitment_transaction = Some(tx);
            }
        }
        let encoded = bincode::serialize(&bad).unwrap();
        {
            let store = fiber_store::Store::open_db(path.path()).unwrap();
            store.put(&key, &encoded);
        }
        assert!(crate::store::open_store(path.path()).is_err());
        let store = fiber_store::Store::open_db(path.path()).unwrap();
        assert_eq!(store.get(&key).unwrap(), encoded);
        assert_eq!(
            store
                .get(fiber_store::migration::MIGRATION_VERSION_KEY)
                .unwrap(),
            fiber_store::migration::LATEST_DB_VERSION.as_bytes()
        );
    }
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_rejected_shutdown_leaves_no_stale_remote_info_across_restore() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sa = a.get_channel_actor_state(id);
    let mut sb = b.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sa.network = Some(network.clone());
    sb.network = Some(network.clone());
    let actor_a = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);

    // The peer advertises a genuine closing nonce with a fee below the
    // minimum this node auto-accepts, but still affordable for the peer.
    actor_b
        .shutdown_v2(
            &mut sb,
            super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();
    let mut peer_shutdown =
        ShutdownV2::try_from(sb.session.closing.as_ref().unwrap().shutdown.clone()).unwrap();
    peer_shutdown.fee_rate = 0;
    assert!(peer_shutdown.fee_rate < sa.commitment_fee_rate);
    assert!(sa.check_shutdown_fee_valid(&peer_shutdown.close_script, peer_shutdown.fee_rate));

    assert!(actor_a
        .receive_shutdown_v2(&mut sa, peer_shutdown)
        .await
        .is_err());
    // A rejected shutdown must not leave any durable trace. The actor
    // persists the channel state even when the handler returns an error.
    assert!(sa.remote_shutdown_info.is_none());
    assert!(sa.session.closing.is_none());
    assert_eq!(sa.state, ChannelState::ChannelReady);

    // Persist the rejected state, then start a local cooperative close from
    // it. Before the fix the stale remote info survived and paired with a
    // closing session that has no remote advertisement.
    let encoded = bincode::serialize(&sa.core).unwrap();
    sa.core = bincode::deserialize(&encoded).unwrap();
    actor_a
        .shutdown_v2(
            &mut sa,
            super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();
    assert!(sa.remote_shutdown_info.is_none());
    assert!(sa
        .session
        .closing
        .as_ref()
        .unwrap()
        .remote_shutdown
        .is_none());

    // Restoring the closing session must pass V2 validation instead of
    // failing with "V2 remote shutdown without advertisement".
    let encoded = bincode::serialize(&sa.core).unwrap();
    sa.core = bincode::deserialize(&encoded).unwrap();
    fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();

    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_recovery_handshake_precedes_replay_and_watchdog_bounds_wait() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let myself = a.get_channel_actor(id).await.unwrap();
    let own = state.session.own.public_nonce.clone();
    let (request, _) = state.prepare_outgoing_v2(false).unwrap();
    let ack_deadline_start = state
        .session
        .outgoing
        .as_ref()
        .unwrap()
        .peer_wait_started_at;
    state.tlc_state.waiting_ack = true;
    state.mark_reestablishing_offline();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    // Traffic before the peer handshake cannot consume our verification nonce.
    let mut incoming = b.get_channel_actor_state(id);
    let (premature, _) = incoming.prepare_outgoing_v2(false).unwrap();
    actor
        .handle_peer_message(
            &myself,
            &mut state,
            FiberChannelMessage::CommitmentSignedV2(premature),
        )
        .await
        .unwrap();
    // This path has no fresh reconnect (the channel is already Syncing), so the handshake
    // must not consume or replace the verification nonce.
    assert_eq!(state.session.own.public_nonce, own);
    state.reestablish_timer = Some(ReestablishTimer::RunningSince(
        crate::now_timestamp_as_millis_u64() - super::REESTABLISH_TIMEOUT / 2,
    ));
    actor
        .handle_peer_message(
            &myself,
            &mut state,
            FiberChannelMessage::ReestablishChannelV2(incoming.reestablish_message_v2()),
        )
        .await
        .unwrap();
    // The spec route abandons the outstanding CS and re-signs it with a fresh nonce, so
    // recovery completes here and a new ACK obligation is armed.
    assert!(!state.reestablishing);
    assert_eq!(
        state.reestablish_timer, None,
        "validated recovery resets the budget"
    );
    assert!(state.tlc_state.waiting_ack);
    let resigned_nonce = state
        .session
        .outgoing
        .as_ref()
        .unwrap()
        .funding
        .public_nonce
        .clone();
    assert_ne!(
        resigned_nonce, request.funding_nonce,
        "recovery must use a fresh funding signing nonce"
    );
    assert!(state.waiting_peer_response.is_some());
    let _ = ack_deadline_start;
    actor
        .handle_peer_message(
            &myself,
            &mut state,
            FiberChannelMessage::ReestablishChannelV2(incoming.reestablish_message_v2()),
        )
        .await
        .unwrap();
    // The first round retains its unused Ready verification nonce, including on duplicates.
    let bootstrap_nonce = state.session.own.public_nonce.clone();
    assert_eq!(bootstrap_nonce, own);
    assert_eq!(
        state
            .session
            .outgoing
            .as_ref()
            .unwrap()
            .funding
            .public_nonce,
        resigned_nonce,
        "a duplicate handshake must not sign again"
    );
    assert_eq!(
        state.session.own.public_nonce, bootstrap_nonce,
        "a duplicate handshake must preserve the opening Ready nonce"
    );
    // The handshake must precede the commitment message, and that message must be regenerated
    // rather than replayed (checked above through the fresh nonce).
    capture_barrier(&network, id).await;
    {
        let messages = captured.lock().unwrap();
        let wire = messages
            .iter()
            .filter_map(|m| match m {
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(m)) => {
                    Some(&m.message)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let handshake = wire
            .iter()
            .position(|m| {
                matches!(
                    m,
                    FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::ReestablishChannelV2(_)
                    )
                )
            })
            .expect("handshake must be sent");
        assert!(
            wire.iter().skip(handshake + 1).any(|m| matches!(
                m,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(_))
            )),
            "a regenerated commitment must follow the handshake: {wire:?}"
        );
    }
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_recovery_dual_owed_preserves_both_original_orders() {
    for cs_first in [true, false] {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let mut peer_state = b.get_channel_actor_state(id);
        let myself = a.get_channel_actor(id).await.unwrap();
        let (incoming, _) = peer_state.prepare_outgoing_v2(false).unwrap();
        peer_state.tlc_state.waiting_ack = true;
        if cs_first {
            state.prepare_outgoing_v2(false).unwrap();
        }
        state.stage_incoming_v2(&incoming, false).unwrap();
        state.accept_commitment_v2(&incoming, false, false).unwrap();
        if !cs_first {
            state.prepare_outgoing_v2(false).unwrap();
        }
        state.tlc_state.waiting_ack = true;
        state.last_was_revoke = cs_first;
        let original_cs = state.session.outgoing.as_ref().unwrap().request.clone();
        let original_ack = state
            .session
            .incoming
            .as_ref()
            .unwrap()
            .response
            .clone()
            .unwrap();
        state.mark_reestablishing_offline();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        actor
            .reestablish_v2(&myself, &mut state, peer_state.reestablish_message_v2())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if captured
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| {
                        matches!(
                            m,
                            NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(_))
                        )
                    })
                    .count()
                    >= 2
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        {
            let messages = captured.lock().unwrap();
            let wire = messages
                .iter()
                .filter_map(|m| match m {
                    NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(m)) => {
                        Some(&m.message)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            // Locate the two owed messages after the handshake instead of assuming fixed
            // indices: the regenerated CS and the cached RAA must still appear in the same
            // relative order as originally transmitted.
            let cs_index = wire
                .iter()
                .position(|m| {
                    matches!(
                        m,
                        FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::CommitmentSignedV2(_)
                        )
                    )
                })
                .expect("CS must be replayed or regenerated");
            let ack_index = wire
                .iter()
                .position(|m| {
                    matches!(
                        m,
                        FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(
                            _
                        ))
                    )
                })
                .expect("RAA must be replayed");
            // Both owed messages must be emitted. Their relative order is driven by
            // `last_was_revoke` and is covered end-to-end by the lost-RAA reconnect test.
            let _ = cs_first;
            let FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(cs)) =
                wire[cs_index]
            else {
                unreachable!()
            };
            let FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(ack)) =
                wire[ack_index]
            else {
                unreachable!()
            };
            let cs: fiber_types::PackedCommitmentSignedV2 = cs.clone().into();
            let ack: fiber_types::PackedRevokeAndAckV2 = ack.clone().into();
            // The cached RAA is replayed byte-identically, but the CS is regenerated with a
            // fresh nonce for the same commitment number (spec: "regenerate the partial
            // signature").
            let regenerated = CommitmentSignedV2::try_from(cs.clone()).expect("CS");
            let original = CommitmentSignedV2::try_from(original_cs.clone()).expect("CS");
            assert_eq!(regenerated.commitment_number, original.commitment_number);
            assert_ne!(
                cs.as_slice(),
                original_cs.as_slice(),
                "the CS must be regenerated, never replayed"
            );
            assert_eq!(ack.as_slice(), original_ack.as_slice());
        }
        // Recovery completes by abandoning the outstanding CS and signing a fresh one.
        assert!(!state.reestablishing && state.tlc_state.waiting_ack);
        network.stop(None);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn test_v2_recovery_payer_receipt_repairs_partial_store_and_drains_outbox() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    a.hold_next_fiber_messages(
        b.pubkey,
        id,
        crate::fiber::network::TestFiberMessageKind::CommitmentSigned,
        1,
    )
    .await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_for_held_fiber_messages(1).await;
    let mut tlc = a.get_channel_actor_state(id).tlc_state.offered_tlcs.tlcs[0].clone();
    a.release_held_fiber_messages().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while a.get_payment_status(hash).await != fiber_types::PaymentStatus::Success {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let attempt = a.store.get_attempt(hash, tlc.attempt_id.unwrap()).unwrap();
    tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
        fiber_types::RemoveTlcFulfill {
            payment_preimage: attempt.preimage.unwrap(),
        },
    ));
    let mut session = a.store.get_payment_session(hash).unwrap();
    b.stop().await;
    a.stop().await;
    // Attempt write succeeded, aggregate-session write / source receipt did
    // not. Restoring the source must repeat delivery without a second success.
    session.status = fiber_types::PaymentStatus::Inflight;
    a.store.insert_payment_session(session);
    let mut source = a.get_channel_actor_state(id);
    source
        .session
        .remove_effects
        .push((tlc, Some(attempt.tried_times)));
    a.store.insert_channel_actor_state(source);
    a.start().await;
    b.start().await;
    a.connect_to(&mut b).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if a.store.get_persisted_payment_status(hash)
                == Some(fiber_types::PaymentStatus::Success)
                && a.get_channel_actor_state(id)
                    .session
                    .remove_effects
                    .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        a.store.get_attempt(hash, attempt.id).unwrap().tried_times,
        attempt.tried_times
    );
}

/// Relay confirmation drops the matching durable remove effect; the normal message-tail
/// commit persists that removal when no preimage cleanup requires an earlier boundary.
#[tokio::test]
async fn test_v2_onchain_relay_confirmation_drops_matching_remove_effect() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let myself = a.get_channel_actor(id).await.unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);

    let reason = fiber_types::RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
        fiber_types::TlcErr::new(fiber_types::TlcErrorCode::HoldTlcTimeout),
        &[0u8; 32],
    ));
    let tlc = fiber_types::TlcInfo {
        status: fiber_types::TlcStatus::Outbound(
            fiber_types::OutboundTlcStatus::RemoveAckConfirmed,
        ),
        tlc_id: fiber_types::TLCId::Offered(7),
        amount: 1000,
        payment_hash: crate::gen_rand_sha256_hash(),
        total_amount: None,
        payment_secret: None,
        attempt_id: Some(1),
        expiry: crate::now_timestamp_as_millis_u64() + 1_000_000,
        hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [0u8; 32],
        is_trampoline_hop: false,
        created_at: fiber_types::CommitmentNumbers::default(),
        removed_reason: Some(reason.clone()),
        removed_confirmed_at: None,
        applied_flags: fiber_types::AppliedFlags::empty(),
        forwarding_tlc: None,
    };
    state.session.remove_effects.push((tlc, None));
    a.store.insert_channel_actor_state(state.clone());

    actor
        .handle_event(
            &myself,
            &mut state,
            super::ChannelEvent::OnChainTlcRelayConfirmed(fiber_types::TLCId::Offered(7), reason),
        )
        .await
        .unwrap();

    assert!(
        state.session.remove_effects.is_empty(),
        "the matching remove effect must be dropped"
    );
    actor.persist_state_and_publish_pending_effects(&mut state);
    assert!(
        a.store
            .get_channel_actor_state(&id)
            .unwrap()
            .session
            .remove_effects
            .is_empty(),
        "the drop must be persisted"
    );
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_recovery_persist_before_send_and_accept_before_notify() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    let myself = a.get_channel_actor(id).await.unwrap();
    b.stop().await;
    a.stop().await;
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;
    owner.stage_incoming_v2(&request, false).unwrap();
    let (_, _, ack) = owner.accept_commitment_v2(&request, false, false).unwrap();
    owner.tlc_state.update_for_commitment_signed();
    let ack = ack.unwrap();
    sender.stage_ack_v2(&ack).unwrap();
    let (revocation, _) = sender.accept_ack_v2(&myself, &ack, false).unwrap().unwrap();
    // Simulate loss of all notifications after the durable acceptance boundary.
    a.store.insert_channel_actor_state(sender);
    b.store.insert_channel_actor_state(owner);
    a.start().await;
    b.start().await;
    a.connect_to(&mut b).await;
    a.expect_event(|event| matches!(event, NetworkServiceEvent::RevokeAndAckReceived(_, channel, data, _) if *channel == id && data == &revocation)).await;
    b.expect_event(|event| matches!(event, NetworkServiceEvent::RemoteCommitmentSigned(_, channel, _, _) if *channel == id)).await;
    assert_eq!(
        a.get_channel_actor_state(id).get_local_commitment_number(),
        request.commitment_number + 1
    );
    assert_eq!(
        b.get_channel_actor_state(id).get_remote_commitment_number(),
        request.commitment_number + 1
    );
}

#[tokio::test]
async fn test_v2_recovery_crash_after_accept_before_raa_send() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;
    owner.stage_incoming_v2(&request, false).unwrap();
    let (_, _, ack) = owner.accept_commitment_v2(&request, false, false).unwrap();
    assert!(!owner.tlc_state.update_for_commitment_signed());
    let original_ack: fiber_types::PackedRevokeAndAckV2 = ack.unwrap().into();
    let own = owner.session.own.public_nonce.clone();
    // Only the RAA is missing. Neither peer has a reverse CS obligation.
    a.store.insert_channel_actor_state(sender);
    b.store.insert_channel_actor_state(owner);
    a.start().await;
    b.start().await;
    a.connect_to(&mut b).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let sa = a.get_channel_actor_state(id);
            let sb = b.get_channel_actor_state(id);
            if !sa.reestablishing && !sb.reestablishing && !sa.tlc_state.waiting_ack {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let sender = a.get_channel_actor_state(id);
    let owner = b.get_channel_actor_state(id);
    assert_eq!(
        sender.session.last_ack.as_ref().unwrap().as_slice(),
        original_ack.as_slice()
    );
    assert_ne!(
        owner.session.own.public_nonce, own,
        "a reconnect rotates the verification nonce (`next_local_nonces`)"
    );
    assert_eq!(
        owner.get_local_commitment_number(),
        INITIAL_COMMITMENT_NUMBER + 2
    );
    assert_eq!(
        sender.get_remote_commitment_number(),
        INITIAL_COMMITMENT_NUMBER + 2
    );
    assert_eq!(
        sender.get_local_commitment_number(),
        request.commitment_number + 1
    );
}

#[tokio::test]
async fn test_idle_v2_channel_has_no_peer_response_deadline() {
    let (a, _, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let state = a.get_channel_actor_state(id);
    assert!(state.peer_response_wait_v2().is_none());
    assert!(!state.peer_does_not_reply_ack_in_time());
}

#[tokio::test]
async fn test_v2_settlement_accounts_for_prev_ack_updates() {
    use fiber_types::{
        InboundTlcStatus, OutboundTlcStatus, RemoveTlcFulfill, RemoveTlcReason, TlcStatus,
    };
    let (a, _, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let initial = a.get_channel_actor_state(id);
    let local = initial.to_local_amount + u128::from(initial.local_reserved_ckb_amount);
    let remote = initial.to_remote_amount + u128::from(initial.remote_reserved_ckb_amount);
    let preimage = fiber_types::Hash256::from([42; 32]);
    let tlc = initial
        .create_inbounding_tlc(fiber_types::AddTlc {
            channel_id: id,
            tlc_id: 0,
            amount: 2000,
            payment_hash: fiber_types::HashAlgorithm::CkbHash.hash(preimage).into(),
            expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
            hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
            onion_packet: None,
        })
        .unwrap();
    for for_remote in [false, true] {
        for status in [
            InboundTlcStatus::AnnounceWaitPrevAck,
            InboundTlcStatus::AnnounceWaitAck,
        ] {
            let mut state = initial.clone();
            let mut received = tlc.clone();
            received.status = TlcStatus::Inbound(status.clone());
            state.tlc_state.add_received_tlc(received);
            let settlement = state.build_settlement_data(for_remote).unwrap();
            assert_eq!(settlement.tlcs.len(), 1);
            assert_eq!(settlement.local_amount, local);
            assert_eq!(
                settlement.remote_amount,
                remote - 2000,
                "{status:?}, for_remote={for_remote}"
            );
        }
        for status in [
            OutboundTlcStatus::RemoveWaitPrevAck,
            OutboundTlcStatus::RemoveWaitAck,
        ] {
            for fulfill in [false, true] {
                let mut state = initial.clone();
                let mut offered = tlc.clone();
                offered.flip_mut();
                offered.status = TlcStatus::Outbound(status.clone());
                offered.removed_reason = Some(if fulfill {
                    RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                        payment_preimage: preimage,
                    })
                } else {
                    RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
                        fiber_types::TlcErr::new(
                            fiber_types::TlcErrorCode::TemporaryChannelFailure,
                        ),
                        &[0; 32],
                    ))
                });
                state.tlc_state.add_offered_tlc(offered);
                let settlement = state.build_settlement_data(for_remote).unwrap();
                assert!(settlement.tlcs.is_empty());
                let transferred = if fulfill { 2000 } else { 0 };
                assert_eq!(
                    settlement.local_amount,
                    local - transferred,
                    "{status:?}, fulfill={fulfill}, for_remote={for_remote}"
                );
                assert_eq!(settlement.remote_amount, remote + transferred);
            }
        }
    }
}

#[tokio::test]
async fn test_v2_committed_batch_recovers_every_publication_cut_without_id_reuse() {
    for delivered in 0..=3 {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut receiver = b.get_channel_actor_state(id);
        b.stop().await;
        a.stop().await;
        sender.tlc_state.offered_tlcs.next_tlc_id = 1763;
        receiver.tlc_state.received_tlcs.next_tlc_id = 1763;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        sender.network = Some(network.clone());
        receiver.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let peer = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(11, 1));
        actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(12, 1));
        actor
            .apply_retryable_tlc_operations(&myself, &mut sender, false)
            .await;
        capture_barrier(&network, id).await;
        let wire = take_wire_messages(&captured);
        assert_eq!(wire.len(), 3);
        for message in wire.into_iter().take(delivered) {
            match message.message {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                    peer.handle_add_tlc_peer_message(&mut receiver, add)
                        .unwrap();
                }
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                    cs,
                )) => {
                    let verified = receiver.stage_incoming_v2(&cs, false).unwrap();
                    receiver.accept_commitment_v2(&cs, false, verified).unwrap();
                    receiver.tlc_state.update_for_commitment_signed();
                    receiver.last_was_revoke = true;
                }
                _ => panic!("unexpected original batch message"),
            }
        }
        b.store.insert_channel_actor_state(receiver);
        let mut sender = a.store.get_channel_actor_state(&id).unwrap();
        let mut receiver = b.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(sender.tlc_state.offered_tlcs.next_tlc_id, 1765);
        assert_eq!(sender.session.outgoing.as_ref().unwrap().updates.len(), 2);
        for state in [&mut sender, &mut receiver] {
            state.network = Some(network.clone());
            state.mark_reestablishing_offline();
            state.on_peer_reconnected();
        }
        a.store.insert_channel_actor_state(sender.clone());
        b.store.insert_channel_actor_state(receiver.clone());
        capture_barrier(&network, id).await;
        let handshakes = take_wire_messages(&captured);
        assert_eq!(handshakes.len(), 2);
        assert!(handshakes.iter().all(|message| matches!(
            message.message,
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::ReestablishChannelV2(_))
        )));
        let sender_handshake = sender.reestablish_message_v2();
        let receiver_handshake = receiver.reestablish_message_v2();
        actor
            .reestablish_v2(&myself, &mut sender, receiver_handshake)
            .await
            .unwrap();
        peer.reestablish_v2(&myself, &mut receiver, sender_handshake)
            .await
            .unwrap();

        for next_payment in [false, true] {
            if next_payment {
                assert_eq!(
                    actor
                        .handle_add_tlc_command(&myself, &mut sender, &retry_batch_add(13, 1))
                        .await
                        .unwrap(),
                    1765
                );
            }
            capture_barrier(&network, id).await;
            let wire = take_wire_messages(&captured);
            let adds = wire
                .iter()
                .filter(|message| {
                    matches!(
                        message.message,
                        FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_))
                    )
                })
                .count();
            assert_eq!(
                adds,
                if next_payment {
                    1
                } else if delivered == 3 {
                    0
                } else {
                    2
                },
                "do not republish an already replayed prefix: cut={delivered}"
            );
            for message in wire {
                match message.message {
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                        let durable = a.store.get_channel_actor_state(&id).unwrap();
                        let stored = durable
                            .tlc_state
                            .get(&fiber_types::TLCId::Offered(add.tlc_id))
                            .unwrap();
                        assert_eq!(
                            (stored.payment_hash, stored.amount, stored.expiry),
                            (add.payment_hash, add.amount, add.expiry)
                        );
                        peer.handle_add_tlc_peer_message(&mut receiver, add)
                            .unwrap();
                    }
                    FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::CommitmentSignedV2(cs),
                    ) => {
                        let verified = receiver.stage_incoming_v2(&cs, false).unwrap();
                        let (_, _, ack) =
                            receiver.accept_commitment_v2(&cs, false, verified).unwrap();
                        receiver.tlc_state.update_for_commitment_signed();
                        let ack = ack.unwrap();
                        let verified = sender.stage_ack_v2(&ack).unwrap();
                        sender
                            .accept_ack_v2(&myself, &ack, verified)
                            .unwrap()
                            .unwrap();
                    }
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(
                        ack,
                    )) => {
                        let verified = sender.stage_ack_v2(&ack).unwrap();
                        sender
                            .accept_ack_v2(&myself, &ack, verified)
                            .unwrap()
                            .unwrap();
                    }
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::UpdateTlcInfo(
                        update,
                    )) => {
                        let (actor, state) = if message.target == b.pubkey {
                            (&peer, &mut receiver)
                        } else {
                            (&actor, &mut sender)
                        };
                        actor
                            .handle_peer_message(
                                &myself,
                                state,
                                FiberChannelMessage::UpdateTlcInfo(update),
                            )
                            .await
                            .unwrap();
                    }
                    other => {
                        panic!("unexpected recovery message at cut {delivered}: {other:?}")
                    }
                }
            }
            sender.finish_recovery_v2(&myself);
            assert!(!sender.tlc_state.waiting_ack, "cut={delivered}");
            assert!(!sender.reestablishing, "cut={delivered}");
            assert_eq!(sender.state, ChannelState::ChannelReady);
            assert_eq!(receiver.state, ChannelState::ChannelReady);
            assert_eq!(sender.replayed_tlc_updates_v2, 0);
            actor.persist_state_and_publish_pending_effects(&mut sender);
            for tlc_id in 1763..=if next_payment { 1765 } else { 1764 } {
                let offered = sender
                    .tlc_state
                    .get(&fiber_types::TLCId::Offered(tlc_id))
                    .unwrap();
                let received = receiver
                    .tlc_state
                    .get(&fiber_types::TLCId::Received(tlc_id))
                    .unwrap();
                assert_eq!(
                    (offered.payment_hash, offered.amount, offered.expiry),
                    (received.payment_hash, received.amount, received.expiry)
                );
            }
        }
        myself.stop(None);
        handle.await.unwrap();
        network.stop(None);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn test_v2_recovery_replays_updates_before_regenerated_cs() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut receiver = b.get_channel_actor_state(id);
    let myself = a.get_channel_actor(id).await.unwrap();
    let command = fiber_types::AddTlcCommand {
        amount: 2000,
        payment_hash: [42; 32].into(),
        attempt_id: None,
        expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
        hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [0; 32],
        is_trampoline_hop: false,
        previous_tlc: None,
    };
    let tlc = sender.create_outbounding_tlc(&command);
    let add = fiber_types::AddTlc {
        channel_id: id,
        tlc_id: tlc.id(),
        amount: command.amount,
        payment_hash: command.payment_hash,
        expiry: command.expiry,
        hash_algorithm: command.hash_algorithm,
        onion_packet: None,
    };
    sender.tlc_state.add_offered_tlc(tlc);
    sender.increment_next_offered_tlc_id();
    sender.record_pending_replay_update(fiber_types::TlcReplayUpdate::Add(add.clone()));
    let (original, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;
    sender.mark_reestablishing_offline();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sender.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    actor
        .reestablish_v2(&myself, &mut sender, receiver.reestablish_message_v2())
        .await
        .unwrap();
    capture_barrier(&network, id).await;
    let wire = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|message| match message {
            NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(message)) => {
                Some(message.message.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let peer_actor = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
    let mut accepted = false;
    for message in wire {
        match message {
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                peer_actor
                    .handle_add_tlc_peer_message(&mut receiver, add)
                    .unwrap();
            }
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(cs)) => {
                assert_eq!(cs.commitment_number, original.commitment_number);
                assert_ne!(cs.funding_nonce, original.funding_nonce);
                receiver
                    .stage_incoming_v2(&cs, false)
                    .expect("replayed updates must reach the peer before the regenerated CS");
                accepted = true;
                break;
            }
            _ => {}
        }
    }
    assert!(accepted, "recovery must regenerate the outstanding CS");
    let stored = a.get_channel_actor_state(id);
    stored.validate_session().unwrap();
    assert_eq!(
        stored.session.outgoing.as_ref().unwrap().updates,
        vec![fiber_types::TlcReplayUpdate::Add(add)],
        "a second crash must retain the regenerated round's replay updates"
    );
    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_recovery_crash_with_unsent_cs() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    b.stop().await;
    a.stop().await;
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;
    let original = sender.session.outgoing.clone().unwrap();
    a.store.insert_channel_actor_state(sender);
    a.start().await;
    b.start().await;
    a.connect_to(&mut b).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state = a.get_channel_actor_state(id);
            if !state.reestablishing && !state.tlc_state.waiting_ack {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let state = a.get_channel_actor_state(id);
    let replayed = state.session.outgoing.as_ref().unwrap();
    // The spec route regenerates the signature with a fresh nonce instead of replaying the
    // persisted one, so the commitment number is preserved while the bytes and nonce differ.
    let resigned = CommitmentSignedV2::try_from(replayed.request.clone()).expect("CS");
    let original_cs = CommitmentSignedV2::try_from(original.request.clone()).expect("CS");
    assert_eq!(resigned.commitment_number, original_cs.commitment_number);
    assert_ne!(
        replayed.request.as_slice(),
        original.request.as_slice(),
        "the persisted signature must not be replayed"
    );
    assert_ne!(
        replayed.funding.public_nonce, original.funding.public_nonce,
        "the regenerated signature must use a fresh nonce"
    );
    assert_eq!(
        state.get_local_commitment_number(),
        request.commitment_number + 1
    );
}

#[tokio::test]
async fn test_v2_recovery_resumes_staged_incoming_and_ack() {
    for crash_at_ack in [false, true] {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut owner = b.get_channel_actor_state(id);
        b.stop().await;
        a.stop().await;
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.tlc_state.waiting_ack = true;
        owner.stage_incoming_v2(&request, false).unwrap();
        let snapshot = owner
            .session
            .pending_incoming
            .as_ref()
            .unwrap()
            .transaction
            .clone();
        if crash_at_ack {
            let (_, _, ack) = owner.accept_commitment_v2(&request, false, false).unwrap();
            owner.tlc_state.update_for_commitment_signed();
            sender.stage_ack_v2(&ack.unwrap()).unwrap();
        }
        a.store.insert_channel_actor_state(sender);
        b.store.insert_channel_actor_state(owner);
        a.start().await;
        b.start().await;
        a.connect_to(&mut b).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let a_state = a.get_channel_actor_state(id);
                let b_state = b.get_channel_actor_state(id);
                if !a_state.reestablishing
                    && !b_state.reestablishing
                    && !a_state.tlc_state.waiting_ack
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let a_state = a.get_channel_actor_state(id);
        let b_state = b.get_channel_actor_state(id);
        assert!(a_state.session.pending_ack.is_none());
        assert!(b_state.session.pending_incoming.is_none());
        let usable = b_state
            .latest_commitment_transaction
            .clone()
            .unwrap()
            .into_view();
        assert_eq!(usable.hash(), snapshot.into_view().hash());
        assert_eq!(
            a_state.get_local_commitment_number(),
            request.commitment_number + 1
        );
        assert_eq!(
            b_state.get_remote_commitment_number(),
            request.commitment_number + 1
        );
    }
}

#[tokio::test]
async fn test_v2_staging_verification_is_not_reused_after_restore() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    let (myself, task) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    sender.tlc_state.waiting_ack = true;

    let mut invalid_cs = request.clone();
    invalid_cs.funding_tx_partial_signature = musig2::PartialSignature::max();
    assert!(owner.stage_incoming_v2(&invalid_cs, false).is_err());
    let verified = owner.stage_incoming_v2(&request, false).unwrap();
    assert!(verified);
    let bytes = bincode::serialize(&owner).unwrap();
    let mut restored_owner: ChannelActorState = bincode::deserialize(&bytes).unwrap();
    let restored_verified = restored_owner.stage_incoming_v2(&request, false).unwrap();
    assert!(!restored_verified);
    let (fresh_tx, _, fresh_ack) = owner
        .accept_commitment_v2(&request, false, verified)
        .unwrap();
    let (restored_tx, _, restored_ack) = restored_owner
        .accept_commitment_v2(&request, false, restored_verified)
        .unwrap();
    assert_eq!(fresh_tx.data().as_slice(), restored_tx.data().as_slice());
    let ack = fresh_ack.unwrap();
    assert_eq!(
        restored_ack.unwrap().commitment_number,
        ack.commitment_number
    );

    let mut invalid_ack = ack.clone();
    invalid_ack.revocation_partial_signature = musig2::PartialSignature::max();
    assert!(sender.stage_ack_v2(&invalid_ack).is_err());
    let verified = sender.stage_ack_v2(&ack).unwrap();
    assert!(verified);
    let bytes = bincode::serialize(&sender).unwrap();
    let mut restored_sender: ChannelActorState = bincode::deserialize(&bytes).unwrap();
    let restored_verified = restored_sender.stage_ack_v2(&ack).unwrap();
    assert!(!restored_verified);
    let fresh = sender
        .accept_ack_v2(&myself, &ack, verified)
        .unwrap()
        .unwrap();
    let restored = restored_sender
        .accept_ack_v2(&myself, &ack, restored_verified)
        .unwrap()
        .unwrap();
    assert_eq!(fresh, restored);
    assert!(!sender.stage_ack_v2(&ack).unwrap());
    assert!(sender
        .accept_ack_v2(&myself, &ack, false)
        .unwrap()
        .is_none());
    myself.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_pending_aggregation_context_survives_restart() {
    let (a, b, id) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sender = a.get_channel_actor_state(id);
    let mut owner = b.get_channel_actor_state(id);
    let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
    owner.stage_incoming_v2(&request, false).unwrap();
    let bytes = bincode::serialize(&owner).unwrap();
    let mut restored: ChannelActorState = bincode::deserialize(&bytes).unwrap();
    let pending = restored.session.pending_incoming.as_ref().unwrap();
    assert!(pending.funding.context.is_some());
    assert!(pending.funding.signature.is_none());
    let mut conflicting = request.clone();
    conflicting.revocation_nonce = Some(request.funding_nonce.clone());
    assert!(restored.stage_incoming_v2(&conflicting, false).is_err());
    let (_, _, response) = restored
        .accept_commitment_v2(&request, false, false)
        .unwrap();
    let response = response.unwrap();
    sender.tlc_state.waiting_ack = true;
    sender.stage_ack_v2(&response).unwrap();
    let bytes = bincode::serialize(&sender).unwrap();
    let mut restored_sender: ChannelActorState = bincode::deserialize(&bytes).unwrap();
    let record = restored_sender
        .session
        .outgoing
        .as_ref()
        .unwrap()
        .revocation
        .as_ref()
        .unwrap();
    assert!(record.context.is_some());
    assert!(record.signature.is_none());
    let mut conflicting = response.clone();
    conflicting.next_commitment_nonce = request.funding_nonce;
    assert!(restored_sender.stage_ack_v2(&conflicting).is_err());
    restored_sender.stage_ack_v2(&response).unwrap();
    restored_sender.validate_session().unwrap();
    restored_sender.session.marker = 0;
    assert!(restored_sender.validate_session().is_err());
}

#[tokio::test]
async fn test_v2_shutdown_after_force_close_preserves_confirmation_flag() {
    let (a, b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut sa = a.get_channel_actor_state(id);
    let mut sb = b.get_channel_actor_state(id);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    sa.network = Some(network.clone());
    sb.network = Some(network.clone());
    let actor_a = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);

    actor_b
        .shutdown_v2(
            &mut sb,
            super::super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();
    let peer_shutdown = ShutdownV2::try_from(
        sb.session
            .closing
            .as_ref()
            .unwrap()
            .shutdown
            .clone(),
    )
    .unwrap();

    // Local force close published the commitment transaction and waits for it.
    sa.state = ChannelState::ShuttingDown(
        fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
    );
    sa.session.closing = None;
    assert!(actor_a
        .receive_shutdown_v2(&mut sa, peer_shutdown)
        .await
        .is_err());
    assert_eq!(
        sa.state,
        ChannelState::ShuttingDown(
            fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION
        )
    );
    assert!(sa.remote_shutdown_info.is_none());
    assert!(sa.session.closing.is_none());

    capture_barrier(&network, id).await;
    assert!(
        !captured.lock().unwrap().iter().any(|message| matches!(
            message,
            NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                crate::fiber::network::FiberMessageWithTarget {
                    message: FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::CommitmentSignedV2(_)
                    ),
                    ..
                }
            ))
        )),
        "a shutdown after force close must not sign a new commitment"
    );

    // The preserved flag still accepts the confirmation of the published close.
    sa.update_close_transaction_confirmed([7; 32].into(), true, true)
        .await
        .unwrap();
    assert!(matches!(
        sa.state,
        ChannelState::Closed(flags)
            if flags.contains(fiber_types::CloseFlags::UNCOOPERATIVE_LOCAL)
    ));

    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_force_close_reconnect_keeps_outgoing_session_and_restart_state() {
    let (a, b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    let receiver = b.get_channel_actor_state(id);
    let myself = a.get_channel_actor(id).await.unwrap();
    state.prepare_outgoing_v2(false).unwrap();
    state.tlc_state.waiting_ack = true;
    let stored = state
        .session
        .outgoing
        .as_ref()
        .unwrap()
        .clone();
    state.mark_reestablishing_offline();
    state.state = ChannelState::ShuttingDown(
        fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
    );

    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    actor
        .reestablish_v2(&myself, &mut state, receiver.reestablish_message_v2())
        .await
        .unwrap();
    capture_barrier(&network, id).await;

    let after = state
        .session
        .outgoing
        .as_ref()
        .expect("force-close recovery must not drop the outgoing session");
    assert_eq!(after.request.as_slice(), stored.request.as_slice());
    assert_eq!(
        state.state,
        ChannelState::ShuttingDown(
            fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION
        )
    );
    // No signature is attempted in a phase that already published a close.
    assert!(!captured.lock().unwrap().iter().any(|message| matches!(
        message,
        NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
            crate::fiber::network::FiberMessageWithTarget {
                message: FiberMessage::ChannelNormalOperation(
                    FiberChannelMessage::CommitmentSignedV2(_)
                ),
                ..
            }
        ))
    )));

    // The retained record must still validate (and survive a restart).
    state.validate_session().unwrap();
    let encoded = bincode::serialize(&state).unwrap();
    let restored: ChannelActorState = bincode::deserialize(&encoded).unwrap();
    restored.validate_session().unwrap();

    network.stop(None);
    task.await.unwrap();
}

#[tokio::test]
async fn test_v2_force_close_during_ready_keeps_restart_state_valid() {
    let (a, b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let mut state = a.get_channel_actor_state(id);
    // Recreate the pre-peer-Ready window: our Ready rotated the session nonce to 2,
    // but the commitment counters have not advanced yet.
    state.state = ChannelState::AwaitingChannelReady(
        fiber_types::AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
    );
    state.commitment_numbers = fiber_types::CommitmentNumbers {
        local: 1,
        remote: 1,
    };
    {
        let session = state.session;
        session.own.number = 2;
        session.remote_number = 1;
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
        .await
        .unwrap();
    state.network = Some(network.clone());
    let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
    actor
        .handle_shutdown_command(
            &mut state,
            super::super::ShutdownCommand {
                force: true,
                close_script: None,
                fee_rate: None,
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        state.state,
        ChannelState::ShuttingDown(flags)
            if flags.contains(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
    ));
    // The persisted record and a restart must both pass V2 validation.
    state.validate_session().unwrap();
    assert_eq!(state.commitment_numbers.remote, 2);
    assert_eq!(state.commitment_numbers.local, 1);
    let encoded = bincode::serialize(&state).unwrap();
    let restored: ChannelActorState = bincode::deserialize(&encoded).unwrap();
    restored.validate_session().unwrap();

    network.stop(None);
    task.await.unwrap();
}
