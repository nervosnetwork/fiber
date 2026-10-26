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
use crate::fiber::network::NetworkActorMessage;
use crate::fiber::tests::settle_tlc_set_command_tests::{
    create_test_channel_state_with_tlc, MockStore,
};
use crate::fiber::types::RemoveTlcReason;
use crate::gen_rand_sha256_hash;
use crate::tests::gen_utils::gen_rand_fiber_public_key;
use fiber_types::{
    AppliedFlags, CommitmentNumbers, Hash256, HashAlgorithm, OutboundTlcStatus,
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
