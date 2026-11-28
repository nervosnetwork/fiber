use fiber_types::AttemptStatus;
use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::fiber::{
    channel::ChannelActorStateStore, graph::NetworkGraphStateStore, network::TestFiberMessageKind,
    FiberActorEvent, FiberActorRef,
};
use crate::tests::test_utils::create_nodes_with_established_channel;

use super::{
    PaymentActor, PaymentActorArguments, PaymentActorMessage, PaymentActorState, PaymentSessionExt,
};

struct RetryMailbox;

#[async_trait::async_trait]
impl Actor for RetryMailbox {
    type Msg = PaymentActorMessage;
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

struct DropAdmissionReply;

#[async_trait::async_trait]
impl Actor for DropAdmissionReply {
    type Msg = crate::fiber::NetworkActorMessage;
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
        // Drop the reply without reporting acceptance or rejection.
        Ok(())
    }
}

struct RejectFirstHop;

#[async_trait::async_trait]
impl Actor for RejectFirstHop {
    type Msg = crate::fiber::NetworkActorMessage;
    type State = super::Pubkey;
    type Arguments = super::Pubkey;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        payer: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(payer)
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        payer: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        use crate::fiber::{FiberActorCommand, FiberActorMessage, NetworkActorMessage};

        if let NetworkActorMessage::Fiber(FiberActorMessage::Command(
            FiberActorCommand::SendPaymentOnionPacket(command, reply),
        )) = message
        {
            let outpoint = super::OutPoint::new(
                command.peeled_onion_packet.current.funding_tx_hash.into(),
                0,
            );
            reply.send(Err(super::TlcErr::new_channel_fail(
                super::TlcErrorCode::UnknownNextPeer,
                *payer,
                outpoint,
                None,
            )))?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_mpp_rejected_first_shard_marks_payment_failed() {
    use secp256k1::SECP256K1;

    use crate::fiber::PaymentStatus;
    use crate::gen_rand_sha256_hash;
    use crate::invoice::{Currency, InvoiceBuilder};
    use crate::tests::test_utils::{create_n_nodes_network, MIN_RESERVED_CKB};

    const PART_CAPACITY: u128 = 10_000_000_000;
    // The invoice needs both channels, so the first hop is rejected with a
    // second shard still waiting in the dispatch loop.
    let (nodes, _) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + PART_CAPACITY, MIN_RESERVED_CKB)); 2],
        2,
    )
    .await;
    let [payer, payee] = nodes.try_into().expect("two nodes");
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(PART_CAPACITY + PART_CAPACITY / 2))
        .payee_pub_key(payee.pubkey.into())
        .allow_mpp(true)
        .payment_preimage(gen_rand_sha256_hash())
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &payee.private_key.0))
        .unwrap();
    let request = super::SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        max_parts: Some(2),
        ..Default::default()
    }
    .build_send_payment_data(Currency::Fibd)
    .unwrap();
    let hash = request.payment_hash;
    let (network, network_task) = Actor::spawn(None, RejectFirstHop, payer.pubkey)
        .await
        .unwrap();
    let actor = PaymentActor::new(
        payer.store.clone(),
        payer.network_graph.clone(),
        network.clone().into(),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (mailbox, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let response = actor
        .send_payment_with_payment_data(mailbox.clone(), &mut state, request)
        .await;
    mailbox.stop(None);
    task.await.unwrap();
    network.stop(None);
    network_task.await.unwrap();

    assert!(response.is_err());
    let attempts = payer.store.get_attempts(hash);
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, AttemptStatus::Failed);
    assert_eq!(
        payer.store.get_persisted_payment_status(hash),
        Some(PaymentStatus::Failed)
    );
}

#[tokio::test]
async fn test_review_lost_admission_returns_trackable_payment() {
    use crate::fiber::{NetworkActorMessage, PaymentStatus};

    let (a, b, _) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let request = super::SendPaymentCommand {
        target_pubkey: Some(b.pubkey),
        amount: Some(1000),
        keysend: Some(true),
        ..Default::default()
    }
    .build_send_payment_data(crate::invoice::Currency::Fibd)
    .unwrap();
    let hash = request.payment_hash;
    let (network, network_task) = Actor::spawn(None, DropAdmissionReply, ()).await.unwrap();
    let actor = PaymentActor::new(
        a.store.clone(),
        a.network_graph.clone(),
        FiberActorRef::from_network(&network),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (myself, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let response = actor
        .send_payment_with_payment_data(myself, &mut state, request)
        .await;
    task.await.unwrap();
    network.stop(None);
    network_task.await.unwrap();
    let response =
        response.expect("a retained payment must return a trackable response, not an RPC failure");
    assert_eq!(response.payment_hash, hash);
    assert_eq!(response.status, PaymentStatus::Created);
    let attempts = a.store.get_attempts(hash);
    assert_eq!(attempts.len(), 1);
    let id = attempts[0].id;
    a.network_actor
        .send_message(NetworkActorMessage::new_event(
            FiberActorEvent::RetrySendPayment(hash.into(), Some(id)),
        ))
        .unwrap();
    a.wait_until_success(hash).await;
    a.wait_until_payment_finished(hash).await;
    a.wait_until_no_inflight_payments().await;
    assert_eq!(a.store.get_attempts(hash).len(), 1);
    assert_eq!(a.store.get_attempt(hash, id).unwrap().tried_times, 1);
}

#[tokio::test]
async fn test_review_rerouted_lost_admission_recovers_on_current_channel() {
    use crate::fiber::{NetworkActorMessage, PaymentStatus};

    let (a, b, channel_id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let request = super::SendPaymentCommand {
        target_pubkey: Some(b.pubkey),
        amount: Some(1000),
        keysend: Some(true),
        ..Default::default()
    }
    .build_send_payment_data(crate::invoice::Currency::Fibd)
    .unwrap();
    let hash = request.payment_hash;
    let (network, network_task) = Actor::spawn(None, DropAdmissionReply, ()).await.unwrap();
    let actor = PaymentActor::new(
        a.store.clone(),
        a.network_graph.clone(),
        FiberActorRef::from_network(&network),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (myself, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let response = actor
        .send_payment_with_payment_data(myself, &mut state, request)
        .await;
    task.await.unwrap();
    network.stop(None);
    network_task.await.unwrap();
    let response =
        response.expect("a retained payment must return a trackable response, not an RPC failure");
    assert_eq!(response.payment_hash, hash);
    assert_eq!(response.status, PaymentStatus::Created);
    let attempts = a.store.get_attempts(hash);
    assert_eq!(attempts.len(), 1);
    let id = attempts[0].id;
    let current_outpoint = attempts[0].first_hop_channel_outpoint().unwrap().clone();
    let mut attempt = attempts[0].clone();
    let mut hops = attempt.route_hops.clone();
    hops[0].funding_tx_hash = fiber_types::Hash256::from([77; 32]);
    attempt.update_route(hops);
    let old_outpoint = attempt.first_hop_channel_outpoint().unwrap().clone();
    attempt.status = AttemptStatus::Retrying;
    attempt.last_error = Some("rebuild route after failure".to_owned());
    a.store.insert_attempt(attempt);
    let mut session = a.store.get_payment_session(hash).unwrap();
    let (network, network_task) = Actor::spawn(None, DropAdmissionReply, ()).await.unwrap();
    let actor = PaymentActor::new(
        a.store.clone(),
        a.network_graph.clone(),
        FiberActorRef::from_network(&network),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (myself, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let result = actor
        .retry_payment_attempt(myself, &mut state, &mut session, Some(id))
        .await;
    task.await.unwrap();
    network.stop(None);
    network_task.await.unwrap();
    assert!(matches!(
        result,
        Err(crate::Error::PaymentAdmissionInterrupted(_))
    ));
    let retained = a.store.get_attempt(hash, id).unwrap();
    assert!(retained.is_retrying());
    assert_eq!(
        retained.first_hop_channel_outpoint(),
        Some(&current_outpoint)
    );
    assert!(a
        .store
        .get_pending_attempts_by_channel_outpoint(&old_outpoint)
        .is_empty());
    assert_eq!(
        a.store
            .get_pending_attempts_by_channel_outpoint(&current_outpoint)
            .len(),
        1
    );
    // Only B becomes ready. No old-channel event or direct retry rescues the payment.
    a.network_actor
        .send_message(NetworkActorMessage::new_event(
            FiberActorEvent::ChannelReady(channel_id, b.pubkey, current_outpoint),
        ))
        .unwrap();
    a.wait_until_success(hash).await;
    a.wait_until_payment_finished(hash).await;
    a.wait_until_no_inflight_payments().await;
    assert_eq!(a.store.get_attempts(hash).len(), 1);
    assert_eq!(a.store.get_attempt(hash, id).unwrap().tried_times, 1);
}

#[tokio::test]
async fn test_lost_admission_reply_does_not_fail_or_retry_attempt() {
    use crate::fiber::PaymentStatus;

    let (a, b, _) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_until_payment_finished(hash).await;
    a.wait_until_no_inflight_payments().await;
    let mut attempt = a.store.get_attempts(hash).pop().unwrap();
    attempt.status = AttemptStatus::Created;
    attempt.preimage = None;
    let tried_times = attempt.tried_times;
    a.store.insert_attempt(attempt.clone());
    let mut session = a.store.get_payment_session(hash).unwrap();
    session.status = PaymentStatus::Created;
    a.store.insert_payment_session(session.clone());
    let (network, network_task) = Actor::spawn(None, DropAdmissionReply, ()).await.unwrap();
    let actor = PaymentActor::new(
        a.store.clone(),
        a.network_graph.clone(),
        FiberActorRef::from_network(&network),
    );
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::TlcUpdated,
    });
    let (myself, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let result = actor
        .send_attempt(myself, &mut state, &mut session, &mut attempt)
        .await;
    assert!(matches!(
        result,
        Err(crate::Error::PaymentAdmissionInterrupted(_))
    ));
    let preserved = a.store.get_attempt(hash, attempt.id).unwrap();
    assert_eq!(preserved.status, AttemptStatus::Created);
    assert_eq!(preserved.tried_times, tried_times);
    assert_eq!(state.retry_send_payment_count, 0);
    assert_eq!(
        a.store.get_persisted_payment_status(hash),
        Some(PaymentStatus::Created)
    );
    task.await.unwrap();
    network.stop(None);
    network_task.await.unwrap();
}

#[tokio::test]
async fn test_review_retry_lost_admission_preserves_attempt() {
    use crate::fiber::PaymentStatus;
    let (a, b, _) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_until_payment_finished(hash).await;
    a.wait_until_no_inflight_payments().await;
    for mpp in [false, true] {
        let mut attempt = a.store.get_attempts(hash).pop().unwrap();
        attempt.status = AttemptStatus::Retrying;
        attempt.preimage = None;
        attempt.last_error = None;
        a.store.insert_attempt(attempt.clone());
        let mut session = a.store.get_payment_session(hash).unwrap();
        session.status = PaymentStatus::Created;
        session.request.keysend = !mpp;
        session.request.allow_mpp = mpp;
        session.request.max_parts = Some(3);
        a.store.insert_payment_session(session.clone());
        let (network, network_task) = Actor::spawn(None, DropAdmissionReply, ()).await.unwrap();
        let actor = PaymentActor::new(
            a.store.clone(),
            a.network_graph.clone(),
            FiberActorRef::from_network(&network),
        );
        let mut state = PaymentActorState::new(PaymentActorArguments {
            session_key: hash.into(),
            init_command: PaymentActorMessage::TlcUpdated,
        });
        let (myself, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
        let result = actor
            .retry_payment_attempt(myself, &mut state, &mut session, Some(attempt.id))
            .await;
        task.await.unwrap();
        network.stop(None);
        network_task.await.unwrap();
        let preserved = a.store.get_attempt(hash, attempt.id).unwrap();
        assert_eq!(
            preserved.status,
            AttemptStatus::Retrying,
            "lost retry admission must not fail the attempt (mpp={mpp})"
        );
        assert!(matches!(
            result,
            Err(crate::Error::PaymentAdmissionInterrupted(_))
        ));
        assert_eq!(preserved.tried_times, attempt.tried_times);
        assert_eq!(state.retry_send_payment_count, 0);
        assert_eq!(a.store.get_attempts(hash).len(), 1);
        assert_eq!(
            a.store.get_persisted_payment_status(hash),
            Some(PaymentStatus::Created)
        );
    }
}

#[tokio::test]
async fn test_retry_exhaustion_does_not_publish_failed_while_tlc_is_inflight() {
    assert_retry_decision_with_inflight_tlc(true, false).await;
}

#[tokio::test]
async fn test_full_part_capacity_waits_without_stopping_payment() {
    assert_retry_decision_with_inflight_tlc(false, false).await;
}

#[tokio::test]
async fn test_reserved_retry_at_budget_limit_is_not_stopped_before_dispatch() {
    assert_retry_decision_with_inflight_tlc(false, true).await;
}

async fn assert_retry_decision_with_inflight_tlc(exhausted: bool, reserved_retry: bool) {
    use super::SendPaymentCommand;
    use crate::{fiber::PaymentStatus, gen_rand_sha256_hash};
    let (a, b, _) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let invoice = b.build_basic_invoice(1000, gen_rand_sha256_hash());
    let hash = *invoice.payment_hash();
    b.insert_invoice(invoice.clone(), None);
    a.send_payment(SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        ..Default::default()
    })
    .await
    .unwrap();
    a.wait_until_inflight(hash).await;
    let mut session = a.store.get_payment_session(hash).unwrap();
    session.request.amount = if reserved_retry { 1000 } else { 2000 };
    session.request.max_parts = Some(1);
    session.try_limit = if exhausted { 0 } else { 100 };
    if reserved_retry {
        let mut attempt = a.store.get_attempts(hash).pop().unwrap();
        attempt.status = AttemptStatus::Retrying;
        attempt.tried_times = 2;
        session.try_limit = 2;
        a.store.insert_attempt(attempt);
    }
    a.store.insert_payment_session(session.clone());
    let actor = PaymentActor::new(
        a.store.clone(),
        a.network_graph.clone(),
        FiberActorRef::from_network(&a.network_actor),
    );
    if reserved_retry {
        actor.advance_payment_result(&PaymentActorState::new(PaymentActorArguments {
            session_key: hash.into(),
            init_command: PaymentActorMessage::TlcUpdated,
        }));
    }
    let decision = actor.payment_need_more_retry(&mut session);
    if reserved_retry {
        let recovered = a.store.get_payment_session(hash).unwrap();
        assert!(
            recovered.can_retry(),
            "tried_times includes the reserved next send, not just sends already executed"
        );
        assert!(!recovered.status.is_final());
        assert!(matches!(decision, Ok(false)));
        return;
    }
    assert_eq!(
        a.store
            .get_payment_session(hash)
            .unwrap()
            .effective_status(&a.store),
        PaymentStatus::Inflight,
        "a retry decision must not publish final failure while an issued TLC can still fulfill"
    );
    if !exhausted {
        assert!(
            matches!(decision, Ok(false)),
            "full part capacity is a wait, not a terminal error"
        );
    }
}

#[tokio::test]
async fn test_success_exits_without_failing_unissued_attempts_or_rewriting_session() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let (a, b, _) = create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_until_payment_finished(hash).await;
    a.wait_until_no_inflight_payments().await;
    let settled = a.store.get_attempts(hash).pop().unwrap();
    let outpoint = settled.first_hop_channel_outpoint().unwrap().clone();
    // Recovery can leave planned or retryable attempts that never reached a channel.
    for (id, status) in [(2, AttemptStatus::Created), (3, AttemptStatus::Retrying)] {
        let mut unissued = settled.clone();
        unissued.id = id;
        unissued.status = status;
        unissued.preimage = None;
        a.store.insert_attempt(unissued);
    }
    let session = a.store.get_payment_session(hash).unwrap();
    assert!(!session.has_pending_payment_work(&a.store));
    assert_eq!(
        a.store
            .get_pending_attempts_by_channel_outpoint(&outpoint)
            .len(),
        2
    );

    let writes = Arc::new(AtomicUsize::new(0));
    let mut store = a.store.clone();
    let observed = writes.clone();
    store.set_watcher(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let actor = PaymentActor::new(
        store,
        a.network_graph.clone(),
        FiberActorRef::from_network(&a.network_actor),
    );
    let (mailbox, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::TlcUpdated,
    });
    actor
        .handle_command(mailbox, &mut state, PaymentActorMessage::TlcUpdated)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        a.store.get_attempt(hash, 2).unwrap().status,
        AttemptStatus::Created
    );
    assert_eq!(
        a.store.get_attempt(hash, 3).unwrap().status,
        AttemptStatus::Retrying
    );
    assert!(a
        .store
        .get_pending_attempts_by_channel_outpoint(&outpoint)
        .is_empty());
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "an unchanged success must not publish duplicate store updates"
    );
}

#[tokio::test]
async fn test_retrying_add_respects_live_owner_but_not_closed_failure_queue() {
    let (mut a, mut b, id) =
        create_nodes_with_established_channel(100000000000, 11800000000, false).await;
    a.hold_next_fiber_messages(b.pubkey, id, TestFiberMessageKind::CommitmentSigned, 1)
        .await;
    let hash = a
        .send_payment_keysend(&b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    a.wait_for_held_fiber_messages(1).await;
    let mut attempt = a.store.get_attempts(hash).pop().unwrap();
    b.stop().await;
    a.stop().await;
    let actor = PaymentActor::new(
        a.store.clone(),
        a.network_graph.clone(),
        FiberActorRef::from_network(&a.network_actor),
    );
    let mut source = a.get_channel_actor_state(id);
    let mut tlc = source.tlc_state.offered_tlcs.tlcs[0].clone();
    source.tlc_state.offered_tlcs.tlcs.clear();
    source
        .retryable_tlc_operations
        .push_back(fiber_types::RetryableTlcOperation::AddTlc(
            fiber_types::AddTlcCommand {
                upstream_tlc_key: tlc.upstream_tlc_key,
                amount: tlc.amount,
                payment_hash: tlc.payment_hash,
                attempt_id: tlc.attempt_id,
                expiry: tlc.expiry,
                hash_algorithm: tlc.hash_algorithm,
                onion_packet: tlc.onion_packet.clone(),
                shared_secret: tlc.shared_secret,
                previous_tlc: None,
                is_trampoline_hop: tlc.is_trampoline_hop,
            },
        ));
    a.store.insert_channel_actor_state(source.clone());
    attempt.status = AttemptStatus::Retrying;
    attempt.tried_times = 1;
    assert!(actor.channel_owns_attempt(&attempt));
    source.state = fiber_types::ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
    a.store.insert_channel_actor_state(source.clone());
    assert!(
        !actor.channel_owns_attempt(&attempt),
        "a closed failure notification cannot execute the retry"
    );
    attempt.status = AttemptStatus::Created;
    assert!(
        actor.channel_owns_attempt(&attempt),
        "Created must still wait for the durable failure handoff"
    );
    attempt.status = AttemptStatus::Retrying;
    tlc.applied_flags.insert(fiber_types::AppliedFlags::REMOVE);
    source
        .session
        .remove_effects
        .push((tlc, Some(attempt.tried_times)));
    a.store.insert_channel_actor_state(source.clone());
    assert!(
        actor.channel_owns_attempt(&attempt),
        "the current execution's outbox must prevent a duplicate retry"
    );
    source.session.remove_effects[0].1 = Some(attempt.tried_times - 1);
    a.store.insert_channel_actor_state(source.clone());
    assert!(
        !actor.channel_owns_attempt(&attempt),
        "an older failed execution must not prevent retrying"
    );
    source.session.remove_effects[0].1 = None;
    a.store.insert_channel_actor_state(source.clone());
    assert!(
        actor.channel_owns_attempt(&attempt),
        "an unbound result must be resolved before retrying"
    );
    source.session.remove_effects[0].0.upstream_tlc_key = Some(fiber_types::UpstreamTlcKey {
        incoming_channel_id: fiber_types::Hash256::from([81; 32]),
        incoming_tlc_id: 7,
    });
    a.store.insert_channel_actor_state(source.clone());
    assert!(
        !actor.channel_owns_attempt(&attempt),
        "same hash and attempt ID in another hosted session must not own this execution"
    );
    let other_session = source.session.remove_effects[0].0.upstream_tlc_key;
    for operation in &mut source.retryable_tlc_operations {
        if let fiber_types::RetryableTlcOperation::AddTlc(command) = operation {
            command.upstream_tlc_key = other_session;
        }
    }
    a.store.insert_channel_actor_state(source.clone());
    assert!(
        !super::attempt_has_channel_work(&attempt, &a.store),
        "another hosted session's queued Add and outbox must not delay this payment"
    );
    source.retryable_tlc_operations.clear();
    source.session.remove_effects.clear();
    a.store.insert_channel_actor_state(source);
    attempt.last_error = Some("persisted failure awaiting retry".to_owned());
    a.store.insert_attempt(attempt.clone());
    let (mailbox, task) = Actor::spawn(None, RetryMailbox, ()).await.unwrap();
    let mut state = PaymentActorState::new(PaymentActorArguments {
        session_key: hash.into(),
        init_command: PaymentActorMessage::RetrySendPayment(None),
    });
    actor.register_payment_retry(mailbox.clone(), &mut state, Some(attempt.id));
    // The stopped transport is intentional: a duplicate startup claim must do no network work.
    actor
        .handle_command(
            mailbox.clone(),
            &mut state,
            PaymentActorMessage::ResumeAfterClosedChannel(attempt.id),
        )
        .await
        .unwrap();
    assert_eq!(
        a.store.get_attempt(hash, attempt.id).unwrap().status,
        AttemptStatus::Retrying,
        "startup must not dispatch again while a live callback's retry is scheduled"
    );
    mailbox.stop(None);
    task.await.unwrap();
}
