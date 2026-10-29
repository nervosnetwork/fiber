//! Recovery tests for a channel actor restart.
//!
//! Two persisted states survive a process exit but used to have no restore path:
//!
//! * `waiting_forward_tlc_tasks`, which only lives in memory as a network actor task, and
//! * `session_v2.remove_effects` entries whose payment effect could not be bound because the
//!   network actor was busy.

use std::sync::{Arc, Mutex};

use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::fiber::channel::{
    ChannelActor, ChannelActorMessage, ChannelActorState, ChannelActorStateStore,
};
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::network::{NetworkActorCommand, NetworkActorEvent, NetworkActorMessage};
use crate::fiber::payment::{PaymentSessionExt, SendPaymentDataBuilder};
use crate::fiber::tests::settle_tlc_set_command_tests::{
    create_test_channel_state_with_tlc, MockStore,
};
use crate::fiber::types::RemoveTlcReason;
use crate::gen_rand_sha256_hash;
use crate::tests::gen_utils::gen_rand_fiber_public_key;
use fiber_types::{
    AppliedFlags, CommitmentNumbers, Hash256, HashAlgorithm, OutboundTlcStatus, RemoveTlcFulfill,
    RetryableTlcOperation, TLCId, TlcInfo, TlcStatus,
};

const TEST_SHARED_SECRET: [u8; 32] = [7u8; 32];

/// A channel actor that records nothing and never replies to scheduled messages.
struct NullChannelActor;

#[async_trait::async_trait]
impl Actor for NullChannelActor {
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

/// A network actor that keeps every message instead of answering it. Keeping the message alive
/// also keeps the `RpcReplyPort` alive, so `call_t!` observes a real timeout rather than an
/// immediately dropped reply.
struct SilentNetworkActor;

type SilentNetworkState = Arc<Mutex<Vec<NetworkActorMessage>>>;

#[async_trait::async_trait]
impl Actor for SilentNetworkActor {
    type Msg = NetworkActorMessage;
    type State = SilentNetworkState;
    type Arguments = SilentNetworkState;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(state)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.lock().unwrap().push(message);
        Ok(())
    }
}

async fn spawn_null_channel_actor() -> ActorRef<ChannelActorMessage> {
    let (actor, _handle) = Actor::spawn(None, NullChannelActor, ())
        .await
        .expect("spawn null channel actor");
    actor
}

fn offered_tlc(
    tlc_id: u64,
    amount: u128,
    payment_hash: Hash256,
    attempt_id: Option<u64>,
) -> TlcInfo {
    TlcInfo {
        status: TlcStatus::Outbound(OutboundTlcStatus::Committed),
        tlc_id: TLCId::Offered(tlc_id),
        amount,
        payment_hash,
        total_amount: None,
        payment_secret: None,
        attempt_id,
        expiry: crate::now_timestamp_as_millis_u64() + 1_000_000,
        hash_algorithm: HashAlgorithm::CkbHash,
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

/// A settled (RemoveAckConfirmed) removal of an offered TLC that still has to be applied locally.
fn offered_tlc_removed_confirmed(
    tlc_id: u64,
    amount: u128,
    payment_hash: Hash256,
    attempt_id: Option<u64>,
) -> TlcInfo {
    let mut tlc = offered_tlc(tlc_id, amount, payment_hash, attempt_id);
    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed);
    tlc.removed_reason = Some(RemoveTlcReason::RemoveTlcFail(
        fiber_types::TlcErrPacket::new(
            fiber_types::TlcErr::new(fiber_types::TlcErrorCode::HoldTlcTimeout),
            &TEST_SHARED_SECRET,
        ),
    ));
    tlc
}

/// The forward that node B dispatched before it restarted is re-linked to the downstream
/// TLC that B already created, instead of being failed back.
#[tokio::test]
async fn test_pending_forward_is_relinked_to_existing_downstream_tlc() {
    let upstream_channel_id = gen_rand_sha256_hash();
    let downstream_channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();

    let mut state =
        create_test_channel_state_with_tlc(upstream_channel_id, 0, 1000, payment_hash, None);
    state.id = upstream_channel_id;
    state
        .waiting_forward_tlc_tasks
        .insert(TLCId::Received(0), TEST_SHARED_SECRET);

    // The downstream channel already holds the offered TLC that carries the upstream link.
    let mut downstream =
        create_test_channel_state_with_tlc(downstream_channel_id, 3, 1000, payment_hash, None);
    downstream.id = downstream_channel_id;
    let mut forwarded_tlc = offered_tlc(3, 1000, payment_hash, None);
    forwarded_tlc.forwarding_tlc = Some((upstream_channel_id, 0));
    downstream.tlc_state.offered_tlcs.tlcs.clear();
    downstream.tlc_state.offered_tlcs.tlcs.push(forwarded_tlc);

    let store = MockStore::new();
    store.insert_channel_actor_state(state.clone());
    store.insert_channel_actor_state(downstream);

    let (network, _) = Actor::spawn(None, SilentNetworkActor, Default::default())
        .await
        .expect("spawn network actor");
    let myself = spawn_null_channel_actor().await;
    let actor = ChannelActor::new(
        gen_rand_fiber_public_key(),
        gen_rand_fiber_public_key(),
        network,
        store,
        None,
    );

    let recovered = actor.recover_pending_forward_tlcs(&myself, &mut state);
    assert!(recovered, "the persisted forward must be recovered");

    assert!(
        state.waiting_forward_tlc_tasks.is_empty(),
        "the resolved forward must leave the waiting map"
    );
    let tlc = state
        .tlc_state
        .get(&TLCId::Received(0))
        .expect("received tlc");
    assert_eq!(
        tlc.forwarding_tlc,
        Some((downstream_channel_id, 3)),
        "the recovered forward must point at the downstream tlc"
    );
    assert!(
        state.retryable_tlc_operations.is_empty(),
        "a recovered forward must not be failed back"
    );
}

/// When the downstream TLC never got created before the restart, the forwarded TLC is failed
/// back so the sender can retry instead of leaving the channel blocked forever.
#[tokio::test]
async fn test_pending_forward_without_downstream_tlc_is_failed_back() {
    let upstream_channel_id = gen_rand_sha256_hash();
    let payment_hash = gen_rand_sha256_hash();

    let mut state =
        create_test_channel_state_with_tlc(upstream_channel_id, 0, 1000, payment_hash, None);
    state.id = upstream_channel_id;
    state
        .waiting_forward_tlc_tasks
        .insert(TLCId::Received(0), TEST_SHARED_SECRET);

    let store = MockStore::new();
    store.insert_channel_actor_state(state.clone());

    let (network, _) = Actor::spawn(None, SilentNetworkActor, Default::default())
        .await
        .expect("spawn network actor");
    let myself = spawn_null_channel_actor().await;
    let actor = ChannelActor::new(
        gen_rand_fiber_public_key(),
        gen_rand_fiber_public_key(),
        network,
        store,
        None,
    );

    let recovered = actor.recover_pending_forward_tlcs(&myself, &mut state);
    assert!(recovered, "the persisted forward must be recovered");

    assert!(
        state.waiting_forward_tlc_tasks.is_empty(),
        "an unresolved forward must not stay pending after restart"
    );
    assert!(
        state.retryable_tlc_operations.iter().any(|op| matches!(
            op,
            RetryableTlcOperation::RemoveTlc(TLCId::Received(0), RemoveTlcReason::RemoveTlcFail(_))
        )),
        "the lost forward must be queued as a retryable failure, got {:?}",
        state.retryable_tlc_operations
    );
}

#[tokio::test]
async fn test_failed_forward_dispatch_keeps_recoverable_ownership() {
    for owner in ["absent", "queued", "created"] {
        let upstream = gen_rand_sha256_hash();
        let downstream = gen_rand_sha256_hash();
        let hash = gen_rand_sha256_hash();
        let mut state = create_test_channel_state_with_tlc(upstream, 0, 1000, hash, None);
        state
            .tlc_state
            .get_mut(&TLCId::Received(0))
            .unwrap()
            .applied_flags = AppliedFlags::ADD;
        let packet = fiber_types::PeeledPaymentOnionPacket::create(
            crate::tests::gen_utils::gen_rand_fiber_private_key(),
            vec![
                fiber_types::PaymentHopData {
                    amount: 1000,
                    next_hop: Some(gen_rand_fiber_public_key()),
                    ..Default::default()
                },
                fiber_types::PaymentHopData {
                    amount: 1000,
                    ..Default::default()
                },
            ],
            Some(hash.as_ref().to_vec()),
            &secp256k1::Secp256k1::new(),
        )
        .unwrap();
        let (stopped, stopped_task) = Actor::spawn(None, SilentNetworkActor, Default::default())
            .await
            .unwrap();
        stopped.stop(None);
        stopped_task.await.unwrap();
        let store = MockStore::new();
        let actor = ChannelActor::new(
            state.get_local_pubkey(),
            state.get_remote_pubkey(),
            stopped,
            MockStore::new(),
            None,
        );
        actor.register_and_apply_forward_tlc(&mut state, hash, TLCId::Received(0), packet, 0);
        store.insert_channel_actor_state(state);
        let saved = store.get_channel_actor_state(&upstream).unwrap();
        let core = bincode::deserialize::<fiber_types::ChannelActorData>(
            &bincode::serialize(&saved.core).unwrap(),
        )
        .unwrap();
        let mut restored = ChannelActorState::from(core);
        assert!(
            restored
                .waiting_forward_tlc_tasks
                .contains_key(&TLCId::Received(0)),
            "failed dispatch must leave work for restore"
        );
        if owner != "absent" {
            let mut next = create_test_channel_state_with_tlc(downstream, 3, 1000, hash, None);
            next.tlc_state = Default::default();
            if owner == "queued" {
                next.retryable_tlc_operations
                    .push_back(RetryableTlcOperation::AddTlc(queued_forward_add(
                        upstream, hash,
                    )));
            } else {
                let mut tlc = offered_tlc(3, 1000, hash, None);
                tlc.forwarding_tlc = Some((upstream, 0));
                next.tlc_state.offered_tlcs.tlcs.push(tlc);
            }
            store.insert_channel_actor_state(next);
        }
        let (network, task) = Actor::spawn(None, SilentNetworkActor, Default::default())
            .await
            .unwrap();
        restored.network = Some(network.clone());
        let actor = ChannelActor::new(
            restored.get_local_pubkey(),
            restored.get_remote_pubkey(),
            network.clone(),
            store,
            None,
        );
        let myself = spawn_null_channel_actor().await;
        actor.recover_pending_forward_tlcs(&myself, &mut restored);
        if owner == "absent" {
            assert!(restored.retryable_tlc_operations.iter().any(|op| matches!(
                op,
                RetryableTlcOperation::RemoveTlc(
                    TLCId::Received(0),
                    RemoveTlcReason::RemoveTlcFail(_)
                )
            )));
        } else {
            assert!(
                restored.retryable_tlc_operations.is_empty(),
                "a downstream owner must keep upstream backing"
            );
            if owner == "queued" {
                assert!(restored
                    .waiting_forward_tlc_tasks
                    .contains_key(&TLCId::Received(0)));
            } else {
                assert_eq!(
                    restored
                        .tlc_state
                        .get(&TLCId::Received(0))
                        .unwrap()
                        .forwarding_tlc,
                    Some((downstream, 3))
                );
            }
        }
        myself.stop(None);
        network.stop(None);
        task.await.unwrap();
    }
}

fn queued_forward_add(upstream: Hash256, hash: Hash256) -> fiber_types::AddTlcCommand {
    let expiry = crate::now_timestamp_as_millis_u64() + 86_400_000;
    let packet = fiber_types::PeeledPaymentOnionPacket::create(
        crate::tests::gen_utils::gen_rand_fiber_private_key(),
        vec![
            fiber_types::PaymentHopData {
                amount: 1000,
                expiry,
                next_hop: Some(gen_rand_fiber_public_key()),
                hash_algorithm: HashAlgorithm::CkbHash,
                ..Default::default()
            },
            fiber_types::PaymentHopData {
                amount: 1000,
                expiry,
                hash_algorithm: HashAlgorithm::CkbHash,
                ..Default::default()
            },
        ],
        Some(hash.as_ref().to_vec()),
        &secp256k1::Secp256k1::new(),
    )
    .unwrap();
    fiber_types::AddTlcCommand {
        amount: 1000,
        payment_hash: hash,
        attempt_id: None,
        expiry,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: packet.next,
        shared_secret: packet.shared_secret,
        is_trampoline_hop: false,
        previous_tlc: Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
            upstream,
            0,
            0,
            TEST_SHARED_SECRET,
        )),
    }
}

#[tokio::test]
async fn test_pending_forward_queue_survives_recovery_and_relinks_after_creation() {
    for downstream_first in [false, true] {
        let upstream = gen_rand_sha256_hash();
        let downstream = gen_rand_sha256_hash();
        let hash = gen_rand_sha256_hash();
        let mut incoming = create_test_channel_state_with_tlc(upstream, 0, 1000, hash, None);
        incoming
            .waiting_forward_tlc_tasks
            .insert(TLCId::Received(0), TEST_SHARED_SECRET);
        let mut outgoing = create_test_channel_state_with_tlc(downstream, 0, 1000, hash, None);
        outgoing.tlc_state = Default::default();
        outgoing
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::AddTlc(queued_forward_add(
                upstream, hash,
            )));
        let (store, _dir) = crate::tests::test_utils::generate_store();
        store.insert_channel_actor_state(incoming);
        store.insert_channel_actor_state(outgoing);
        let (network, task) = Actor::spawn(None, SilentNetworkActor, Default::default())
            .await
            .unwrap();
        let myself = spawn_null_channel_actor().await;
        let actor = ChannelActor::new(
            gen_rand_fiber_public_key(),
            gen_rand_fiber_public_key(),
            network.clone(),
            store.clone(),
            None,
        );
        let mut incoming = store.get_channel_actor_state(&upstream).unwrap();
        if !downstream_first {
            for _ in 0..2 {
                actor.recover_pending_forward_tlcs(&myself, &mut incoming);
                assert!(
                    incoming.retryable_tlc_operations.is_empty(),
                    "a durable queued forward must not be failed back"
                );
                assert!(incoming.is_waiting_forward_result_for_received_tlc(TLCId::Received(0)));
                assert!(
                    incoming
                        .tlc_state
                        .get(&TLCId::Received(0))
                        .unwrap()
                        .forwarding_tlc
                        .is_none(),
                    "a queued Add has no downstream TLC id yet"
                );
                assert_eq!(
                    store
                        .get_channel_actor_state(&downstream)
                        .unwrap()
                        .retryable_tlc_operations
                        .len(),
                    1
                );
            }
        }
        // Model the downstream actor's durable queue-to-TLC handoff before its result arrives.
        let mut outgoing = store.get_channel_actor_state(&downstream).unwrap();
        outgoing.retryable_tlc_operations.clear();
        let mut created = offered_tlc(3, 1000, hash, None);
        created.forwarding_tlc = Some((upstream, 0));
        outgoing.tlc_state.offered_tlcs.tlcs.push(created);
        store.insert_channel_actor_state(outgoing);
        actor.recover_pending_forward_tlcs(&myself, &mut incoming);
        assert!(incoming.waiting_forward_tlc_tasks.is_empty());
        assert!(incoming.retryable_tlc_operations.is_empty());
        assert_eq!(
            incoming
                .tlc_state
                .get(&TLCId::Received(0))
                .unwrap()
                .forwarding_tlc,
            Some((downstream, 3))
        );
        myself.stop(None);
        network.stop(None);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn test_pending_forward_queue_must_match_upstream_identity_and_hash() {
    for mismatch in ["hash", "channel", "tlc"] {
        let upstream = gen_rand_sha256_hash();
        let downstream = gen_rand_sha256_hash();
        let hash = gen_rand_sha256_hash();
        let mut incoming = create_test_channel_state_with_tlc(upstream, 0, 1000, hash, None);
        incoming
            .waiting_forward_tlc_tasks
            .insert(TLCId::Received(0), TEST_SHARED_SECRET);
        let mut outgoing = create_test_channel_state_with_tlc(downstream, 0, 1000, hash, None);
        outgoing.tlc_state = Default::default();
        let mut command = queued_forward_add(upstream, hash);
        match mismatch {
            "hash" => command.payment_hash = gen_rand_sha256_hash(),
            "channel" => {
                command.previous_tlc.as_mut().unwrap().prev_channel_id = gen_rand_sha256_hash()
            }
            "tlc" => command.previous_tlc.as_mut().unwrap().prev_tlc_id = 1,
            _ => unreachable!(),
        }
        outgoing
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::AddTlc(command));
        let store = MockStore::new();
        store.insert_channel_actor_state(incoming.clone());
        store.insert_channel_actor_state(outgoing);
        let (network, task) = Actor::spawn(None, SilentNetworkActor, Default::default())
            .await
            .unwrap();
        let myself = spawn_null_channel_actor().await;
        let actor = ChannelActor::new(
            gen_rand_fiber_public_key(),
            gen_rand_fiber_public_key(),
            network.clone(),
            store,
            None,
        );
        actor.recover_pending_forward_tlcs(&myself, &mut incoming);
        assert!(
            !incoming.retryable_tlc_operations.is_empty(),
            "unrelated queue must not claim this incoming TLC: {mismatch}"
        );
        myself.stop(None);
        network.stop(None);
        task.await.unwrap();
    }
}

/// Trampoline payments own their upstream TLC through the persisted payment context, not a
/// downstream channel's `forwarding_tlc` link. Recovery must match the full TLC identity.
#[tokio::test]
async fn test_pending_trampoline_forward_is_resumed_only_for_matching_tlc() {
    for mismatch in [
        None,
        Some("hash"),
        Some("channel"),
        Some("tlc"),
        Some("context"),
    ] {
        let upstream_channel_id = gen_rand_sha256_hash();
        let payment_hash = gen_rand_sha256_hash();
        let mut state =
            create_test_channel_state_with_tlc(upstream_channel_id, 0, 1000, payment_hash, None);
        state.id = upstream_channel_id;
        state
            .waiting_forward_tlc_tasks
            .insert(TLCId::Received(0), TEST_SHARED_SECRET);
        let (store, _dir) = crate::tests::test_utils::generate_store();
        store.insert_channel_actor_state(state.clone());
        let request = SendPaymentDataBuilder::new(
            gen_rand_fiber_public_key(),
            900,
            if mismatch == Some("hash") {
                gen_rand_sha256_hash()
            } else {
                payment_hash
            },
        )
        .max_fee_amount(Some(100))
        .trampoline_context(
            (mismatch != Some("context")).then(|| fiber_types::TrampolineContext {
                remaining_trampoline_onion: vec![],
                previous_tlcs: vec![fiber_types::PrevTlcInfo::new_with_shared_secret(
                    if mismatch == Some("channel") {
                        gen_rand_sha256_hash()
                    } else {
                        upstream_channel_id
                    },
                    if mismatch == Some("tlc") { 1 } else { 0 },
                    100,
                    TEST_SHARED_SECRET,
                )],
                hash_algorithm: HashAlgorithm::CkbHash,
                max_outgoing_tlc_expiry: None,
            }),
        )
        .build()
        .unwrap();
        store.insert_payment_session(fiber_types::PaymentSession::new_session(&store, request, 3));

        let captured = SilentNetworkState::default();
        let (network, handle) = Actor::spawn(None, SilentNetworkActor, captured.clone())
            .await
            .unwrap();
        let myself = spawn_null_channel_actor().await;
        let actor = ChannelActor::new(
            gen_rand_fiber_public_key(),
            gen_rand_fiber_public_key(),
            network.clone(),
            store,
            None,
        );
        actor.recover_pending_forward_tlcs(&myself, &mut state);
        if mismatch.is_none() {
            assert!(
                state.retryable_tlc_operations.is_empty(),
                "a persisted trampoline payment must not be failed back"
            );
            assert!(
                state.is_waiting_forward_result_for_received_tlc(TLCId::Received(0)),
                "keep the guard against local auto-fulfillment while the payment owns this TLC"
            );
            assert!(state
                .tlc_state
                .get(&TLCId::Received(0))
                .unwrap()
                .removed_reason
                .is_none());
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while captured.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                matches!(captured.lock().unwrap().as_slice(), [NetworkActorMessage::Event(NetworkActorEvent::RetrySendPayment(hash, None))] if *hash == payment_hash),
                "re-drive the payment owner, including its terminal outcome replay"
            );
        } else {
            assert!(
                state.waiting_forward_tlc_tasks.is_empty(),
                "an unrelated payment must not claim this forward: {mismatch:?}"
            );
            assert!(matches!(
                state.retryable_tlc_operations.front(),
                Some(RetryableTlcOperation::RemoveTlc(
                    TLCId::Received(0),
                    RemoveTlcReason::RemoveTlcFail(_)
                ))
            ));
        }
        myself.stop(None);
        network.stop(None);
        handle.await.unwrap();
    }
}

/// A payment-effect lookup that times out must defer the settled removal instead of panicking
/// the channel actor, and must leave the durable state untouched so it can be retried.
#[tokio::test]
async fn test_v2_settled_remove_defers_when_payment_effect_lookup_times_out() {
    use fiber_types::sample::StoreSample;

    let payment_hash = gen_rand_sha256_hash();
    let data = fiber_types::ChannelActorData::samples(7)
        .into_iter()
        .find(|data| data.session_v2.is_some())
        .expect("a V2 sample");
    let mut state = ChannelActorState::from(data);
    // Only the settled removal under test: the sample state carries its own TLCs.
    state.tlc_state = Default::default();
    let tlc = offered_tlc_removed_confirmed(0, 1000, payment_hash, Some(42));
    state.tlc_state.offered_tlcs.tlcs.push(tlc);
    state.tlc_state.offered_tlcs.next_tlc_id = 1;

    let store = MockStore::new();
    store.insert_channel_actor_state(state.clone());

    let (network, _) = Actor::spawn(None, SilentNetworkActor, Default::default())
        .await
        .expect("spawn network actor");
    let myself = spawn_null_channel_actor().await;
    let actor = ChannelActor::new(
        gen_rand_fiber_public_key(),
        gen_rand_fiber_public_key(),
        network,
        store,
        None,
    );

    // The silent network actor never answers `GetV2PaymentExecution`, so this used to turn into
    // `InvalidState("Cannot bind V2 payment effect: timeout")` and panic the channel actor.
    actor
        .apply_settled_remove_tlcs(&myself, &mut state, false)
        .await;

    let tlc = state
        .tlc_state
        .get(&TLCId::Offered(0))
        .expect("settled tlc must survive the deferral");
    assert!(
        !tlc.applied_flags.contains(AppliedFlags::REMOVE),
        "a deferred removal must stay unapplied so it can be retried"
    );
    assert!(
        state
            .session_v2
            .as_ref()
            .expect("v2 session")
            .remove_effects
            .is_empty(),
        "no outbox entry may be durable before its payment effect is bound"
    );
}

/// A settled removal that cannot be applied any more (here: a fulfill whose preimage does not
/// match the payment hash) must not kill the channel actor. The settled state is inconsistent,
/// so the only safe action is a bounded on-chain close.
#[tokio::test]
async fn test_v2_inapplicable_settled_remove_closes_instead_of_panicking() {
    use fiber_types::sample::StoreSample;

    let payment_hash = gen_rand_sha256_hash();
    let data = fiber_types::ChannelActorData::samples(7)
        .into_iter()
        .find(|data| data.session_v2.is_some())
        .expect("a V2 sample");
    let mut state = ChannelActorState::from(data);
    state.tlc_state = Default::default();
    let mut tlc = offered_tlc(0, 1000, payment_hash, None);
    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed);
    tlc.removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: gen_rand_sha256_hash(),
    }));
    state.tlc_state.offered_tlcs.tlcs.push(tlc);
    state.tlc_state.offered_tlcs.next_tlc_id = 1;

    let captured: Arc<Mutex<Vec<NetworkActorMessage>>> = Default::default();
    let (network, _) = Actor::spawn(None, SilentNetworkActor, captured.clone())
        .await
        .expect("spawn network actor");
    state.network = Some(network.clone());
    let store = MockStore::new();
    store.insert_channel_actor_state(state.clone());
    let myself = spawn_null_channel_actor().await;
    let actor = ChannelActor::new(
        gen_rand_fiber_public_key(),
        gen_rand_fiber_public_key(),
        network.clone(),
        store,
        None,
    );

    let balance_before = state.get_local_balance();

    // Used to panic with `expect remove tlc success: FinalIncorrectPreimage`.
    actor
        .apply_settled_remove_tlcs(&myself, &mut state, false)
        .await;

    let force_closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            {
                let messages = captured.lock().unwrap();
                if messages.iter().any(|message| {
                    matches!(
                        message,
                        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                            crate::fiber::channel::ChannelCommandWithId {
                                channel_id,
                                command: crate::fiber::channel::ChannelCommand::Shutdown(
                                    command,
                                    _
                                )
                            }
                        )) if *channel_id == state.id && command.force
                    )
                }) {
                    return true;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        force_closed,
        "an inapplicable settled removal must ask for a force close instead of panicking"
    );
    assert_eq!(
        state.get_local_balance(),
        balance_before,
        "a failed removal must not change the local balance"
    );
    assert!(
        state.tlc_state.get(&TLCId::Offered(0)).is_some(),
        "the unsettled tlc must stay in the channel state"
    );
}
