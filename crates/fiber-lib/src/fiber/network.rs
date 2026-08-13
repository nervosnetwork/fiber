use crate::store::actor::StoreActorMessage;
use ckb_hash::blake2b_256;
use ckb_sdk::rpc::ckb_indexer::{Order, ScriptType, SearchKey, SearchMode};
use ckb_types::core::tx_pool::TxStatus;
use ckb_types::core::{EpochNumberWithFraction, TransactionView};
use ckb_types::packed::{self, Byte32, OutPoint, Script, Transaction};
use ckb_types::prelude::{Builder, Entity, IntoTransactionView, Pack, Unpack};
use ckb_types::H256;
use either::Either;
use once_cell::sync::OnceCell;
use ractor::concurrency::Duration;
use ractor::{
    call_t, Actor, ActorCell, ActorProcessingErr, ActorRef, DerivedActorRef, RpcReplyPort,
    SupervisionEvent,
};
use rand::seq::{IteratorRandom, SliceRandom};
use secp256k1::SECP256K1;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::borrow::Cow;
#[cfg(test)]
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};
#[cfg(test)]
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use strum::AsRefStr;
use tentacle::multiaddr::{MultiAddr, Protocol};
use tentacle::service::SessionType;
use tentacle::utils::extract_peer_id;
use tentacle::utils::TransportType;
use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::SessionContext,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    multiaddr::Multiaddr,
    secio::PeerId,
    secio::SecioKeyPair,
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::codec::length_delimited;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

pub(crate) const CHANNEL_REESTABLISH_INTERVAL: Duration = Duration::from_millis(10);

use super::channel::{
    get_funding_and_reserved_amount, AcceptChannelParameter, ChannelActor, ChannelActorMessage,
    ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, ChannelEvent,
    ChannelInitializationParameter, ChannelOpenRecordStore, OpenChannelParameter,
    ProcessingChannelError, ProcessingChannelResult, RemoveTlcCommand, StopReason,
    DEFAULT_MAX_TLC_VALUE_IN_FLIGHT, PEER_CHANNEL_RESPONSE_TIMEOUT,
};
use super::gossip::{
    get_latest_startup_broadcast_message_cursor, GossipActorMessage, GossipMessageStore,
    GossipMessageUpdates,
};
use super::graph::{NetworkGraph, NetworkGraphStateStore, OwnedChannelUpdateEvent};
use super::types::{
    BroadcastMessageWithTimestamp, FiberMessage, ForwardTlcResult, GossipMessage, Init, OpenChannel,
};
use super::{
    FiberConfig, InFlightCkbTxActor, InFlightCkbTxActorArguments, InFlightCkbTxActorMessage,
    InFlightCkbTxKind, ASSUME_NETWORK_ACTOR_ALIVE,
};
use crate::actors::log_actor_failed;
use crate::ckb::client::CkbChainClient;
use crate::ckb::config::UdtCfgInfosExt;
use crate::ckb::contracts::{
    check_udt_script, get_udt_info, get_udt_whitelist, is_udt_type_auto_accept,
};
use crate::ckb::{CkbChainMessage, FundingError, FundingRequest, FundingTx, GetShutdownTxResponse};
use crate::fiber::channel::{
    tlc_expiry_delay, AddTlcResponse, ChannelActorState, ChannelEphemeralConfig,
    ChannelInitializationOperation, OfflineChannelRestoreMode,
    OpenChannelWithExternalFundingParameter, TxCollaborationCommand, TxUpdateCommand,
    MAX_TLC_NUMBER_IN_FLIGHT,
};
use crate::fiber::config::{DEFAULT_COMMITMENT_DELAY_EPOCHS, MIN_TLC_EXPIRY_DELTA};
use crate::fiber::fee::{check_open_channel_parameters, check_tlc_delta_with_epochs};
use crate::fiber::gossip::{GossipConfig, GossipService, SubscribableGossipMessageStore};
use crate::fiber::onchain_tlc_reconcile::{
    collect_onchain_confirmed_payer_tlcs, collect_onchain_fulfilled_tlcs,
    collect_onchain_received_timeout_settled_tlcs, collect_onchain_timeout_settled_tlcs,
    has_unresolved_onchain_tlcs, onchain_fulfilled_preimage, OnChainTimeoutTlcRole,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::fiber::payment::PaymentSessionExt;
use crate::fiber::payment::{
    PaymentActor, PaymentActorArguments, PaymentActorMessage, SendPaymentCommand,
    SendPaymentWithRouterCommand,
};
use crate::fiber::peer_message_policy::{PeerMessageAdmission, PeerMessagePolicy};
use crate::fiber::types::{
    pubkey_to_tentacle, FiberChannelMessage, TrampolineHopPayload, TrampolineOnionPacket, TxAbort,
    TxSignatures,
};
use crate::fiber::{
    settle_tlc_set_command::{SettleOnChainFulfilledInvoiceCommand, TlcSettlement},
    SettleTlcSetCommand,
};
use crate::invoice::{
    CancelInvoiceError, CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore, PreimageStore,
    SettleInvoiceError,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::lsp::{LspDeliveryDecision, LspPaymentOutcomeDecision, LspServiceMessage};
use crate::lsp::{LspPaymentDispatchError, TrampolineForwardingRequest};
use crate::utils::actor::ActorHandleLogGuard;
use crate::{now_timestamp_as_millis_u64, Error};
use fiber_types::protocol::AnnouncedNodeName;
pub use fiber_types::HopRequire;
#[cfg(any(debug_assertions, test, feature = "bench"))]
use fiber_types::SessionRoute;
use fiber_types::{
    blake2b_hash_with_salt, AddTlcCommand, AwaitingTxSignaturesFlags, ChannelOpenRecord,
    ChannelOpenSignerMaterial, ChannelOpeningStatus, ChannelState, ChannelTlcInfo, CloseFlags,
    EcdsaSignature, EntityHex, FeatureVector, Hash256, NodeAnnouncement, PaymentCustomRecords,
    PaymentSession, PaymentStatus, PeeledPaymentOnionPacket, PersistentNetworkActorState,
    PrevTlcInfo, Privkey, Pubkey, PublicChannelInfo, RemoveTlcFulfill, RemoveTlcReason,
    RetryableTlcOperation, RevocationData, RouterHop, SettlementData, ShuttingDownFlags, TLCId,
    TlcErr, TlcErrPacket, TlcErrorCode, UdtCfgInfos, NO_SHARED_SECRET,
};

pub const FIBER_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub const GOSSIP_PROTOCOL_ID: ProtocolId = ProtocolId::new(43);

pub const DEFAULT_CHAIN_ACTOR_TIMEOUT: u64 = 300000;

// TODO: make it configurable
pub const CKB_TX_TRACING_CONFIRMATIONS: u64 = 4;

pub const DEFAULT_PAYMENT_TRY_LIMIT: u32 = 5;

const ACTOR_HANDLE_WARN_THRESHOLD_MS: u64 = 15_000;

#[derive(Debug, Clone)]
struct OnChainTlcRemoveRelay {
    downstream_tlc_id: TLCId,
    forwarding_channel_id: Hash256,
    forwarding_tlc_id: u64,
    payment_hash: Hash256,
    reason: RemoveTlcReason,
}

pub(crate) fn onchain_upstream_removed_reason_matches(
    state: &ChannelActorState,
    tlc_id: u64,
    reason: &RemoveTlcReason,
) -> bool {
    state
        .tlc_state
        .get(&TLCId::Received(tlc_id))
        .and_then(|tlc| tlc.removed_reason.as_ref())
        .is_some_and(|removed_reason| removed_reason == reason)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum BufferedTrampolineUpstreamStatus {
    /// The exact upstream TLC still exists and has no removal queued.
    Pending,
    /// The upstream TLC is absent, removed, or has a durable removal queued.
    Removed,
    /// The channel is unavailable or the TLC belongs to another payment hash.
    Unknown,
}

fn buffered_trampoline_upstream_status(
    state: &ChannelActorState,
    payment_hash: Hash256,
    previous_tlc: &PrevTlcInfo,
) -> BufferedTrampolineUpstreamStatus {
    if state.retryable_tlc_operations.iter().any(|operation| {
        matches!(
            operation,
            RetryableTlcOperation::RemoveTlc(tlc_id, _)
                if *tlc_id == TLCId::Received(previous_tlc.prev_tlc_id)
        )
    }) {
        return BufferedTrampolineUpstreamStatus::Removed;
    }
    let Some(tlc) = state
        .tlc_state
        .get(&TLCId::Received(previous_tlc.prev_tlc_id))
    else {
        return BufferedTrampolineUpstreamStatus::Removed;
    };
    if tlc.payment_hash != payment_hash {
        error!(
            "Refusing to settle mismatched upstream trampoline TLC: payment_hash={:?}, tlc_payment_hash={:?}, channel_id={:?}, tlc_id={:?}",
            payment_hash,
            tlc.payment_hash,
            previous_tlc.prev_channel_id,
            previous_tlc.prev_tlc_id
        );
        return BufferedTrampolineUpstreamStatus::Unknown;
    }
    if tlc.removed_reason.is_none() {
        BufferedTrampolineUpstreamStatus::Pending
    } else {
        BufferedTrampolineUpstreamStatus::Removed
    }
}

fn trampoline_upstream_tlc_needs_settlement(
    state: &ChannelActorState,
    payment_hash: Hash256,
    previous_tlc: &PrevTlcInfo,
) -> bool {
    buffered_trampoline_upstream_status(state, payment_hash, previous_tlc)
        == BufferedTrampolineUpstreamStatus::Pending
}

// (128 + 2) KB, 2 KB for custom records
pub const MAX_SERVICE_PROTOCOAL_DATA_SIZE: usize = 1024 * (128 + 2);
pub const MAX_CUSTOM_RECORDS_SIZE: usize = 2 * 1024; // 2 KB

// This is a temporary way to document that we assume the chain actor is always alive.
// We may later relax this assumption. At the moment, if the chain actor fails, we
// should panic with this message, and later we may find all references to this message
// to make sure that we handle the case where the chain actor is not alive.
const ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW: &str =
    "We currently assume that chain actor is always alive, but it failed. This is a known issue.";

pub(crate) const ASSUME_NETWORK_MYSELF_ALIVE: &str = "network actor myself alive";

const ASSUME_GOSSIP_ACTOR_ALIVE: &str = "gossip actor must be alive";

// The duration for which we will try to maintain the number of peers in connection.
const MAINTAINING_CONNECTIONS_INTERVAL: Duration = Duration::from_secs(1200);

// The duration for which we will check all channels.
#[cfg(debug_assertions)]
// use a short interval for debugging build
pub(crate) const CHECK_CHANNELS_INTERVAL: Duration = Duration::from_secs(3);
#[cfg(not(debug_assertions))]
pub(crate) const CHECK_CHANNELS_INTERVAL: Duration = Duration::from_secs(60);

const CHECK_CHANNELS_SHUTDOWN_INTERVAL: Duration = Duration::from_secs(300);

// The duration for which we will check peer init messages.
const CHECK_PEER_INIT_INTERVAL: Duration = Duration::from_secs(20);

#[cfg(debug_assertions)]
const PEER_RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);
#[cfg(not(debug_assertions))]
const PEER_RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(2);
const PEER_RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

// While creating a network graph from the gossip messages, we will load current gossip messages
// in the store and process them. We will load all current messages and get the latest cursor.
// The problem is that we can't guarantee that the messages are in order, that is to say it is
// possible that messages with smaller cursor may arrive at the store from the time we create
// the graph. So we have to subscribe to gossip messages with a cursor slightly smaller than
// current latest cursor. This parameter is the difference between the cursor we use to subscribe
// and the latest cursor.
const MAX_GRAPH_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT: Duration =
    Duration::from_secs(60 * 60 * 2);

/// Maximum number of tries for a single funding step (initial try plus follow-up attempts after
/// transient failures). `retry_count` passed to handlers is zero-based (0 = first try).
const FUNDING_RETRY_MAX_TOTAL_ATTEMPTS: u32 = 5;
const FUNDING_RETRY_BASE_MILLIS: u64 = 2000;
const FUNDING_RETRY_MAX_MILLIS: u64 = 60_000;

/// Debounce interval for payment retry scans triggered by ChannelReady
/// (e.g. after reestablish). Prevents resource exhaustion when a peer
/// rapidly reconnects/reestablishes.
const CHANNEL_READY_RETRY_DEBOUNCE_MS: u64 = 60_000;

/// An owned permit for one inbound Fiber frame admitted into the NetworkActor.
///
/// The permit keeps the frame's message and byte capacity occupied while it is queued or being
/// processed. Dropping it returns that capacity to the global Fiber ingress budget.
pub struct FiberIngressPermit {
    peer_message_policy: Arc<StdMutex<PeerMessagePolicy>>,
    bytes: u64,
}

impl fmt::Debug for FiberIngressPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FiberIngressPermit")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl Drop for FiberIngressPermit {
    fn drop(&mut self) {
        match self.peer_message_policy.lock() {
            Ok(mut policy) => policy.release_ingress(self.bytes),
            Err(poisoned) => poisoned.into_inner().release_ingress(self.bytes),
        }
    }
}

#[derive(Debug)]
enum InboundFiberAdmission {
    Admitted(FiberIngressPermit),
    Disconnect,
    Ban,
}

fn admit_inbound_fiber_message(
    peer_message_policy: &Arc<StdMutex<PeerMessagePolicy>>,
    peer: &Pubkey,
    bytes: u64,
    now_ms: u64,
) -> InboundFiberAdmission {
    let decision = peer_message_policy
        .lock()
        .expect("peer message policy lock")
        .admit(peer, bytes, now_ms);
    match decision {
        PeerMessageAdmission::Allow => InboundFiberAdmission::Admitted(FiberIngressPermit {
            peer_message_policy: peer_message_policy.clone(),
            bytes,
        }),
        PeerMessageAdmission::Disconnect => InboundFiberAdmission::Disconnect,
        PeerMessageAdmission::Ban => InboundFiberAdmission::Ban,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelReadyRetryScanDecision {
    ScanNow,
    ScheduleTrailing(Duration),
    AlreadyScheduled,
}

fn decide_channel_ready_retry_scan(
    last_channel_ready_scan: &mut HashMap<OutPoint, u64>,
    pending_channel_ready_retry_scans: &mut HashSet<OutPoint>,
    channel_outpoint: OutPoint,
    now: u64,
) -> ChannelReadyRetryScanDecision {
    let should_scan = last_channel_ready_scan
        .get(&channel_outpoint)
        .is_none_or(|last| now.saturating_sub(*last) >= CHANNEL_READY_RETRY_DEBOUNCE_MS);

    if should_scan {
        last_channel_ready_scan.insert(channel_outpoint.clone(), now);
        pending_channel_ready_retry_scans.remove(&channel_outpoint);
        return ChannelReadyRetryScanDecision::ScanNow;
    }

    if !pending_channel_ready_retry_scans.insert(channel_outpoint.clone()) {
        return ChannelReadyRetryScanDecision::AlreadyScheduled;
    }

    let elapsed = last_channel_ready_scan
        .get(&channel_outpoint)
        .map(|last| now.saturating_sub(*last))
        .unwrap_or(0);
    let remaining = CHANNEL_READY_RETRY_DEBOUNCE_MS.saturating_sub(elapsed);

    ChannelReadyRetryScanDecision::ScheduleTrailing(Duration::from_millis(remaining))
}

fn funding_retry_delay(retry_count: u32) -> Duration {
    let shift = retry_count.min(63);
    let factor = 1u64 << shift;
    let delay = FUNDING_RETRY_BASE_MILLIS.saturating_mul(factor);
    Duration::from_millis(delay.min(FUNDING_RETRY_MAX_MILLIS))
}

fn should_reconcile_closed_channel_without_live_actor(channel_state: ChannelState) -> bool {
    matches!(
        channel_state,
        ChannelState::Closed(flags)
            if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
                && flags.intersects(
                    CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::UNCOOPERATIVE_REMOTE
                )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
            NetworkActorMessage::Fiber(FiberActorMessage::Command(
                FiberActorCommand::CheckChannels
            ))
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
}

/// Handles a `FundingError` with retry logic.  If the error is temporary and
/// retries remain, schedules a delayed retry via `send_after` using the
/// provided `retry_msg_fn` and returns `false`.  Otherwise logs the exhaustion
/// and returns `true` so the caller can perform its own abort.
fn schedule_funding_retry(
    myself: &FiberActorRef,
    err: &FundingError,
    retry_count: u32,
    channel_id: Hash256,
    operation: &str,
    retry_msg_fn: impl FnOnce(u32) -> FiberActorCommand + Send + 'static,
) -> bool {
    let attempt = retry_count + 1;
    error!(
        "Failed to {} (attempt {}/{}): {}",
        operation, attempt, FUNDING_RETRY_MAX_TOTAL_ATTEMPTS, err
    );
    if err.is_temporary() && attempt < FUNDING_RETRY_MAX_TOTAL_ATTEMPTS {
        let delay = funding_retry_delay(retry_count);
        warn!(
            "Temporary {} error, scheduling retry in {:?} (next attempt {}/{})",
            operation,
            delay,
            attempt + 1,
            FUNDING_RETRY_MAX_TOTAL_ATTEMPTS
        );
        let myself = myself.clone();
        myself.send_after(delay, move || {
            FiberActorMessage::new_command(retry_msg_fn(retry_count + 1))
        });
        false
    } else {
        if err.is_temporary() {
            error!(
                "Exhausted {} attempts for {}, aborting channel {:?}",
                FUNDING_RETRY_MAX_TOTAL_ATTEMPTS, operation, channel_id
            );
        }
        true
    }
}

static CHAIN_HASH_INSTANCE: OnceCell<Hash256> = OnceCell::new();

pub fn init_chain_hash(chain_hash: Hash256) {
    CHAIN_HASH_INSTANCE
        .set(chain_hash)
        .expect("init_chain_hash should only be called once");
}

pub fn get_chain_hash() -> Hash256 {
    CHAIN_HASH_INSTANCE.get().cloned().unwrap_or_default()
}

pub(crate) fn check_chain_hash(chain_hash: &Hash256) -> Result<(), Error> {
    if chain_hash == &get_chain_hash() {
        Ok(())
    } else {
        Err(Error::InvalidChainHash(*chain_hash, get_chain_hash()))
    }
}

fn compute_peer_reconnect_delay(attempt: u32) -> Duration {
    let shift = attempt.min(10);
    let factor = 1u32 << shift;
    PEER_RECONNECT_BACKOFF_BASE
        .checked_mul(factor)
        .unwrap_or(PEER_RECONNECT_BACKOFF_MAX)
        .min(PEER_RECONNECT_BACKOFF_MAX)
}

/// The index of active channels for each peer.
/// Note we maintain peer to channel index no matter the peer is connected or not.
#[derive(Clone, Default)]
pub struct PeerChannelIndex {
    inner: Arc<StdRwLock<PeerChannelIndexState>>,
}

#[derive(Default)]
struct PeerChannelIndexState {
    // Map from peer pubkey to the set of active channel ids
    peer_channels_map: HashMap<Pubkey, HashSet<Hash256>>,
    // Map from peer id to pubkey, used for dialing with peer id and maintaining peer reconnect backoff.
    peer_id_to_pubkey_map: HashMap<PeerId, Pubkey>,
    // Map from channel id to peer pubkey.
    channel_id_to_peer_map: HashMap<Hash256, Pubkey>,
    // Channel actors that have been accepted/created but have not reached ChannelReady yet.
    opening_channels: HashSet<Hash256>,
}

impl PeerChannelIndexState {
    fn build<S>(store: &S) -> Self
    where
        S: ChannelActorStateStore,
    {
        let mut peer_channels_map = HashMap::<Pubkey, HashSet<Hash256>>::new();
        let mut opening_channels = HashSet::new();
        for (pubkey, channel_id, channel_state) in store.get_active_channel_states(None) {
            peer_channels_map
                .entry(pubkey)
                .or_default()
                .insert(channel_id);
            if is_pending_channel_state(&channel_state) {
                opening_channels.insert(channel_id);
            }
        }
        let peer_id_to_pubkey_map = peer_channels_map
            .keys()
            .map(|pubkey| {
                let peer_id = pubkey_to_tentacle(*pubkey).peer_id();
                (peer_id, *pubkey)
            })
            .collect();
        let channel_id_to_peer_map = peer_channels_map
            .iter()
            .flat_map(|(pubkey, channels)| {
                channels
                    .iter()
                    .map(move |channel_id| (*channel_id, *pubkey))
            })
            .collect();
        Self {
            peer_channels_map,
            peer_id_to_pubkey_map,
            channel_id_to_peer_map,
            opening_channels,
        }
    }
}

impl PeerChannelIndex {
    pub(crate) fn build<S>(store: &S) -> Self
    where
        S: ChannelActorStateStore,
    {
        Self {
            inner: Arc::new(StdRwLock::new(PeerChannelIndexState::build(store))),
        }
    }

    pub(crate) fn add_channel(&self, pubkey: Pubkey, channel_id: Hash256) {
        let mut state = self.inner.write().expect("peer channel index write lock");
        state
            .peer_channels_map
            .entry(pubkey)
            .or_default()
            .insert(channel_id);
        let peer_id = pubkey_to_tentacle(pubkey).peer_id();
        state.peer_id_to_pubkey_map.entry(peer_id).or_insert(pubkey);
        state
            .channel_id_to_peer_map
            .entry(channel_id)
            .or_insert(pubkey);
    }

    pub(crate) fn remove_channel(&self, pubkey: &Pubkey, channel_id: &Hash256) {
        let mut state = self.inner.write().expect("peer channel index write lock");
        let mut is_empty = false;
        if let Some(channels) = state.peer_channels_map.get_mut(pubkey) {
            channels.remove(channel_id);
            if channels.is_empty() {
                is_empty = true;
            }
        }
        state.channel_id_to_peer_map.remove(channel_id);
        state.opening_channels.remove(channel_id);
        if is_empty {
            let peer_id = pubkey_to_tentacle(*pubkey).peer_id();
            state.peer_channels_map.remove(pubkey);
            state.peer_id_to_pubkey_map.remove(&peer_id);
        }
    }

    pub(crate) fn has_channels(&self, pubkey: &Pubkey) -> bool {
        self.inner
            .read()
            .expect("peer channel index read lock")
            .peer_channels_map
            .contains_key(pubkey)
    }

    pub(crate) fn has_channel(&self, pubkey: &Pubkey, channel_id: &Hash256) -> bool {
        self.inner
            .read()
            .expect("peer channel index read lock")
            .peer_channels_map
            .get(pubkey)
            .map(|channels| channels.contains(channel_id))
            .unwrap_or(false)
    }

    pub(crate) fn get_channels(&self, pubkey: &Pubkey) -> Option<HashSet<Hash256>> {
        self.inner
            .read()
            .expect("peer channel index read lock")
            .peer_channels_map
            .get(pubkey)
            .cloned()
    }

    pub(crate) fn replace_channel(&self, pubkey: Pubkey, old: Hash256, new: Hash256) {
        let mut state = self.inner.write().expect("peer channel index write lock");
        if let Some(channels) = state.peer_channels_map.get_mut(&pubkey) {
            channels.remove(&old);
            channels.insert(new);
            state.channel_id_to_peer_map.remove(&old);
            state.channel_id_to_peer_map.insert(new, pubkey);
            if state.opening_channels.remove(&old) {
                state.opening_channels.insert(new);
            }
        }
    }

    pub(crate) fn mark_channel_opening(&self, channel_id: Hash256) {
        self.inner
            .write()
            .expect("peer channel index write lock")
            .opening_channels
            .insert(channel_id);
    }

    pub(crate) fn mark_channel_ready(&self, channel_id: &Hash256) {
        self.inner
            .write()
            .expect("peer channel index write lock")
            .opening_channels
            .remove(channel_id);
    }

    pub(crate) fn opening_channel_count(&self) -> usize {
        self.inner
            .read()
            .expect("peer channel index read lock")
            .opening_channels
            .len()
    }

    pub(crate) fn opening_channel_count_by_peer(&self, pubkey: &Pubkey) -> usize {
        let state = self.inner.read().expect("peer channel index read lock");
        state.peer_channels_map.get(pubkey).map_or(0, |channels| {
            channels
                .iter()
                .filter(|channel_id| state.opening_channels.contains(channel_id))
                .count()
        })
    }

    pub(crate) fn get_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.inner
            .read()
            .expect("peer channel index read lock")
            .peer_id_to_pubkey_map
            .get(peer_id)
            .cloned()
    }

    pub(crate) fn get_peer_by_channel_id(&self, channel_id: &Hash256) -> Option<Pubkey> {
        self.inner
            .read()
            .expect("peer channel index read lock")
            .channel_id_to_peer_map
            .get(channel_id)
            .cloned()
    }
}

fn is_pending_channel_state(state: &ChannelState) -> bool {
    matches!(
        state,
        ChannelState::NegotiatingFunding(_)
            | ChannelState::CollaboratingFundingTx(_)
            | ChannelState::SigningCommitment(_)
            | ChannelState::AwaitingTxSignatures(_)
            | ChannelState::AwaitingChannelReady(_)
    )
}

#[derive(Debug)]
pub enum PeerDisconnectReason {
    /// User request disconnection.
    Requested,
    /// Init message timeout.
    InitMessageTimeout,
    /// Chain hash mismatch.
    ChainHashMismatch,
    /// Duplicate Init message.
    DuplicateInitMessage,
    /// Gossip peer temporarily banned.
    Banned,
}

#[derive(Debug, Clone, Copy)]
pub enum PeerReconnectTrigger {
    Disconnected,
    DialError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectSource {
    Manual,
    Automatic,
}

#[derive(Debug)]
pub struct OpenChannelResponse {
    pub channel_id: Hash256,
}

#[derive(Debug)]
pub struct AcceptChannelResponse {
    pub old_channel_id: Hash256,
    pub new_channel_id: Hash256,
}

/// A channel that has been received from a remote peer but not yet accepted locally.
/// These are held in `to_be_accepted_channels` waiting for a manual `accept_channel` call.
#[derive(Debug, Clone)]
pub struct PendingAcceptChannel {
    /// The temporary channel ID assigned by the initiator.
    pub channel_id: Hash256,
    /// The public key of the channel initiator.
    pub pubkey: Pubkey,
    /// The amount of CKB or UDT the initiator is contributing to the channel.
    pub funding_amount: u128,
    /// UDT type script, if this is a UDT channel.
    pub udt_type_script: Option<Script>,
    /// Timestamp (milliseconds since UNIX epoch) when this channel request was received.
    pub created_at: u64,
}

#[derive(Debug)]
pub struct SendPaymentResponse {
    pub payment_hash: Hash256,
    /// The preimage learned from a successful attempt, if the payment completed.
    pub payment_preimage: Option<Hash256>,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub failed_error: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) failed_error_code: Option<TlcErrorCode>,
    pub custom_records: Option<PaymentCustomRecords>,
    pub fee: u128,
    #[cfg(any(debug_assertions, test, feature = "bench"))]
    pub routers: Vec<SessionRoute>,
}

/// What kind of local information should be broadcasted to the network.
#[derive(Debug)]
pub enum LocalInfoKind {
    NodeAnnouncement,
}

#[derive(Debug, Clone)]
pub struct NodeInfoResponse {
    pub node_name: Option<AnnouncedNodeName>,
    pub node_id: Pubkey,
    pub addresses: Vec<MultiAddr>,
    pub features: FeatureVector,
    pub chain_hash: Hash256,
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,
    pub auto_accept_channel_ckb_funding_amount: u64,
    pub tlc_expiry_delta: u64,
    pub tlc_min_value: u128,
    pub tlc_fee_proportional_millionths: u128,
    pub channel_count: u32,
    pub pending_channel_count: u32,
    pub peers_count: u32,
    pub udt_cfg_infos: UdtCfgInfos,
}

/// The information about a peer connected to the node.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PeerInfo {
    /// The identity public key of the peer (also known as `node_id`).
    pub pubkey: Pubkey,

    /// The multi-address associated with the connecting peer.
    /// Note: this is only the address which used for connecting to the peer, not all addresses of the peer.
    /// The `graph_nodes` in Graph rpc module will return all addresses of the peer.
    pub address: MultiAddr,
}

#[derive(Debug, Clone)]
pub struct SendOnionPacketCommand {
    pub peeled_onion_packet: PeeledPaymentOnionPacket,
    // We are currently forwarding a previous tlc. The previous tlc's channel id, tlc id
    // and the fee paid are included here.
    pub previous_tlc: Option<PrevTlcInfo>,
    pub payment_hash: Hash256,
    pub attempt_id: Option<u64>,
}

#[cfg(test)]
#[derive(Debug)]
pub enum TestFiberMessageKind {
    AddTlc,
    CommitmentSigned,
    RevokeAndAck,
}

#[cfg(test)]
#[derive(Debug)]
pub struct TestFiberMessageHold {
    pub target: Pubkey,
    pub channel_id: Hash256,
    pub kind: TestFiberMessageKind,
    pub remaining: NonZeroUsize,
}

#[cfg(test)]
impl TestFiberMessageHold {
    fn matches(&self, message: &FiberMessageWithTarget) -> bool {
        if message.target != self.target {
            return false;
        }
        let FiberMessage::ChannelNormalOperation(channel_message) = &message.message else {
            return false;
        };
        if channel_message.get_channel_id() != self.channel_id {
            return false;
        }
        matches!(
            (&self.kind, channel_message),
            (TestFiberMessageKind::AddTlc, FiberChannelMessage::AddTlc(_))
                | (
                    TestFiberMessageKind::CommitmentSigned,
                    FiberChannelMessage::CommitmentSigned(_)
                )
                | (
                    TestFiberMessageKind::RevokeAndAck,
                    FiberChannelMessage::RevokeAndAck(_)
                )
        )
    }
}

/// Commands owned by the public P2P runtime.
///
/// A hosted tenant actor never accepts this type, which prevents public peer,
/// gossip, and announcement work from entering a tenant mailbox.
#[derive(Debug, AsRefStr)]
pub enum PublicNetworkCommand {
    // Connect to a peer, and optionally also save the peer to the peer store.
    ConnectPeer(
        Multiaddr,
        bool,
        PeerConnectSource,
        Option<RpcReplyPort<Result<(), String>>>,
    ),
    // Connect to a peer via pubkey, resolving address from local graph/saved state.
    // The optional TransportType filters addresses by transport type (e.g. Wss for WASM).
    ConnectPeerWithPubkey(
        Pubkey,
        Option<TransportType>,
        PeerConnectSource,
        RpcReplyPort<Result<(), String>>,
    ),
    DisconnectPeer(
        Pubkey,
        PeerDisconnectReason,
        Option<RpcReplyPort<Result<(), String>>>,
    ),
    SeedPeerReconnectBackoff(PeerId, PeerReconnectTrigger),
    PeerReconnectBackoffTick(PeerId, u32),
    // Save the address of a peer to the peer store, the address here must be a valid
    // multiaddr with the peer id.
    SavePeerAddress(Multiaddr),
    // Remove queued save addresses for a peer when dialing fails.
    RemovePendingSavePeerAddress(PeerId),
    // We need to maintain a certain number of peers connections to keep the network running.
    MaintainConnections,
    // Check peer send us Init message in an expected time, otherwise disconnect with the peer.
    CheckPeerInit(Pubkey, SessionId),
    // Pace persisted channel reestablishment without blocking the NetworkActor. The channel ids
    // stay in one heap allocation while each continuation only moves the Vec header.
    ReestablishChannels(Pubkey, SessionId, Vec<Hash256>),
    // Broadcast our BroadcastMessage to the network.
    BroadcastMessages(Vec<BroadcastMessageWithTimestamp>),
    // Broadcast local information to the network.
    BroadcastLocalInfo(LocalInfoKind),
    NodeInfo((), RpcReplyPort<Result<NodeInfoResponse, String>>),
    ListPeers((), RpcReplyPort<Result<Vec<PeerInfo>, String>>),
    #[cfg(not(target_arch = "wasm32"))]
    SetLspService(ActorRef<LspServiceMessage>),
    #[cfg(any(debug_assertions, feature = "bench"))]
    UpdateFeatures(FeatureVector),
}

/// Commands owned by the Fiber channel, payment, and invoice data plane.
///
/// Commands that require a public P2P runtime belong to [`PublicNetworkCommand`]
/// instead. Keeping the two enums disjoint makes a hosted tenant incapable of
/// receiving public-runtime work at the type level.
#[derive(Debug, AsRefStr)]
pub enum FiberActorCommand {
    /// Register a co-located Fiber endpoint. Messages to this peer are
    /// delivered directly to its actor without Tentacle encoding or a socket.
    RegisterInProcessPeer {
        pubkey: Pubkey,
        actor: FiberActorRef,
        features: FeatureVector,
        reply: RpcReplyPort<Result<(), String>>,
    },
    /// Start channel reestablishment after both directions of an in-process
    /// route have been registered.
    ActivateInProcessPeer(Pubkey, RpcReplyPort<Result<(), String>>),
    /// Remove a previously registered co-located Fiber endpoint.
    UnregisterInProcessPeer(Pubkey),
    // Check hold tlcs that have expired and need to be removed.
    CheckChannels,
    // Timeout a hold tlc
    TimeoutHoldTlc(Hash256, Hash256, u64),
    // Settle tlc set by given a list of `(channel_id, tlc_id)`
    SettleTlcSet(Hash256, Vec<(Hash256, u64)>),
    // Settle hold tlc set saved for a payment hash when a new TLC arrives.
    SettleHoldTlcSet(Hash256),
    // Retry settling a hold tlc set after the invoice has already been marked Received.
    SettleReceivedHoldTlcSet(Hash256),
    // Settle an invoice from received TLCs already reconciled as fulfilled on-chain.
    SettleOnChainFulfilledInvoice(Hash256),
    /// Reconcile one origin-payer attempt from a channel-scoped on-chain fulfill proof.
    ReconcileOnChainPayerTlc {
        channel_id: Hash256,
        tlc_id: TLCId,
        payment_hash: Hash256,
        attempt_id: u64,
        payment_preimage: Hash256,
        reply: RpcReplyPort<Result<(), String>>,
    },
    /// Relay a RemoveTlc for an on-chain-resolved downstream TLC to its upstream channel.
    /// The downstream TLC is finalized only after this is delivered or durably queued.
    RelayOnChainTlcRemove {
        downstream_channel_id: Hash256,
        downstream_tlc_id: TLCId,
        forwarding_channel_id: Hash256,
        forwarding_tlc_id: u64,
        payment_hash: Hash256,
        reason: RemoveTlcReason,
    },
    /// Completion of an asynchronously forwarded on-chain RemoveTlc relay.
    RelayOnChainTlcRemoveResult {
        downstream_channel_id: Hash256,
        downstream_tlc_id: TLCId,
        forwarding_channel_id: Hash256,
        forwarding_tlc_id: u64,
        payment_hash: Hash256,
        reason: RemoveTlcReason,
        result: Result<(), ProcessingChannelError>,
    },
    /// Completion of an asynchronously forwarded RemoveTlc command.
    RemoveTlcResult {
        channel_id: Hash256,
        tlc_id: u64,
        hold_payment_hash: Option<Hash256>,
        result: Result<(), ProcessingChannelError>,
    },
    #[cfg(test)]
    InstallTestChannelActor(Hash256, ActorRef<ChannelActorMessage>, RpcReplyPort<()>),
    #[cfg(test)]
    SetTestFiberMessageHold(TestFiberMessageHold, RpcReplyPort<()>),
    #[cfg(test)]
    TakeTestHeldFiberMessages(RpcReplyPort<Vec<FiberMessageWithTarget>>),
    #[cfg(test)]
    ReleaseTestHeldFiberMessages(RpcReplyPort<Result<(), String>>),
    #[cfg(test)]
    GetTestHeldFiberMessageCount(RpcReplyPort<usize>),
    #[cfg(test)]
    SetTestTrampolineSettlementPaused(bool, RpcReplyPort<()>),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendFiberMessage(FiberMessageWithTarget),
    // Open a channel to a peer.
    OpenChannel(
        OpenChannelCommand,
        RpcReplyPort<Result<OpenChannelResponse, String>>,
    ),
    // Abandon a channel, channel_id maybe temp_channel_id or normal channel_id
    AbandonChannel(Hash256, RpcReplyPort<Result<(), String>>),
    // Accept a channel to a peer.
    AcceptChannel(
        AcceptChannelCommand,
        RpcReplyPort<Result<AcceptChannelResponse, String>>,
    ),
    // Send a command to a channel.
    ControlFiberChannel(ChannelCommandWithId),
    #[cfg(any(test, feature = "bench"))]
    GetChannelActor(Hash256, RpcReplyPort<Option<ActorRef<ChannelActorMessage>>>),
    // Send an onion packet to the next hop. The `PeeledPaymentOnionPacket::current` contains
    // the hop data for the current node.
    SendPaymentOnionPacket(SendOnionPacketCommand, RpcReplyPort<Result<(), TlcErr>>),
    UpdateChannelFunding(Hash256, Transaction, FundingRequest),
    VerifyFundingTx {
        peer: Pubkey,
        local_tx: Transaction,
        remote_tx: Transaction,
        funding_cell_lock_script: Script,
        funding_udt_type_script: Option<Script>,
        funding_source_lock_script: Option<Script>,
        reply: RpcReplyPort<Result<(), FundingError>>,
    },
    SignFundingTx(Pubkey, Hash256, Transaction, Option<Vec<Vec<u8>>>),
    RetryUpdateChannelFunding(Hash256, Transaction, FundingRequest, u32),
    RetrySignFundingTx(Pubkey, Hash256, Transaction, Option<Vec<Vec<u8>>>, u32),
    NotifyFundingTx(Transaction),
    CheckChannelsShutdown,
    CheckChannelShutdown(Hash256, RpcReplyPort<Result<(), String>>),
    RemoteForceShutdownChannel(Hash256, Option<GetShutdownTxResponse>),
    // Payment related commands
    SendPayment(
        SendPaymentCommand,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    // Send payment with router
    SendPaymentWithRouter(
        SendPaymentWithRouterCommand,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    // Get Payment Session for query payment status and errors
    GetPayment(Hash256, RpcReplyPort<Result<SendPaymentResponse, String>>),
    #[cfg(not(target_arch = "wasm32"))]
    GetHostedTenantActivity(RpcReplyPort<HostedTenantActivity>),
    InspectBufferedTrampolineUpstream {
        request: TrampolineForwardingRequest,
        reply: RpcReplyPort<BufferedTrampolineUpstreamStatus>,
    },
    #[cfg(not(target_arch = "wasm32"))]
    DispatchBufferedTrampoline {
        request: TrampolineForwardingRequest,
        reply: RpcReplyPort<Result<(), LspPaymentDispatchError>>,
    },
    #[cfg(not(target_arch = "wasm32"))]
    ReconcileBufferedTrampolineSettlement {
        payment_hash: Hash256,
        reply: RpcReplyPort<Result<(), String>>,
    },
    #[cfg(not(target_arch = "wasm32"))]
    FailBufferedTrampoline {
        request: TrampolineForwardingRequest,
        reason: String,
        error_code: TlcErrorCode,
        reply: RpcReplyPort<Result<bool, String>>,
    },
    // Build a payment router with the given hops
    BuildPaymentRouter(
        BuildRouterCommand,
        RpcReplyPort<Result<PaymentRouter, String>>,
    ),
    // Get the count of inflight payments
    GetInflightPaymentCount(RpcReplyPort<Result<u32, String>>),

    AddInvoice(
        CkbInvoice,
        Option<Hash256>,
        RpcReplyPort<Result<(), InvoiceError>>,
    ),
    GetInvoice(
        Hash256,
        RpcReplyPort<Result<(CkbInvoice, CkbInvoiceStatus), InvoiceError>>,
    ),

    SettleInvoice(
        Hash256,
        Hash256,
        RpcReplyPort<Result<(), SettleInvoiceError>>,
    ),
    CancelInvoice(Hash256, RpcReplyPort<Result<(), CancelInvoiceError>>),

    // Get all inbound channel requests that are waiting for `accept_channel`
    GetPendingAcceptChannels(RpcReplyPort<Result<Vec<PendingAcceptChannel>, String>>),
    // Open a channel with external funding - the funding transaction will be returned
    // for the user to sign with their own wallet.
    OpenChannelWithExternalFunding(
        OpenChannelWithExternalFundingCommand,
        RpcReplyPort<Result<OpenChannelWithExternalFundingResponse, String>>,
    ),
    // Submit a signed funding transaction for external funding.
    SubmitSignedFundingTx {
        channel_id: Hash256,
        signed_tx: Transaction,
        reply: RpcReplyPort<Result<Hash256, String>>,
    },
}

pub fn sign_network_message(private_key: &Privkey, message: [u8; 32]) -> EcdsaSignature {
    debug!(
        "Signing message with node private key: message {:?}, public key {:?}",
        message,
        private_key.pubkey()
    );
    private_key.sign(message)
}

#[derive(Debug)]
pub struct OpenChannelCommand {
    pub pubkey: Pubkey,
    pub funding_amount: u128,
    pub public: bool,
    pub one_way: bool,
    pub shutdown_script: Option<Script>,
    pub funding_udt_type_script: Option<Script>,
    pub commitment_fee_rate: Option<u64>,
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    pub funding_fee_rate: Option<u64>,
    pub tlc_expiry_delta: Option<u64>,
    pub tlc_min_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
}

/// Command to open a channel with external funding.
/// Similar to OpenChannelCommand, but the user will sign the funding transaction
/// with their own wallet instead of having the node sign automatically.
#[derive(Debug)]
pub struct OpenChannelWithExternalFundingCommand {
    pub pubkey: Pubkey,
    pub funding_amount: u128,
    pub public: bool,
    /// Required for external funding - the script to receive funds when channel closes.
    pub shutdown_script: Script,
    /// The lock script that controls the funding cells (user's wallet lock script).
    pub funding_lock_script: Script,
    /// Optional extra cell deps required to use `funding_lock_script`.
    pub funding_lock_script_cell_deps: Vec<packed::CellDep>,
    pub funding_udt_type_script: Option<Script>,
    pub commitment_fee_rate: Option<u64>,
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    pub funding_fee_rate: Option<u64>,
    pub tlc_expiry_delta: Option<u64>,
    pub tlc_min_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
    pub external_channel_signer: Option<ChannelOpenSignerMaterial>,
}
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRouterCommand {
    /// the amount of the payment, the unit is Shannons for non UDT payment
    pub amount: Option<u128>,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub hops_info: Vec<HopRequire>,
    pub final_tlc_expiry_delta: Option<u64>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentRouter {
    pub router_hops: Vec<RouterHop>,
}

#[derive(Debug)]
pub struct AcceptChannelCommand {
    pub temp_channel_id: Hash256,
    pub funding_amount: u128,
    pub shutdown_script: Option<Script>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
    pub min_tlc_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub tlc_expiry_delta: Option<u64>,
}

/// Response for opening a channel with external funding.
#[derive(Debug, Clone)]
pub struct OpenChannelWithExternalFundingResponse {
    /// The temporary channel ID.
    pub channel_id: Hash256,
    /// The unsigned funding transaction for the user to sign.
    pub unsigned_funding_tx: Transaction,
}

#[cfg(any(debug_assertions, feature = "bench"))]
#[derive(Clone, Debug)]
pub enum DebugEvent {
    // A AddTlc peer message processed with failure
    AddTlcFailed(Pubkey, Hash256, TlcErr),
    // Common event with string
    Common(String),
}

#[macro_export]
macro_rules! debug_event {
    ($network:expr, $debug_event:expr) => {
        #[cfg(any(debug_assertions, feature = "bench"))]
        $network
            .send_message($crate::fiber::network::FiberActorMessage::new_notification(
                $crate::fiber::network::NetworkServiceEvent::DebugEvent(
                    $crate::fiber::network::DebugEvent::Common($debug_event.to_string()),
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    };
}

#[derive(Clone, Debug, AsRefStr)]
pub enum NetworkServiceEvent {
    NetworkStarted(Pubkey, Vec<MultiAddr>, Vec<Multiaddr>),
    NetworkStopped(Pubkey),
    PeerConnected(Pubkey, Multiaddr),
    PeerDisConnected(Pubkey, Multiaddr),
    // An incoming/outgoing channel is created.
    ChannelCreated(Pubkey, Hash256),
    // An incoming channel is pending to be accepted.
    ChannelPendingToBeAccepted(Pubkey, Hash256),
    // A funding tx is completed. The watch tower may use this to monitor the channel.
    RemoteTxComplete(
        Pubkey,
        Hash256,
        Option<Script>,
        Option<Privkey>,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        SettlementData,
    ),
    // The channel is ready to use (with funding transaction confirmed
    // and both parties sent ChannelReady messages).
    ChannelReady(Pubkey, Hash256, OutPoint),
    // The channel connectivity is online and normal operations may resume.
    ChannelOnline(Pubkey, Hash256, OutPoint),
    // The channel connectivity is offline and normal operations are paused.
    ChannelOffline(Pubkey, Hash256, OutPoint),
    ChannelClosed(Pubkey, Hash256, Byte32),
    ChannelAbandon(Hash256),
    ChannelFundingAborted(Hash256),
    // A RevokeAndAck is received from the peer. Other data relevant to this
    // RevokeAndAck message are also assembled here. The watch tower may use this.
    RevokeAndAckReceived(
        Pubkey,  /* Peer pubkey */
        Hash256, /* Channel Id */
        RevocationData,
        SettlementData,
    ),
    // The other party has signed a valid commitment transaction,
    // and we successfully assemble the partial signature from other party
    // to create a complete commitment transaction and a settlement transaction.
    RemoteCommitmentSigned(Pubkey, Hash256, TransactionView, SettlementData),
    // We have signed a valid commitment transaction, and the other party may use
    // the signature we sent to them to create a complete commitment transaction
    LocalCommitmentSigned(Hash256, SettlementData),
    // Preimage is created for the payment hash, the first Hash256 is the payment hash,
    // and the second Hash256 is the preimage.
    PreimageCreated(Hash256, Hash256),
    // Preimage is removed for the payment hash.
    PreimageRemoved(Hash256),
    // Some other debug event for assertion.
    #[cfg(any(debug_assertions, feature = "bench"))]
    DebugEvent(DebugEvent),
}

/// Events owned by the public P2P runtime.
///
/// In-process Fiber messages bypass this type and enter the data plane as
/// [`FiberActorEvent::PeerMessage`].
#[derive(Debug, AsRefStr)]
pub enum PublicNetworkEvent {
    PeerConnected(Pubkey, SessionContext),
    PeerDisconnected(Pubkey, SessionContext),
    /// A Fiber protocol message from a peer. Network ingress messages carry a permit that keeps
    /// their global queue capacity occupied until handling completes; internally injected
    /// messages do not need one.
    FiberMessage(Pubkey, FiberMessage, Option<FiberIngressPermit>),

    // Some gossip messages have been updated in the gossip message store.
    // Normally we need to propagate these messages to the network graph.
    GossipMessageUpdates(GossipMessageUpdates),
}

/// Events owned by the Fiber channel and payment data plane.
#[derive(Debug, AsRefStr)]
pub enum FiberActorEvent {
    /// A decoded Fiber message from an already authenticated in-process peer.
    PeerMessage(Pubkey, FiberMessage),

    /// Channel related events.
    /// A channel has been accepted.
    /// The two Hash256 are respectively newly agreed channel id and temp channel id,
    /// The two u128 are respectively local and remote funding amount,
    /// and the script is the lock script of the agreed funding cell.
    ChannelAccepted(
        Pubkey,
        Hash256,
        Hash256,
        u128,
        u128,
        Script,
        Option<Script>,
        u64,
        u64,
        u64,
    ),
    /// A channel with external funding has been accepted.
    /// This is used when the user wants to sign the funding transaction themselves.
    ChannelAcceptedForExternalFunding {
        peer_id: PeerId,
        new_channel_id: Hash256,
        old_channel_id: Hash256,
        funding_amount: u128,
        remote_funding_amount: u128,
        /// The lock script of the user's wallet, used to collect input cells.
        funding_source_lock_script: Script,
        /// Optional extra deps required by the user's funding lock script.
        funding_source_lock_script_cell_deps: Vec<packed::CellDep>,
        /// The 2-of-2 multisig lock script for the funding cell output.
        funding_cell_lock_script: Script,
        funding_udt_type_script: Option<Script>,
        local_reserved_ckb_amount: u64,
        remote_reserved_ckb_amount: u64,
        funding_fee_rate: u64,
    },
    /// The final unsigned external funding transaction has been negotiated and is ready
    /// for the user to sign without changing its structure.
    ExternalFundingTxReady(Hash256, Transaction),
    /// A channel is ready to use.
    ChannelReady(Hash256, Pubkey, OutPoint),
    /// Retry pending payment attempts for a ChannelReady outpoint after debounce.
    RetryPendingPaymentsForChannel(OutPoint),
    /// A channel is going to be closed, waiting the closing transaction to be broadcasted and confirmed.
    ClosingTransactionPending(Hash256, Pubkey, TransactionView, bool),

    /// Both parties are now able to broadcast a valid funding transaction.
    FundingTransactionPending(Transaction, OutPoint, Hash256),

    /// A funding transaction has been confirmed. The transaction was included in the
    /// block with the given transaction index, and the timestamp in the block header.
    FundingTransactionConfirmed(OutPoint, H256, u32, u64),

    /// A funding transaction has failed.
    FundingTransactionFailed(OutPoint),

    /// A closing transaction has been confirmed (pubkey, channel_id, tx_hash, force, close_by_us).
    ClosingTransactionConfirmed(Pubkey, Hash256, Byte32, bool, bool),

    /// A closing transaction has failed (either because of invalid transaction or timeout)
    ClosingTransactionFailed(Pubkey, Hash256, Byte32),

    // A tlc remove message is received. (payment_hash, attempt_id, remove_tlc)
    TlcRemoveReceived(Hash256, Option<u64>, RemoveTlcReason),

    // A payment need to retry
    RetrySendPayment(Hash256, Option<u64>),

    // AddTlc result from peer (payment_hash, attempt_id, add_tlc_result, (previous_channel_id, previous_tlc_id))
    AddTlcResult(
        Hash256,
        Option<u64>,
        Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
        Option<PrevTlcInfo>,
    ),

    // An owned channel is updated.
    OwnedChannelUpdateEvent(OwnedChannelUpdateEvent),

    // A channel actor stopped event.
    ChannelActorStopped(Hash256, StopReason),

    // A payment actor stopped event.
    PaymentActorStopped(Hash256, Option<TlcErrPacket>),

    // Channel settlement check completed - channel is fully settled on-chain.
    ChannelSettlementCompleted(Hash256),
}

#[derive(Debug)]
pub enum FiberActorMessage {
    Command(FiberActorCommand),
    Event(FiberActorEvent),
    Notification(NetworkServiceEvent),
}

impl FiberActorMessage {
    pub fn new_command(command: FiberActorCommand) -> Self {
        Self::Command(command)
    }

    pub fn new_event(event: FiberActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_notification(event: NetworkServiceEvent) -> Self {
        Self::Notification(event)
    }
}

impl Display for FiberActorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(command) => write!(f, "Command.{}", command.as_ref()),
            Self::Event(event) => write!(f, "Event.{}", event.as_ref()),
            Self::Notification(event) => write!(f, "Notification.{}", event.as_ref()),
        }
    }
}

/// Public node mailbox. Public-runtime work and Fiber data-plane work have
/// different envelope variants, so dispatch does not need to classify the
/// command again in `NetworkActor::handle`.
#[derive(Debug)]
pub enum NetworkActorMessage {
    PublicCommand(PublicNetworkCommand),
    PublicEvent(PublicNetworkEvent),
    Fiber(FiberActorMessage),
}

impl NetworkActorMessage {
    pub fn new_command(command: impl Into<Self>) -> Self {
        command.into()
    }

    pub fn new_event(event: impl Into<Self>) -> Self {
        event.into()
    }

    pub fn new_notification(event: NetworkServiceEvent) -> Self {
        Self::Fiber(FiberActorMessage::new_notification(event))
    }
}

impl From<FiberActorMessage> for NetworkActorMessage {
    fn from(message: FiberActorMessage) -> Self {
        Self::Fiber(message)
    }
}

impl From<FiberActorCommand> for NetworkActorMessage {
    fn from(command: FiberActorCommand) -> Self {
        Self::Fiber(FiberActorMessage::new_command(command))
    }
}

impl From<PublicNetworkCommand> for NetworkActorMessage {
    fn from(command: PublicNetworkCommand) -> Self {
        Self::PublicCommand(command)
    }
}

impl From<FiberActorEvent> for NetworkActorMessage {
    fn from(event: FiberActorEvent) -> Self {
        Self::Fiber(FiberActorMessage::new_event(event))
    }
}

impl From<PublicNetworkEvent> for NetworkActorMessage {
    fn from(event: PublicNetworkEvent) -> Self {
        Self::PublicEvent(event)
    }
}

impl TryFrom<NetworkActorMessage> for FiberActorMessage {
    type Error = NetworkActorMessage;

    fn try_from(message: NetworkActorMessage) -> Result<Self, Self::Error> {
        match message {
            NetworkActorMessage::Fiber(message) => Ok(message),
            message => Err(message),
        }
    }
}

impl TryFrom<NetworkActorMessage> for PublicNetworkCommand {
    type Error = NetworkActorMessage;

    fn try_from(message: NetworkActorMessage) -> Result<Self, Self::Error> {
        match message {
            NetworkActorMessage::PublicCommand(command) => Ok(command),
            message => Err(message),
        }
    }
}

impl Display for NetworkActorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicCommand(command) => write!(f, "PublicCommand.{}", command.as_ref()),
            Self::PublicEvent(event) => write!(f, "PublicEvent.{}", event.as_ref()),
            Self::Fiber(message) => message.fmt(f),
        }
    }
}

/// Restricted handle accepted by the Fiber channel/payment data plane.
#[derive(Clone, Debug)]
pub struct FiberActorRef {
    actor: DerivedActorRef<FiberActorMessage>,
    public_network: Option<DerivedActorRef<PublicNetworkCommand>>,
}

impl FiberActorRef {
    pub fn from_network(actor: &ActorRef<NetworkActorMessage>) -> Self {
        Self {
            actor: actor.get_derived(),
            public_network: Some(actor.get_derived()),
        }
    }

    pub fn from_fiber(actor: &ActorRef<FiberActorMessage>) -> Self {
        Self {
            actor: actor.get_derived(),
            public_network: None,
        }
    }

    /// Sends work through the public P2P runtime when this data plane is
    /// attached to one. Hosted tenants deliberately have no such capability.
    pub fn send_public_command(&self, command: PublicNetworkCommand) -> Result<(), String> {
        let actor = self
            .public_network
            .as_ref()
            .ok_or_else(|| "public network service is unavailable".to_string())?;
        actor
            .send_message(command)
            .map_err(|error| error.to_string())
    }

    fn send_public_after(
        &self,
        delay: Duration,
        command: impl FnOnce() -> PublicNetworkCommand + Send + 'static,
    ) {
        if let Some(actor) = self.public_network.as_ref() {
            actor.send_after(delay, command);
        }
    }
}

impl std::ops::Deref for FiberActorRef {
    type Target = DerivedActorRef<FiberActorMessage>;

    fn deref(&self) -> &Self::Target {
        &self.actor
    }
}

impl PartialEq for FiberActorRef {
    fn eq(&self, other: &Self) -> bool {
        self.actor.get_cell() == other.actor.get_cell()
    }
}

impl Eq for FiberActorRef {}

#[derive(Debug)]
pub struct FiberMessageWithTarget {
    pub target: Pubkey,
    pub message: FiberMessage,
}

impl FiberMessageWithTarget {
    pub fn new(target: Pubkey, message: FiberMessage) -> Self {
        Self { target, message }
    }
}

#[derive(Debug)]
pub struct GossipMessageWithTarget {
    pub target: Pubkey,
    pub message: GossipMessage,
}

impl GossipMessageWithTarget {
    pub fn new(target: Pubkey, message: GossipMessage) -> Self {
        Self { target, message }
    }
}

/// Shared Fiber channel/payment data plane.
///
/// This core deliberately has no P2P actor identity of its own. Both
/// the public `NetworkActor` and a local-only `HostedTenantActor` drive it with
/// their respective runtime state.
pub(crate) struct FiberActorCore<S, C> {
    // An event emitter to notify outside observers.
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    chain_actor: ActorRef<CkbChainMessage>,
    store: S,
    store_actor: Option<ActorRef<StoreActorMessage>>,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    chain_client: C,
}

struct PublicNetworkRuntimeState {
    state_to_be_persisted: PersistentNetworkActorState,
    node_name: Option<AnnouncedNodeName>,
    announced_addrs: Vec<Multiaddr>,
    auto_announce: bool,
    last_node_announcement_message: Option<NodeAnnouncement>,
    control: ServiceAsyncControl,
    peer_message_policy: Arc<StdMutex<PeerMessagePolicy>>,
    #[cfg(not(target_arch = "wasm32"))]
    onion_service_token: Option<tokio_util::sync::CancellationToken>,
    peer_session_map: HashMap<Pubkey, ConnectedPeer>,
    pending_save_peer_addresses: HashMap<PeerId, Vec<Multiaddr>>,
    gossip_actor: Option<ActorRef<GossipActorMessage>>,
    max_inbound_peers: usize,
    min_outbound_peers: usize,
    enable_peer_reconnect_backoff: bool,
    peer_reconnect_backoff_attempts: HashMap<Pubkey, u32>,
    requested_disconnect_peers: HashSet<Pubkey>,
}

struct FiberActorStateArgs {
    private_key: Privkey,
    entropy: [u8; 32],
    default_shutdown_script: Script,
    network: FiberActorRef,
    peer_channel_index: PeerChannelIndex,
    features: FeatureVector,
}

impl<S, C> FiberActorCore<S, C>
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
    pub fn new(
        event_sender: mpsc::Sender<NetworkServiceEvent>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        store_actor: Option<ActorRef<StoreActorMessage>>,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
        chain_client: C,
    ) -> Self {
        Self {
            event_sender,
            chain_actor,
            store: store.clone(),
            store_actor,
            network_graph,
            chain_client,
        }
    }

    fn build_actor_state(
        &self,
        config: &FiberConfig,
        args: FiberActorStateArgs,
    ) -> FiberActorState<S, C> {
        let FiberActorStateArgs {
            private_key,
            entropy,
            default_shutdown_script,
            network,
            peer_channel_index,
            features,
        } = args;
        let mut pending_trampoline_settlements: HashMap<Hash256, HashSet<Hash256>> = HashMap::new();
        for session in self.store.get_all_payment_sessions() {
            let Some(context) = session.request.trampoline_context.as_ref() else {
                continue;
            };
            if !session.status.is_final() {
                continue;
            }
            for previous_tlc in &context.previous_tlcs {
                let unresolved = self
                    .store
                    .get_channel_actor_state(&previous_tlc.prev_channel_id)
                    .is_some_and(|state| {
                        trampoline_upstream_tlc_needs_settlement(
                            &state,
                            session.request.payment_hash,
                            previous_tlc,
                        )
                    });
                if unresolved {
                    pending_trampoline_settlements
                        .entry(previous_tlc.prev_channel_id)
                        .or_default()
                        .insert(session.request.payment_hash);
                }
            }
        }

        FiberActorState {
            store: self.store.clone(),
            store_actor: self.store_actor.clone(),
            private_key,
            entropy,
            default_shutdown_script,
            network,
            p2p_peers: Default::default(),
            p2p_peer_features: Default::default(),
            in_process_peers: Default::default(),
            peer_channel_index,
            channels: Default::default(),
            channels_funding_lock_script_cache: Default::default(),
            outpoint_channel_map: Default::default(),
            to_be_accepted_channels: ToBeAcceptedChannels::new_with_config(config),
            pending_channels: Default::default(),
            chain_actor: self.chain_actor.clone(),
            chain_client: self.chain_client.clone(),
            open_channel_auto_accept_min_ckb_funding_amount: config
                .open_channel_auto_accept_min_ckb_funding_amount(),
            auto_accept_channel_ckb_funding_amount: config.auto_accept_channel_ckb_funding_amount(),
            pending_channels_number_limit: config
                .pending_channels_number_limit
                .unwrap_or(DEFAULT_PENDING_CHANNELS_NUMBER_LIMIT),
            tlc_expiry_delta: config.tlc_expiry_delta(),
            tlc_min_value: config.tlc_min_value(),
            tlc_fee_proportional_millionths: config.tlc_fee_proportional_millionths(),
            features,
            channel_ephemeral_config: ChannelEphemeralConfig {
                funding_timeout_seconds: config.funding_timeout_seconds,
                external_funding_timeout_seconds: config.external_funding_timeout_seconds,
                external_funding: Default::default(),
            },
            inflight_payments: Default::default(),
            pending_trampoline_settlements,
            pending_external_funding_replies: Default::default(),
            last_channel_ready_scan: Default::default(),
            pending_channel_ready_retry_scans: Default::default(),
            pending_remove_tlcs: Default::default(),
            inflight_tracers: Default::default(),
            #[cfg(not(target_arch = "wasm32"))]
            lsp_service: None,
            #[cfg(test)]
            test_fiber_message_hold: None,
            #[cfg(test)]
            test_held_fiber_messages: Default::default(),
            #[cfg(test)]
            test_trampoline_settlement_paused: false,
        }
    }

    /// Start Tor onion hidden service if properly configured.
    /// Returns the onion multiaddr and a CancellationToken to stop the service,
    /// or None if the required configuration (onion_server or proxy_url) is missing.
    #[cfg(not(target_arch = "wasm32"))]
    async fn start_onion_service(
        &self,
        config: &FiberConfig,
        listening_addrs: &[MultiAddr],
        my_peer_id: &tentacle::secio::PeerId,
        tracker: &tokio_util::task::TaskTracker,
        myself: ActorRef<NetworkActorMessage>,
    ) -> Result<Option<(MultiAddr, tokio_util::sync::CancellationToken)>, String> {
        use std::{
            net::{Ipv4Addr, SocketAddr},
            time::Duration,
        };

        use tokio::time::timeout;

        // Resolve p2p listen address for onion service forwarding
        let p2p_listen_address: SocketAddr = match &config.onion.p2p_listen_address {
            Some(addr) => {
                let addr: SocketAddr = addr
                    .parse()
                    .map_err(|err| format!("Failed to parse onion_p2p_listen_address: {}", err))?;
                if addr.port() == 0 {
                    return Err("onion_p2p_listen_address port must not be 0".to_string());
                }
                addr
            }
            None => {
                // Try to derive from listening addresses
                let port = listening_addrs.iter().find_map(|addr| {
                    let mut iter = addr.iter();
                    if let (
                        Some(tentacle::multiaddr::Protocol::Ip4(ip)),
                        Some(tentacle::multiaddr::Protocol::Tcp(port)),
                    ) = (iter.next(), iter.next())
                    {
                        if ip == Ipv4Addr::new(0, 0, 0, 0) || ip == Ipv4Addr::new(127, 0, 0, 1) {
                            return Some(port);
                        }
                    }
                    None
                });
                match port {
                    Some(port) => {
                        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
                    }
                    None => {
                        error!(
                            "No suitable IPv4 listen address found for onion service; \
                            please configure `onion.p2p_listen_address` or ensure an IPv4 \
                            listener on 0.0.0.0 or 127.0.0.1 is present"
                        );
                        return Err(
                            "No suitable IPv4 listen address found for onion service".to_string()
                        );
                    }
                }
            }
        };

        // Check tor controller is reachable
        let tor_controller_str = config.onion.tor_controller.as_str();
        let tor_controller_addr: SocketAddr = tor_controller_str
            .parse()
            .map_err(|err| format!("Failed to parse tor_controller address: {}", err))?;
        let tor_connect_result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(tor_controller_addr),
        )
        .await;
        match tor_connect_result {
            Ok(Ok(_)) => {
                info!(
                    "Confirmed tor_controller is listening on {}",
                    tor_controller_str
                );
            }
            Ok(Err(_)) | Err(_) => {
                error!(
                    "tor_controller is not listening on {}, skipping onion service",
                    tor_controller_addr
                );
                return Ok(None);
            }
        }

        let onion_private_key_path =
            config
                .onion
                .onion_private_key_path
                .clone()
                .unwrap_or_else(|| {
                    config
                        .base_dir()
                        .join("onion_private_key")
                        .display()
                        .to_string()
                });

        let onion_config = super::onion_service::OnionServiceConfig {
            onion_private_key_path,
            tor_controller: tor_controller_str.to_string(),
            tor_password: config.onion.tor_password.clone(),
            p2p_listen_address,
            onion_external_port: config.onion.onion_external_port,
        };

        let peer_id_str = my_peer_id.to_base58();
        let (onion_service, onion_addr) =
            super::onion_service::OnionService::new(onion_config, &peer_id_str)?;

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let token_clone = cancel_token.clone();
        let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        tracker.spawn(async move {
            if let Err(err) = onion_service
                .start(token_clone, reconnect_tx, ready_tx)
                .await
            {
                error!("Onion service stopped with error: {}", err);
            }
        });

        // Wait for the onion service to successfully register with Tor before
        // returning the address, so callers don't advertise an unreachable address.
        match timeout(
            Duration::from_secs(config.onion.onion_service_start_timeout as u64),
            ready_rx,
        )
        .await
        {
            Err(_) => {
                cancel_token.cancel();
                return Err(String::from("Timed out waiting for onion service"));
            }
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => {
                cancel_token.cancel();
                return Err(err);
            }
            Ok(Err(_)) => {
                cancel_token.cancel();
                return Err("Onion service task exited before signaling readiness".to_string());
            }
        }

        // Listen for Tor reconnection events and trigger peer reconnection
        let cancel_for_listener = cancel_token.clone();
        tracker.spawn(async move {
            loop {
                tokio::select! {
                    msg = reconnect_rx.recv() => {
                        if msg.is_none() {
                            break;
                        }
                        info!("Tor reconnected, delaying before MaintainConnections to let DisconnectPeer events drain");
                        // Delay to ensure that PeerDisconnected events (triggered
                        // by the old Tor connection dropping) are processed by the
                        // actor before we send MaintainConnections. Without this,
                        // MaintainConnections may see stale peer_session_map entries
                        // and skip reconnection, leaving peers disconnected until
                        // the next periodic cycle (1200 s).
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        info!("Triggering MaintainConnections after Tor reconnect");
                        let _ = myself.send_message(NetworkActorMessage::new_command(
                            PublicNetworkCommand::MaintainConnections,
                        ));
                    }
                    _ = cancel_for_listener.cancelled() => {
                        break;
                    }
                }
            }
        });

        Ok(Some((onion_addr, cancel_token)))
    }

    async fn handle_peer_message(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        peer_pubkey: Pubkey,
        message: FiberMessage,
    ) -> crate::Result<FiberMessageDisposition> {
        match message {
            FiberMessage::Init(_) => {
                return Err(Error::InvalidPeerMessage("unexpected Init".to_string()));
            }
            // We should process OpenChannel message here because there is no channel corresponding
            // to the channel id in the message yet.
            FiberMessage::ChannelInitialization(open_channel) => {
                state.check_feature_compatibility(&peer_pubkey)?;
                let temp_channel_id = open_channel.channel_id;
                let peer_pubkey_for_logging = peer_pubkey;
                match state
                    .on_open_channel_msg(peer_pubkey, open_channel.clone())
                    .await
                {
                    Ok(()) => {
                        let auto_accept = if let Some(udt_type_script) =
                            open_channel.funding_udt_type_script.as_ref()
                        {
                            is_udt_type_auto_accept(udt_type_script, open_channel.funding_amount)
                        } else {
                            state.auto_accept_channel_ckb_funding_amount > 0
                                && open_channel.funding_amount
                                    >= state.open_channel_auto_accept_min_ckb_funding_amount as u128
                        };
                        if auto_accept {
                            let accept_channel = AcceptChannelCommand {
                                temp_channel_id,
                                funding_amount: if open_channel.funding_udt_type_script.is_some() {
                                    0
                                } else {
                                    state.auto_accept_channel_ckb_funding_amount as u128
                                },
                                shutdown_script: None,
                                max_tlc_number_in_flight: None,
                                max_tlc_value_in_flight: None,
                                min_tlc_value: None,
                                tlc_fee_proportional_millionths: None,
                                tlc_expiry_delta: None,
                            };
                            state.create_inbound_channel(accept_channel).await?;
                        } else {
                            // Log warning when auto-accept fails
                            state.log_receiver_auto_accept_failure(
                                &peer_pubkey_for_logging,
                                &open_channel,
                                temp_channel_id,
                            );
                            debug_event!(myself, "ChannelAutoAcceptFailed");
                        }
                    }
                    Err(err) => {
                        error!("Failed to process OpenChannel message: {}", err);
                    }
                }
            }
            FiberMessage::ChannelNormalOperation(msg) => {
                state.check_feature_compatibility(&peer_pubkey)?;
                let channel_id = msg.get_channel_id();
                let mut found = state
                    .peer_channel_index
                    .has_channel(&peer_pubkey, &channel_id);

                // If a channel message arrives before the live actor has processed the reconnect,
                // attempt to nudge reestablishment on-the-fly so the message is not dropped.
                if !found && state.is_peer_available(&peer_pubkey) {
                    if let Some(actor_state) = state.store.get_channel_actor_state(&channel_id) {
                        let _peer_id =
                            PeerId::from_public_key(&super::types::pubkey_to_tentacle(peer_pubkey));
                        if !actor_state.is_closed()
                            && actor_state.get_remote_pubkey() == peer_pubkey
                        {
                            let channel_ready = state.channels.contains_key(&channel_id)
                                || state.reestablish_channel(channel_id).await.is_ok();
                            if channel_ready {
                                found = true;
                            }
                        }
                    }
                }

                if !found {
                    debug!(
                        peer = format!("{peer_pubkey:?}"),
                        channel = format!("{channel_id:?}"),
                        "Dropping peer message for a channel not associated with the peer"
                    );
                    return Ok(FiberMessageDisposition::UnknownChannel);
                }
                state
                    .send_message_to_channel_actor(
                        channel_id,
                        Some(peer_pubkey),
                        ChannelActorMessage::PeerMessage(msg),
                    )
                    .await;
            }
        };
        Ok(FiberMessageDisposition::Processed)
    }

    async fn handle_event(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        event: FiberActorEvent,
    ) -> crate::Result<()> {
        match event {
            FiberActorEvent::PeerMessage(pubkey, message) => {
                self.handle_peer_message(myself, state, pubkey, message)
                    .await?;
            }
            FiberActorEvent::ChannelAccepted(
                pubkey,
                new,
                old,
                local,
                remote,
                script,
                udt_funding_script,
                local_reserved_ckb_amount,
                remote_reserved_ckb_amount,
                funding_fee_rate,
            ) => {
                assert_ne!(new, old, "new and old channel id must be different");
                if let Some(channel) = state.channels.remove(&old) {
                    debug!("Channel accepted: {:?} -> {:?}", old, new);
                    state.channels.insert(new, channel);
                    state.peer_channel_index.replace_channel(pubkey, old, new);

                    state.move_channel_open_record_to_final_id(&old, new);

                    debug!("Starting funding channel");
                    // TODO: Here we implies the one who receives AcceptChannel message
                    //  (i.e. the channel initiator) will send TxUpdate message first.
                    myself
                        .send_message(FiberActorMessage::new_command(
                            FiberActorCommand::UpdateChannelFunding(
                                new,
                                Default::default(),
                                FundingRequest {
                                    script,
                                    udt_type_script: udt_funding_script,
                                    local_amount: local,
                                    funding_fee_rate,
                                    remote_amount: remote,
                                    local_reserved_ckb_amount,
                                    remote_reserved_ckb_amount,
                                },
                            ),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
            }
            FiberActorEvent::ChannelReady(channel_id, pubkey, channel_outpoint) => {
                info!(
                    "Channel ({:?}) to peer {:?} is now ready",
                    channel_id, pubkey
                );
                state.peer_channel_index.mark_channel_ready(&channel_id);

                // Mark the opening record as ChannelReady (terminal success state).
                if let Some(mut record) = state.store.get_channel_open_record(&channel_id) {
                    record.update_status(ChannelOpeningStatus::ChannelReady);
                    state.store.insert_channel_open_record(record);
                }

                // FIXME(yukang): need to make sure ChannelReady is sent after the channel is reestablished
                state
                    .outpoint_channel_map
                    .insert(channel_outpoint.clone(), channel_id);

                // Notify outside observers.
                myself
                    .send_message(FiberActorMessage::new_notification(
                        NetworkServiceEvent::ChannelReady(
                            pubkey,
                            channel_id,
                            channel_outpoint.clone(),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);

                // Retry payment attempts whose first hop uses this channel.
                // Debounce to prevent resource exhaustion from repeated
                // reestablish events. Uses trailing-edge: when suppressed,
                // schedules a deferred scan so the trailing ChannelReady
                // (e.g. from a real reconnect) is never lost.
                match decide_channel_ready_retry_scan(
                    &mut state.last_channel_ready_scan,
                    &mut state.pending_channel_ready_retry_scans,
                    channel_outpoint.clone(),
                    now_timestamp_as_millis_u64(),
                ) {
                    ChannelReadyRetryScanDecision::ScanNow => {
                        state.retry_pending_payments_for_channel(&myself, &channel_outpoint);
                    }
                    ChannelReadyRetryScanDecision::ScheduleTrailing(delay) => {
                        let ch_outpoint = channel_outpoint.clone();
                        debug!(
                            "Debounced ChannelReady retry scan for {:?}, scheduling deferred scan in {}ms",
                            ch_outpoint,
                            delay.as_millis()
                        );
                        myself.send_after(delay, move || {
                            FiberActorMessage::new_event(
                                FiberActorEvent::RetryPendingPaymentsForChannel(ch_outpoint),
                            )
                        });
                    }
                    ChannelReadyRetryScanDecision::AlreadyScheduled => {
                        trace!(
                            "Debounced ChannelReady retry scan for {:?}, trailing scan already scheduled",
                            channel_outpoint
                        );
                    }
                }

                state
                    .recover_trampoline_settlements_for_channel(channel_id)
                    .await;

                debug_event!(
                    myself,
                    format!(
                        "Channel is now ready with channel_id {:?} to peer {:?}",
                        channel_id, pubkey
                    )
                );
            }
            FiberActorEvent::FundingTransactionPending(transaction, outpoint, channel_id) => {
                // Advance the opening record to FundingTxBroadcasted.
                if let Some(mut record) = state.store.get_channel_open_record(&channel_id) {
                    record.update_status(ChannelOpeningStatus::FundingTxBroadcasted);
                    state.store.insert_channel_open_record(record);
                }
                state
                    .on_funding_transaction_pending(channel_id, transaction, outpoint)
                    .await;
            }
            FiberActorEvent::FundingTransactionConfirmed(
                outpoint,
                block_hash,
                tx_index,
                timestamp,
            ) => {
                state.inflight_tracers.remove(&outpoint.tx_hash().into());
                state
                    .on_funding_transaction_confirmed(outpoint, block_hash, tx_index, timestamp)
                    .await;
            }
            FiberActorEvent::FundingTransactionFailed(outpoint) => {
                state.inflight_tracers.remove(&outpoint.tx_hash().into());
                error!("Funding transaction failed: {:?}", outpoint);
                state.abort_funding(Either::Right(outpoint)).await;
            }
            FiberActorEvent::ClosingTransactionPending(channel_id, pubkey, tx, force) => {
                state
                    .on_closing_transaction_pending(channel_id, pubkey, tx.clone(), force)
                    .await;
            }
            FiberActorEvent::ClosingTransactionConfirmed(
                pubkey,
                channel_id,
                tx_hash,
                force,
                close_by_us,
            ) => {
                state.inflight_tracers.remove(&tx_hash.clone().into());
                state
                    .on_closing_transaction_confirmed(
                        &pubkey,
                        &channel_id,
                        tx_hash,
                        force,
                        close_by_us,
                    )
                    .await;
            }
            FiberActorEvent::ClosingTransactionFailed(pubkey, channel_id, tx_hash) => {
                state.inflight_tracers.remove(&tx_hash.clone().into());
                error!(
                    "Closing transaction failed for channel {:?}, tx hash: {:?}, peer pubkey: {:?}",
                    &channel_id, &tx_hash, &pubkey
                );
            }
            FiberActorEvent::TlcRemoveReceived(payment_hash, attempt_id, remove_tlc_reason) => {
                // When a node is restarted, RemoveTLC will also be resent if necessary
                self.on_remove_tlc_event(
                    myself.clone(),
                    state,
                    payment_hash,
                    attempt_id,
                    remove_tlc_reason,
                )
                .await;
                #[cfg(debug_assertions)]
                {
                    if let Some(payment_session) = self.store.get_payment_session(payment_hash) {
                        debug_event!(
                            myself,
                            format!(
                                "after on_remove_tlc_event session_status: {:?}",
                                payment_session.status
                            )
                        );
                    }
                }
            }
            FiberActorEvent::RetrySendPayment(payment_hash, attempt_id) => {
                self.resume_payment_actor_and_send_command(
                    myself,
                    state,
                    payment_hash,
                    PaymentActorMessage::RetrySendPayment(attempt_id),
                )
                .await;
            }
            FiberActorEvent::RetryPendingPaymentsForChannel(channel_outpoint) => {
                if state
                    .pending_channel_ready_retry_scans
                    .remove(&channel_outpoint)
                    && state.outpoint_channel_map.contains_key(&channel_outpoint)
                {
                    state
                        .last_channel_ready_scan
                        .insert(channel_outpoint.clone(), now_timestamp_as_millis_u64());
                    state.retry_pending_payments_for_channel(&myself, &channel_outpoint);
                }
            }
            FiberActorEvent::AddTlcResult(
                payment_hash,
                attempt_id,
                add_tlc_result,
                previous_tlc,
            ) => {
                self.on_add_tlc_result_event(
                    myself,
                    state,
                    payment_hash,
                    attempt_id,
                    add_tlc_result,
                    previous_tlc,
                )
                .await;
            }
            FiberActorEvent::OwnedChannelUpdateEvent(owned_channel_update_event) => {
                let mut graph = self.network_graph.write().await;
                debug!(
                    "Received owned channel update event: {:?}",
                    owned_channel_update_event
                );
                let is_down =
                    matches!(owned_channel_update_event, OwnedChannelUpdateEvent::Down(_));
                graph.process_owned_channel_update_event(owned_channel_update_event);
                if is_down {
                    debug!("Owned channel is down");
                }
            }
            FiberActorEvent::ChannelActorStopped(channel_id, reason) => {
                // If the channel failed before reaching ChannelReady, mark the opening record as Failed.
                if let Some(mut record) = state.store.get_channel_open_record(&channel_id) {
                    if record.status != ChannelOpeningStatus::ChannelReady {
                        let failure_detail = match &reason {
                            StopReason::Abandon => "Channel was abandoned".to_string(),
                            StopReason::AbortFunding => "Funding transaction aborted".to_string(),
                            StopReason::AbortFundingWithDetail(detail) => detail.clone(),
                            StopReason::FundingFailed => "Funding transaction failed".to_string(),
                            StopReason::PeerDisConnected => {
                                "Peer disconnected during channel opening".to_string()
                            }
                            StopReason::Closed => {
                                "Channel closed before becoming ready".to_string()
                            }
                        };
                        if record.failure_detail.is_none() {
                            record.fail(failure_detail);
                        } else {
                            record.update_status(ChannelOpeningStatus::Failed);
                        }
                        state.store.insert_channel_open_record(record);
                    }
                }
                state.on_channel_actor_stopped(channel_id, reason).await;
            }
            FiberActorEvent::PaymentActorStopped(payment_hash, last_error_packet) => {
                state
                    .on_payment_actor_stopped(payment_hash, last_error_packet)
                    .await;
            }
            FiberActorEvent::ChannelSettlementCompleted(channel_id) => {
                if let Some(channel_actor) = state.channels.get(&channel_id) {
                    if let Err(err) = channel_actor.send_message(ChannelActorMessage::Event(
                        ChannelEvent::OnChainSettlementCompleted,
                    )) {
                        error!(
                            "Failed to notify channel {:?} about on-chain settlement completion: {:?}",
                            channel_id, err
                        );
                    }
                } else if let Some(mut actor_state) =
                    self.store.get_channel_actor_state(&channel_id)
                {
                    self.reconcile_onchain_tlcs_without_live_actor(
                        state,
                        &mut actor_state,
                        now_timestamp_as_millis_u64(),
                        true,
                    )
                    .await;
                }
            }
            FiberActorEvent::ChannelAcceptedForExternalFunding {
                peer_id,
                new_channel_id,
                old_channel_id,
                funding_amount,
                remote_funding_amount,
                funding_source_lock_script,
                funding_source_lock_script_cell_deps,
                funding_cell_lock_script,
                funding_udt_type_script,
                local_reserved_ckb_amount,
                remote_reserved_ckb_amount,
                funding_fee_rate,
            } => {
                assert_ne!(
                    new_channel_id, old_channel_id,
                    "new and old channel id must be different"
                );

                // Update channel mapping
                if let Some(peer_pubkey) = state.peer_channel_index.get_pubkey(&peer_id) {
                    if let Some(channel) = state.channels.remove(&old_channel_id) {
                        debug!(
                            "Channel accepted for external funding: {:?} -> {:?}",
                            old_channel_id, new_channel_id
                        );
                        state.channels.insert(new_channel_id, channel);
                        state.peer_channel_index.replace_channel(
                            peer_pubkey,
                            old_channel_id,
                            new_channel_id,
                        );
                    }
                }

                state.move_channel_open_record_to_final_id(&old_channel_id, new_channel_id);

                // Move the pending reply to the final channel id. The actual RPC response is
                // sent only after tx collaboration finishes and the unsigned tx is frozen.
                let reply = state
                    .pending_external_funding_replies
                    .remove(&old_channel_id)
                    .or_else(|| {
                        state
                            .pending_external_funding_replies
                            .remove(&new_channel_id)
                    });

                if let Some(reply) = reply {
                    // Build the local unsigned tx. External funding passes a custom funding
                    // source lock and optional extra cell deps; the shared builder handles both
                    // internal and external funding paths.
                    let request = FundingRequest {
                        script: funding_source_lock_script.clone(),
                        udt_type_script: funding_udt_type_script,
                        local_amount: funding_amount,
                        remote_amount: remote_funding_amount,
                        funding_fee_rate,
                        local_reserved_ckb_amount,
                        remote_reserved_ckb_amount,
                    };

                    let funding_tx = FundingTx::new();
                    let (send, recv) = oneshot::channel::<Result<FundingTx, FundingError>>();
                    let rpc_reply = RpcReplyPort::from(send);

                    let _ =
                        state
                            .chain_actor
                            .send_message(CkbChainMessage::BuildUnsignedFundingTx {
                                funding_tx,
                                request,
                                funding_source_lock_script,
                                funding_source_lock_script_cell_deps,
                                funding_cell_lock_script,
                                reply: rpc_reply,
                            });

                    match ractor::concurrency::timeout(
                        Duration::from_millis(DEFAULT_CHAIN_ACTOR_TIMEOUT),
                        recv,
                    )
                    .await
                    {
                        Ok(Ok(Ok(built_tx))) => {
                            if let Some(tx) = built_tx.into_inner() {
                                debug!(
                                    "Starting external funding tx collaboration for channel {:?} with locally built tx {:?}",
                                    new_channel_id,
                                    tx.hash()
                                );
                                state
                                    .pending_external_funding_replies
                                    .insert(new_channel_id, reply);
                                if let Err(e) = state
                                    .send_command_to_channel(
                                        new_channel_id,
                                        ChannelCommand::TxCollaborationCommand(
                                            TxCollaborationCommand::TxUpdate(TxUpdateCommand {
                                                transaction: tx.data(),
                                            }),
                                        ),
                                    )
                                    .await
                                {
                                    error!(
                                        "Failed to start external funding tx collaboration: {:?}",
                                        e
                                    );
                                    if let Some(reply) = state
                                        .pending_external_funding_replies
                                        .remove(&new_channel_id)
                                    {
                                        let _ = reply.send(Err(format!(
                                            "Failed to start external funding tx collaboration: {}",
                                            e
                                        )));
                                    }
                                }
                            } else {
                                error!(
                                    "Built funding tx is empty for channel {:?}",
                                    new_channel_id
                                );
                                let _ = reply
                                    .send(Err("Failed to build unsigned funding tx: empty result"
                                        .to_string()));
                            }
                        }
                        Ok(Ok(Err(e))) => {
                            error!(
                                "Failed to build unsigned funding tx for channel {:?}: {:?}",
                                new_channel_id, e
                            );
                            let _ = reply
                                .send(Err(format!("Failed to build unsigned funding tx: {}", e)));
                        }
                        Ok(Err(e)) => {
                            error!(
                                "Channel recv error for channel {:?}: {:?}",
                                new_channel_id, e
                            );
                            let _ = reply.send(Err(format!("Channel recv error: {}", e)));
                        }
                        Err(_) => {
                            error!(
                                "Timeout waiting for unsigned funding tx for channel {:?}",
                                new_channel_id
                            );
                            let _ = reply
                                .send(Err("Timeout waiting for unsigned funding tx".to_string()));
                        }
                    }
                } else {
                    warn!(
                        "No pending reply found for external funding channel {:?} (old: {:?})",
                        new_channel_id, old_channel_id
                    );
                }
            }
            FiberActorEvent::ExternalFundingTxReady(channel_id, funding_tx) => {
                if let Some(reply) = state.pending_external_funding_replies.remove(&channel_id) {
                    debug!(
                        "Returning negotiated unsigned external funding tx for channel {:?}: {:?}",
                        channel_id,
                        funding_tx.calc_tx_hash()
                    );
                    let _ = reply.send(Ok(OpenChannelWithExternalFundingResponse {
                        channel_id,
                        unsigned_funding_tx: funding_tx,
                    }));
                } else {
                    warn!(
                        "No pending external funding reply found when tx became ready for channel {:?}",
                        channel_id
                    );
                }
            }
        }
        Ok(())
    }

    async fn handle_command(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        command: FiberActorCommand,
    ) -> crate::Result<()> {
        match command {
            FiberActorCommand::SendFiberMessage(message_with_target) => {
                #[cfg(test)]
                if state
                    .test_fiber_message_hold
                    .as_ref()
                    .is_some_and(|hold| hold.matches(&message_with_target))
                {
                    let remaining = state
                        .test_fiber_message_hold
                        .as_ref()
                        .expect("matching test hold")
                        .remaining
                        .get()
                        - 1;
                    if let Some(remaining) = NonZeroUsize::new(remaining) {
                        state
                            .test_fiber_message_hold
                            .as_mut()
                            .expect("matching test hold")
                            .remaining = remaining;
                    } else {
                        state.test_fiber_message_hold = None;
                    }
                    state
                        .test_held_fiber_messages
                        .push_back(message_with_target);
                    return Ok(());
                }
                let FiberMessageWithTarget { target, message } = message_with_target;
                state.send_fiber_message(&target, message).await?;
            }
            #[cfg(test)]
            FiberActorCommand::SetTestFiberMessageHold(hold, reply) => {
                state.test_fiber_message_hold = Some(hold);
                let _ = reply.send(());
            }
            #[cfg(test)]
            FiberActorCommand::TakeTestHeldFiberMessages(reply) => {
                state.test_fiber_message_hold = None;
                let messages = state.test_held_fiber_messages.drain(..).collect();
                let _ = reply.send(messages);
            }
            #[cfg(test)]
            FiberActorCommand::ReleaseTestHeldFiberMessages(reply) => {
                state.test_fiber_message_hold = None;
                while let Some(FiberMessageWithTarget { target, message }) =
                    state.test_held_fiber_messages.pop_front()
                {
                    if let Err(error) = state.send_fiber_message(&target, message).await {
                        let remaining = state.test_held_fiber_messages.len();
                        let noun = if remaining == 1 {
                            "message"
                        } else {
                            "messages"
                        };
                        let _ = reply.send(Err(format!(
                            "failed to release held Fiber message: {error}; \
                             {remaining} unattempted {noun} remains queued"
                        )));
                        return Ok(());
                    }
                }
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            FiberActorCommand::GetTestHeldFiberMessageCount(reply) => {
                let _ = reply.send(state.test_held_fiber_messages.len());
            }
            #[cfg(test)]
            FiberActorCommand::SetTestTrampolineSettlementPaused(paused, reply) => {
                state.test_trampoline_settlement_paused = paused;
                let _ = reply.send(());
            }
            FiberActorCommand::RegisterInProcessPeer {
                pubkey,
                actor,
                features,
                reply,
            } => {
                let result = if pubkey == state.get_public_key() {
                    Err("cannot register the local Fiber endpoint as its own peer".to_string())
                } else if state.in_process_peers.get(&pubkey).is_some_and(|peer| {
                    peer.actor != actor && peer.actor.get_status() < ractor::ActorStatus::Stopping
                }) {
                    Err(format!(
                        "in-process peer {pubkey:?} is already owned by another actor"
                    ))
                } else {
                    state
                        .in_process_peers
                        .insert(pubkey, InProcessPeer { actor, features });
                    Ok(())
                };
                let _ = reply.send(result);
            }
            FiberActorCommand::ActivateInProcessPeer(pubkey, reply) => {
                let result = if !state.in_process_peers.contains_key(&pubkey) {
                    Err(format!("in-process peer {pubkey:?} is not registered"))
                } else {
                    if let Some(channel_ids) = state.peer_channel_index.get_channels(&pubkey) {
                        for channel_id in channel_ids {
                            if let Err(error) = state.reestablish_channel(channel_id).await {
                                error!(
                                    "Failed to reestablish in-process channel {:x}: {:?}",
                                    channel_id, error
                                );
                            }
                        }
                    }
                    Ok(())
                };
                let _ = reply.send(result);
            }
            FiberActorCommand::UnregisterInProcessPeer(pubkey) => {
                state.disconnect_in_process_peer(pubkey);
            }
            FiberActorCommand::CheckChannelsShutdown => {
                for (_pubkey, channel_id, channel_state) in self.store.get_channel_states(None) {
                    if matches!(
                        channel_state,
                        ChannelState::ChannelReady | ChannelState::ShuttingDown(..)
                    ) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            let funding_lock_script = state
                                .get_cached_channel_funding_lock_script(channel_id, &actor_state);
                            // Spawn async task for concurrent RPC call
                            let chain_client = self.chain_client.clone();
                            let myself_clone = myself.clone();
                            crate::tasks::spawn(async move {
                                Self::check_channel_shutdown(
                                    chain_client,
                                    myself_clone,
                                    channel_id,
                                    funding_lock_script,
                                )
                                .await;
                            });
                        }
                    } else if matches!(
                        channel_state,
                        ChannelState::Closed(flags)
                            if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
                                && !flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED)
                    ) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            // Spawn async task for concurrent RPC call
                            let chain_client = self.chain_client.clone();
                            let myself_clone = myself.clone();
                            crate::tasks::spawn(async move {
                                Self::check_channel_shutdown_settlement(
                                    chain_client,
                                    myself_clone,
                                    actor_state,
                                )
                                .await;
                            });
                        }
                    }
                }
            }
            FiberActorCommand::CheckChannels => {
                let now = now_timestamp_as_millis_u64();

                for (_pubkey, channel_id, channel_state) in self.store.get_channel_states(None) {
                    if should_reconcile_closed_channel_without_live_actor(channel_state) {
                        if state.channels.contains_key(&channel_id) {
                            continue;
                        }
                        let Some(mut actor_state) = self.store.get_channel_actor_state(&channel_id)
                        else {
                            continue;
                        };
                        self.reconcile_onchain_tlcs_without_live_actor(
                            state,
                            &mut actor_state,
                            now,
                            false,
                        )
                        .await;
                    }
                }

                self.retry_hold_tlc_sets(&myself);
            }
            FiberActorCommand::SettleHoldTlcSet(payment_hash) => {
                self.settle_hold_tlc_set(myself, state, payment_hash);
            }
            FiberActorCommand::SettleReceivedHoldTlcSet(payment_hash) => {
                self.settle_received_hold_tlc_set(myself, state, payment_hash);
            }
            FiberActorCommand::SettleOnChainFulfilledInvoice(payment_hash) => {
                self.settle_onchain_fulfilled_invoice(payment_hash);
            }
            FiberActorCommand::ReconcileOnChainPayerTlc {
                channel_id,
                tlc_id,
                payment_hash,
                attempt_id,
                payment_preimage,
                reply,
            } => {
                let result = self
                    .reconcile_onchain_payer_tlc(
                        state,
                        channel_id,
                        tlc_id,
                        payment_hash,
                        attempt_id,
                        payment_preimage,
                    )
                    .await;
                if let Err(err) = &result {
                    warn!(
                        "Failed to reconcile on-chain payer TLC {:?} in channel {:?}: {}",
                        tlc_id, channel_id, err
                    );
                }
                let _ = reply.send(result);
            }
            FiberActorCommand::RelayOnChainTlcRemove {
                downstream_channel_id,
                downstream_tlc_id,
                forwarding_channel_id,
                forwarding_tlc_id,
                payment_hash,
                reason,
            } => {
                self.forward_onchain_tlc_remove_upstream(
                    myself,
                    state,
                    downstream_channel_id,
                    downstream_tlc_id,
                    forwarding_channel_id,
                    forwarding_tlc_id,
                    payment_hash,
                    reason,
                );
            }
            FiberActorCommand::RelayOnChainTlcRemoveResult {
                downstream_channel_id,
                downstream_tlc_id,
                forwarding_channel_id,
                forwarding_tlc_id,
                payment_hash,
                reason,
                result,
            } => {
                state
                    .pending_remove_tlcs
                    .remove(&(forwarding_channel_id, forwarding_tlc_id));

                let delivered = match result {
                    Ok(()) | Err(ProcessingChannelError::WaitingTlcAck) => true,
                    Err(err) => {
                        let already_removed_with_same_reason = self
                            .store
                            .get_channel_actor_state(&forwarding_channel_id)
                            .is_some_and(|channel_state| {
                                onchain_upstream_removed_reason_matches(
                                    &channel_state,
                                    forwarding_tlc_id,
                                    &reason,
                                )
                            });
                        if !already_removed_with_same_reason {
                            error!(
                                "Failed to relay on-chain resolved tlc {:?} upstream to channel {:?}: {}; will retry on next maintenance tick",
                                forwarding_tlc_id, forwarding_channel_id, err
                            );
                        }
                        already_removed_with_same_reason
                    }
                };
                if delivered {
                    self.confirm_onchain_tlc_remove_relay(
                        state,
                        downstream_channel_id,
                        downstream_tlc_id,
                        payment_hash,
                        reason,
                    );
                }
            }
            FiberActorCommand::RemoveTlcResult {
                channel_id,
                tlc_id,
                hold_payment_hash,
                result,
            } => {
                state.pending_remove_tlcs.remove(&(channel_id, tlc_id));
                match result {
                    Ok(()) | Err(ProcessingChannelError::WaitingTlcAck) => {
                        if let Some(payment_hash) = hold_payment_hash {
                            self.store
                                .remove_payment_hold_tlc(&payment_hash, &channel_id, tlc_id);
                        }
                    }
                    Err(err) => {
                        error!(
                            "Failed to remove tlc {:?} for channel {:?}: {}",
                            tlc_id, channel_id, err
                        );
                    }
                }
            }
            #[cfg(test)]
            FiberActorCommand::InstallTestChannelActor(channel_id, actor, reply) => {
                state.channels.insert(channel_id, actor);
                let _ = reply.send(());
            }
            FiberActorCommand::SettleTlcSet(payment_hash, channel_tlc_ids) => {
                self.settle_tlc_set(myself, state, payment_hash, channel_tlc_ids);
            }
            FiberActorCommand::TimeoutHoldTlc(payment_hash, channel_id, tlc_id) => {
                self.timeout_hold_tlc(myself, state, payment_hash, channel_id, tlc_id);
            }
            FiberActorCommand::OpenChannel(open_channel, reply) => {
                let network_graph = self.network_graph.clone();
                match state
                    .create_outbound_channel(open_channel, network_graph)
                    .await
                {
                    Ok((_, channel_id)) => {
                        let _ = reply.send(Ok(OpenChannelResponse { channel_id }));
                    }
                    Err(err) => {
                        error!("Failed to create channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            FiberActorCommand::AcceptChannel(accept_channel, reply) => {
                match state.create_inbound_channel(accept_channel).await {
                    Ok((_, old_channel_id, new_channel_id)) => {
                        let _ = reply.send(Ok(AcceptChannelResponse {
                            old_channel_id,
                            new_channel_id,
                        }));
                    }
                    Err(err) => {
                        error!("Failed to accept channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            FiberActorCommand::AbandonChannel(channel_id, reply) => {
                match state.abandon_channel(channel_id).await {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(err) => {
                        error!("Failed to abandon channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            FiberActorCommand::ControlFiberChannel(c) => {
                state
                    .send_command_to_channel(c.channel_id, c.command)
                    .await?
            }
            #[cfg(any(test, feature = "bench"))]
            FiberActorCommand::GetChannelActor(channel_id, reply) => {
                let _ = reply.send(state.channels.get(&channel_id).cloned());
            }
            FiberActorCommand::SendPaymentOnionPacket(command, reply) => {
                match self
                    .handle_send_onion_packet_command(state, command.clone())
                    .await
                {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(err) => {
                        self.on_add_tlc_result_event(
                            myself,
                            state,
                            command.payment_hash,
                            command.attempt_id,
                            Err((
                                ProcessingChannelError::TlcForwardingError(err.clone()),
                                err.clone(),
                            )),
                            command.previous_tlc,
                        )
                        .await;
                        let _ = reply.send(Err(err));
                    }
                }
            }
            FiberActorCommand::UpdateChannelFunding(channel_id, transaction, request) => {
                self.do_update_channel_funding(&myself, state, channel_id, 0, transaction, request)
                    .await?
            }
            FiberActorCommand::VerifyFundingTx {
                peer,
                local_tx,
                remote_tx,
                funding_cell_lock_script,
                funding_udt_type_script,
                funding_source_lock_script,
                reply,
            } => {
                let _ = self
                    .chain_actor
                    .send_message(CkbChainMessage::VerifyFundingTx {
                        local_tx,
                        remote_tx,
                        reply,
                        funding_cell_lock_script,
                        funding_udt_type_script,
                        funding_source_lock_script,
                        allow_peer_funding_source_lock: state.in_process_peers.contains_key(&peer),
                    });
            }
            FiberActorCommand::NotifyFundingTx(tx) => {
                let _ = self
                    .chain_actor
                    .send_message(CkbChainMessage::AddFundingTx(tx.into()));
            }
            FiberActorCommand::SignFundingTx(target, channel_id, funding_tx, partial_witnesses) => {
                debug!(
                    "Received SignFundingTx request for transaction {:?} (has_partial_witnesses={})",
                    &funding_tx,
                    partial_witnesses.is_some()
                );
                self.do_sign_funding_tx(
                    &myself,
                    state,
                    channel_id,
                    0,
                    target,
                    funding_tx,
                    partial_witnesses,
                )
                .await?
            }
            FiberActorCommand::RetryUpdateChannelFunding(
                channel_id,
                transaction,
                request,
                retry_count,
            ) => {
                self.do_update_channel_funding(
                    &myself,
                    state,
                    channel_id,
                    retry_count,
                    transaction,
                    request,
                )
                .await?
            }
            FiberActorCommand::RetrySignFundingTx(
                target,
                channel_id,
                funding_tx,
                partial_witnesses,
                retry_count,
            ) => {
                self.do_sign_funding_tx(
                    &myself,
                    state,
                    channel_id,
                    retry_count,
                    target,
                    funding_tx,
                    partial_witnesses,
                )
                .await?
            }
            FiberActorCommand::CheckChannelShutdown(channel_id, rpc_reply) => {
                if let Some(channel_state) = self.store.get_channel_actor_state(&channel_id) {
                    let funding_lock_script =
                        state.get_cached_channel_funding_lock_script(channel_id, &channel_state);
                    // Spawn async task for concurrent RPC call
                    let chain_client = self.chain_client.clone();
                    let myself_clone = myself.clone();
                    crate::tasks::spawn(async move {
                        Self::check_channel_shutdown(
                            chain_client,
                            myself_clone,
                            channel_id,
                            funding_lock_script,
                        )
                        .await;
                    });
                    let _ = rpc_reply.send(Ok(()));
                } else {
                    tracing::debug!(
                        "stop check channel shutdown, can't find {channel_id:?} actor state"
                    );
                    let _ = rpc_reply.send(Err(format!("Channel not found: {:?}", channel_id)));
                }
            }
            FiberActorCommand::RemoteForceShutdownChannel(channel_id, response) => {
                if let Some(shutdown_tx_response) = response {
                    self.handle_remote_channel_shutdown(myself, channel_id, shutdown_tx_response)
                        .await;
                }
            }
            FiberActorCommand::SendPayment(payment_request, reply) => {
                let payment_request = match payment_request.build_send_payment_data() {
                    Ok(payment) => payment,
                    Err(err) => {
                        error!("Failed to build payment from command: {:?}", err);
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    }
                };

                let _ = self
                    .start_payment_actor(
                        myself,
                        state,
                        payment_request.payment_hash,
                        PaymentActorMessage::SendPayment(payment_request, reply),
                    )
                    .await;
            }
            FiberActorCommand::SendPaymentWithRouter(payment_request, reply) => {
                let source = self.network_graph.read().await.get_source_pubkey();
                let payment_request = match payment_request.build_send_payment_data(source) {
                    Ok(payment) => payment,
                    Err(err) => {
                        error!("Failed to build payment from command: {:?}", err);
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    }
                };
                let _ = self
                    .start_payment_actor(
                        myself,
                        state,
                        payment_request.payment_hash,
                        PaymentActorMessage::SendPayment(payment_request, reply),
                    )
                    .await;
            }
            FiberActorCommand::BuildPaymentRouter(build_payment_router, reply) => {
                match self.on_build_payment_router(build_payment_router).await {
                    Ok(router) => {
                        let _ = reply.send(Ok(router));
                    }
                    Err(e) => {
                        error!("Failed to build payment router: {:?}", e);
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            FiberActorCommand::GetPayment(payment_hash, reply) => {
                match self.on_get_payment(&payment_hash) {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            FiberActorCommand::GetHostedTenantActivity(reply) => {
                let _ = reply.send(state.hosted_tenant_activity());
            }
            FiberActorCommand::InspectBufferedTrampolineUpstream { request, reply } => {
                let status = state
                    .store
                    .get_channel_actor_state(&request.previous_tlc.prev_channel_id)
                    .map(|channel| {
                        buffered_trampoline_upstream_status(
                            &channel,
                            request.payment_hash,
                            &request.previous_tlc,
                        )
                    })
                    .unwrap_or(BufferedTrampolineUpstreamStatus::Unknown);
                let _ = reply.send(status);
            }
            #[cfg(not(target_arch = "wasm32"))]
            FiberActorCommand::DispatchBufferedTrampoline { request, reply } => {
                let result = self.try_dispatch_trampoline_payment(state, request).await;
                let _ = reply.send(result);
            }
            #[cfg(not(target_arch = "wasm32"))]
            FiberActorCommand::ReconcileBufferedTrampolineSettlement {
                payment_hash,
                reply,
            } => {
                let result = match state.store.get_payment_session(payment_hash) {
                    Some(session) if session.status.is_final() => {
                        state.settle_trampoline_payment(&session, None, None).await
                    }
                    Some(_) => Err(format!(
                        "hosted payment {payment_hash} is not ready for upstream settlement"
                    )),
                    None => Err(format!("hosted payment {payment_hash} does not exist")),
                };
                let _ = reply.send(result);
            }
            #[cfg(not(target_arch = "wasm32"))]
            FiberActorCommand::FailBufferedTrampoline {
                request,
                reason,
                error_code,
                reply,
            } => {
                let lsp_service = state.lsp_service.clone();
                let result = self
                    .fail_buffered_trampoline(state, lsp_service, request, reason, error_code)
                    .await;
                let _ = reply.send(result);
            }
            FiberActorCommand::GetInflightPaymentCount(reply) => {
                let _ = reply.send(Ok(state.inflight_payments.len() as u32));
            }
            FiberActorCommand::GetPendingAcceptChannels(rpc) => {
                let pending = state
                    .to_be_accepted_channels
                    .map
                    .iter()
                    .map(
                        |(channel_id, (pubkey, open_channel))| PendingAcceptChannel {
                            channel_id: *channel_id,
                            pubkey: *pubkey,
                            funding_amount: open_channel.funding_amount,
                            udt_type_script: open_channel.funding_udt_type_script.clone(),
                            created_at: state
                                .store
                                .get_channel_open_record(channel_id)
                                .map(|r| r.created_at)
                                .unwrap_or_else(crate::now_timestamp_as_millis_u64),
                        },
                    )
                    .collect::<Vec<_>>();
                let _ = rpc.send(Ok(pending));
            }

            FiberActorCommand::SettleInvoice(hash, preimage, reply) => {
                let _ = reply.send(self.settle_invoice(&myself, hash, preimage));
            }
            FiberActorCommand::CancelInvoice(hash, reply) => {
                let _ = reply.send(self.cancel_invoice(&myself, hash));
            }
            FiberActorCommand::AddInvoice(invoice, preimage, reply) => {
                let _ = reply.send(self.add_invoice(invoice, preimage));
            }
            FiberActorCommand::GetInvoice(payment_hash, reply) => {
                let result = self
                    .store
                    .get_invoice(&payment_hash)
                    .ok_or(InvoiceError::InvoiceNotFound)
                    .and_then(|invoice| {
                        let status = self
                            .store
                            .get_invoice_status(&payment_hash)
                            .ok_or(InvoiceError::InvoiceNotFound)?;
                        let status = match status {
                            CkbInvoiceStatus::Open if invoice.is_expired() => {
                                CkbInvoiceStatus::Expired
                            }
                            status => status,
                        };
                        Ok((invoice, status))
                    });
                let _ = reply.send(result);
            }

            FiberActorCommand::OpenChannelWithExternalFunding(open_channel, reply) => {
                debug!(
                    "OpenChannelWithExternalFunding request: pubkey={:?}, funding_amount={:?}",
                    open_channel.pubkey, open_channel.funding_amount
                );
                match state
                    .create_outbound_channel_with_external_funding(open_channel)
                    .await
                {
                    Ok((_channel_actor, temp_channel_id)) => {
                        // Channel is now in NegotiatingFunding state waiting for AcceptChannel.
                        // Store the reply port - we'll send the response when the peer accepts
                        // and we build the unsigned funding tx.
                        state
                            .pending_external_funding_replies
                            .insert(temp_channel_id, reply);
                        debug!(
                            "Stored pending reply for external funding channel {:?}",
                            temp_channel_id
                        );
                    }
                    Err(err) => {
                        error!("Failed to create channel with external funding: {:?}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            FiberActorCommand::SubmitSignedFundingTx {
                channel_id,
                signed_tx,
                reply,
            } => {
                debug!(
                    "SubmitSignedFundingTx request: channel_id={:?}, tx_hash={:?}",
                    channel_id,
                    signed_tx.calc_tx_hash()
                );

                if !state.channels.contains_key(&channel_id) {
                    let Some(channel_state) = state.store.get_channel_actor_state(&channel_id)
                    else {
                        let err = Error::ChannelNotFound(channel_id);
                        error!(
                            "Failed to send SubmitExternalFundingTx command to channel {:?}: {:?}",
                            channel_id, err
                        );
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    };

                    if channel_state.is_closed() {
                        let err = Error::ChannelError(ProcessingChannelError::InvalidState(
                            format!("Channel {:x} is already closed", &channel_id),
                        ));
                        error!(
                            "Failed to restore channel {:?} for SubmitExternalFundingTx: {:?}",
                            channel_id, err
                        );
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    }

                    if let Err(err) = state
                        .restore_offline_channel(channel_state.get_remote_pubkey(), channel_id)
                        .await
                    {
                        error!(
                            "Failed to restore channel {:?} for SubmitExternalFundingTx: {:?}",
                            channel_id, err
                        );
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    }
                }

                // Forward the command to the channel actor
                if let Err(e) = state
                    .send_command_to_channel(
                        channel_id,
                        ChannelCommand::SubmitExternalFundingTx(signed_tx, reply),
                    )
                    .await
                {
                    error!(
                        "Failed to send SubmitExternalFundingTx command to channel {:?}: {:?}",
                        channel_id, e
                    );
                }
            }
        };
        Ok(())
    }

    fn forward_remove_tlc_to_channel<F>(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        channel_id: Hash256,
        remove_tlc_command: RemoveTlcCommand,
        completion: F,
    ) where
        F: Fn(Result<(), ProcessingChannelError>) -> FiberActorCommand + Clone + Send + 'static,
    {
        let tlc_id = remove_tlc_command.id;
        if !state.pending_remove_tlcs.insert((channel_id, tlc_id)) {
            return;
        }

        let Some(channel_actor) = state.channels.get(&channel_id).cloned() else {
            let result = if state.queue_retryable_remove_tlc(channel_id, &remove_tlc_command) {
                Ok(())
            } else {
                Err(ProcessingChannelError::InvalidState(format!(
                    "Channel {channel_id:?} not found"
                )))
            };
            let _ = myself.send_message(FiberActorMessage::new_command(completion(result)));
            return;
        };

        let forward_target = myself.clone();
        ractor::concurrency::spawn(async move {
            let completion_for_result = completion.clone();
            let result = ractor::call_t!(
                channel_actor,
                |reply| ChannelActorMessage::Command(ChannelCommand::RemoveTlc(
                    remove_tlc_command,
                    reply,
                )),
                DEFAULT_CHAIN_ACTOR_TIMEOUT
            );
            match result {
                Ok(result) => {
                    let _ = forward_target.send_message(FiberActorMessage::new_command(
                        completion_for_result(result),
                    ));
                }
                Err(err) => {
                    let _ = myself.send_message(FiberActorMessage::new_command(completion(Err(
                        ProcessingChannelError::InvalidState(format!(
                            "RemoveTlc reply dropped for channel {channel_id:?}: {err}"
                        )),
                    ))));
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn forward_onchain_tlc_remove_upstream(
        &self,
        myself: FiberActorRef,
        network_state: &mut FiberActorState<S, C>,
        downstream_channel_id: Hash256,
        downstream_tlc_id: TLCId,
        forwarding_channel_id: Hash256,
        forwarding_tlc_id: u64,
        payment_hash: Hash256,
        reason: RemoveTlcReason,
    ) {
        let reason_for_result = reason.clone();
        self.forward_remove_tlc_to_channel(
            myself,
            network_state,
            forwarding_channel_id,
            RemoveTlcCommand {
                id: forwarding_tlc_id,
                reason,
            },
            move |result| FiberActorCommand::RelayOnChainTlcRemoveResult {
                downstream_channel_id,
                downstream_tlc_id,
                forwarding_channel_id,
                forwarding_tlc_id,
                payment_hash,
                reason: reason_for_result.clone(),
                result,
            },
        );
    }

    fn confirm_onchain_tlc_remove_relay(
        &self,
        state: &mut FiberActorState<S, C>,
        downstream_channel_id: Hash256,
        downstream_tlc_id: TLCId,
        payment_hash: Hash256,
        reason: RemoveTlcReason,
    ) {
        if let RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }) = &reason {
            self.store.insert_preimage(payment_hash, *payment_preimage);
        }
        let mut confirmed_to_live_actor = false;
        if let Some(actor) = state.channels.get(&downstream_channel_id) {
            confirmed_to_live_actor = actor
                .send_message(ChannelActorMessage::Event(
                    ChannelEvent::OnChainTlcRelayConfirmed(downstream_tlc_id, reason.clone()),
                ))
                .is_ok();
        }
        if !confirmed_to_live_actor {
            if let Some(mut channel_state) =
                self.store.get_channel_actor_state(&downstream_channel_id)
            {
                if let TLCId::Offered(id) = downstream_tlc_id {
                    if channel_state
                        .tlc_state
                        .get(&downstream_tlc_id)
                        .is_some_and(|tlc| tlc.removed_reason.is_none())
                    {
                        channel_state.tlc_state.set_offered_tlc_removed(id, reason);
                        self.store.insert_channel_actor_state(channel_state);
                    }
                }
            }
        }
    }

    /// Start relaying an on-chain resolved downstream TLC upstream. The completion message marks
    /// the downstream TLC removed only after the upstream delivery is durable.
    fn relay_onchain_tlc_remove_upstream(
        &self,
        network_state: &mut FiberActorState<S, C>,
        channel_state: &ChannelActorState,
        relay: OnChainTlcRemoveRelay,
    ) -> bool {
        let network = network_state.network.clone();
        self.forward_onchain_tlc_remove_upstream(
            network,
            network_state,
            channel_state.get_id(),
            relay.downstream_tlc_id,
            relay.forwarding_channel_id,
            relay.forwarding_tlc_id,
            relay.payment_hash,
            relay.reason,
        );
        false
    }

    fn already_fulfilled_onchain_invoice_hashes(
        &self,
        actor_state: &ChannelActorState,
    ) -> HashSet<Hash256> {
        let channel_id = actor_state.get_id();
        actor_state
            .tlc_state
            .received_tlcs
            .tlcs
            .iter()
            .filter_map(|tlc| {
                let Some(RemoveTlcReason::RemoveTlcFulfill(fulfill)) = &tlc.removed_reason else {
                    return None;
                };
                let preimage = onchain_fulfilled_preimage(&channel_id, &self.store, tlc)?;
                if preimage != fulfill.payment_preimage {
                    warn!(
                        "Skipping already-fulfilled TLC {:?} in channel {:?}: local preimage does not match on-chain preimage",
                        tlc.tlc_id, channel_id
                    );
                    return None;
                }
                if self.store.get_invoice_status(&tlc.payment_hash) == Some(CkbInvoiceStatus::Paid)
                {
                    return None;
                }
                Some(tlc.payment_hash)
            })
            .collect()
    }

    /// Reconcile on-chain resolved TLCs for a force-closed channel without a live actor.
    /// When `mark_settlement_confirmed` is set, this also records the settlement confirmation.
    /// Once all on-chain TLCs are resolved, this clears the waiting flags and finalizes the
    /// closed channel state.
    async fn reconcile_onchain_tlcs_without_live_actor(
        &self,
        state: &mut FiberActorState<S, C>,
        actor_state: &mut ChannelActorState,
        now: u64,
        mark_settlement_confirmed: bool,
    ) {
        let channel_id = actor_state.get_id();
        let mut settlement_state_changed = false;
        // Snapshot before newly discovered TLCs are marked removed below, so each payer effect is
        // applied at most once per reconciliation pass.
        let confirmed_payer_tlcs = collect_onchain_confirmed_payer_tlcs(actor_state, &self.store);
        let mut payer_effects_applied = true;

        // Settlement-completed events are the durable chain signal that the channel close
        // transaction has finished. Record that signal first, then let the normal reconciliation
        // pass decide whether all TLC-level outcomes are known yet.
        if mark_settlement_confirmed {
            let ChannelState::Closed(mut flags) = actor_state.state else {
                return;
            };
            if !flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT) {
                return;
            }
            if !flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED) {
                flags.insert(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED);
                actor_state.state = ChannelState::Closed(flags);
                settlement_state_changed = true;
            }
        }

        let delay_epoch =
            EpochNumberWithFraction::from_full_value(actor_state.commitment_delay_epoch);
        let expect_expiry = now.saturating_add(tlc_expiry_delay(&delay_epoch));
        let mut actor_state_changed = false;

        // Resolve offered TLCs that have timed out on chain. Forwarded TLCs must first relay a
        // matching failure upstream; origin-payer TLCs can notify the local payment state directly.
        let expired_tlcs =
            collect_onchain_timeout_settled_tlcs(actor_state, &self.store, expect_expiry);
        for tlc in expired_tlcs {
            let reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                TlcErr::new(TlcErrorCode::ExpiryTooSoon),
                &tlc.shared_secret,
            ));
            match tlc.role {
                OnChainTimeoutTlcRole::Forwarded {
                    forwarding_channel_id,
                    forwarding_tlc_id,
                } => {
                    actor_state_changed |= self.relay_onchain_tlc_remove_upstream(
                        state,
                        actor_state,
                        OnChainTlcRemoveRelay {
                            downstream_tlc_id: tlc.tlc_id,
                            forwarding_channel_id,
                            forwarding_tlc_id,
                            payment_hash: tlc.payment_hash,
                            reason,
                        },
                    );
                }
                OnChainTimeoutTlcRole::OriginPayer { attempt_id } => {
                    state
                        .network
                        .send_message(FiberActorMessage::new_event(
                            FiberActorEvent::TlcRemoveReceived(
                                tlc.payment_hash,
                                attempt_id,
                                reason.clone(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    if let TLCId::Offered(id) = tlc.tlc_id {
                        actor_state.tlc_state.set_offered_tlc_removed(id, reason);
                        actor_state_changed = true;
                    }
                }
            }
        }

        // Resolve fulfilled TLCs only from confirmed channel-scoped on-chain settlement records.
        // Forwarded offered TLCs relay upstream before the downstream channel is marked removed;
        // source payments and payee invoices are completed locally.
        let fulfilled = collect_onchain_fulfilled_tlcs(actor_state, &self.store);
        let mut invoice_hashes = Vec::new();
        for tlc in fulfilled {
            match tlc.tlc_id {
                TLCId::Offered(id) => {
                    let fulfill = RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                        payment_preimage: tlc.preimage,
                    });
                    if let Some((forwarding_channel_id, forwarding_tlc_id)) = tlc.forwarding_tlc {
                        actor_state_changed |= self.relay_onchain_tlc_remove_upstream(
                            state,
                            actor_state,
                            OnChainTlcRemoveRelay {
                                downstream_tlc_id: tlc.tlc_id,
                                forwarding_channel_id,
                                forwarding_tlc_id,
                                payment_hash: tlc.payment_hash,
                                reason: fulfill,
                            },
                        );
                    } else {
                        self.store.insert_preimage(tlc.payment_hash, tlc.preimage);
                        actor_state
                            .tlc_state
                            .set_offered_tlc_removed(id, fulfill.clone());
                        actor_state_changed = true;
                        if let Some(attempt_id) = tlc.attempt_id {
                            if let Err(err) = self
                                .reconcile_onchain_payer_tlc(
                                    state,
                                    channel_id,
                                    tlc.tlc_id,
                                    tlc.payment_hash,
                                    attempt_id,
                                    tlc.preimage,
                                )
                                .await
                            {
                                warn!(
                                    "Will retry payer reconciliation for TLC {:?} in channel {:?}: {}",
                                    tlc.tlc_id, channel_id, err
                                );
                                payer_effects_applied = false;
                            }
                        }
                    }
                }
                TLCId::Received(id) => {
                    let fulfill = RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                        payment_preimage: tlc.preimage,
                    });
                    self.store.insert_preimage(tlc.payment_hash, tlc.preimage);
                    actor_state.tlc_state.set_received_tlc_removed(id, fulfill);
                    self.store
                        .remove_payment_hold_tlc(&tlc.payment_hash, &channel_id, id);
                    invoice_hashes.push(tlc.payment_hash);
                    actor_state_changed = true;
                }
            }
        }
        for tlc in confirmed_payer_tlcs {
            if let Err(err) = self
                .reconcile_onchain_payer_tlc(
                    state,
                    channel_id,
                    tlc.tlc_id,
                    tlc.payment_hash,
                    tlc.attempt_id,
                    tlc.preimage,
                )
                .await
            {
                warn!(
                    "Will retry payer reconciliation for TLC {:?} in channel {:?}: {}",
                    tlc.tlc_id, channel_id, err
                );
                payer_effects_applied = false;
            }
        }
        invoice_hashes.extend(self.already_fulfilled_onchain_invoice_hashes(actor_state));

        // Received TLCs that timed out on chain are terminal for the payee side: remove the hold
        // record and keep invoice settlement untouched.
        for tlc in collect_onchain_received_timeout_settled_tlcs(actor_state, &self.store) {
            let reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                TlcErr::new(TlcErrorCode::ExpiryTooSoon),
                &NO_SHARED_SECRET,
            ));
            let payment_hash = actor_state
                .tlc_state
                .set_received_tlc_removed(tlc.tlc_id, reason);
            self.store
                .remove_payment_hold_tlc(&payment_hash, &channel_id, tlc.tlc_id);
            actor_state_changed = true;
        }

        // Persist channel TLC mutations before running invoice side effects. This keeps the
        // no-live-actor path aligned with the live RemoveTlc ack ordering: channel state is durable
        // before higher-level payment or invoice notifications are emitted.
        if actor_state_changed {
            self.store.insert_channel_actor_state(actor_state.clone());
            if let Some(ref store_actor) = state.store_actor {
                if let Err(err) = store_actor.cast(StoreActorMessage::RequestBackup) {
                    error!(
                        "Failed to request store backup after on-chain TLC reconciliation: {err}"
                    );
                }
            }
            settlement_state_changed = false;
        }
        for payment_hash in invoice_hashes {
            self.settle_onchain_fulfilled_invoice(payment_hash);
        }

        // Keep waiting while any TLC outcome is still unknown. Once the settlement was confirmed
        // and every TLC is terminal, clear the close flags and drop the funding-script cache entry.
        if !payer_effects_applied || has_unresolved_onchain_tlcs(actor_state) {
            if mark_settlement_confirmed {
                info!(
                    "Channel {channel_id:?} on-chain reconciliation incomplete; CheckChannels will retry"
                );
            }
        } else if let ChannelState::Closed(mut flags) = actor_state.state {
            if flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED) {
                flags.remove(
                    CloseFlags::WAITING_ONCHAIN_SETTLEMENT
                        | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
                );
                actor_state.state = ChannelState::Closed(flags);
                settlement_state_changed = true;
                state.channels_funding_lock_script_cache.remove(&channel_id);
                info!("Channel {channel_id:?} on-chain settlement completed without a live actor");
            }
        }

        if settlement_state_changed {
            self.store.insert_channel_actor_state(actor_state.clone());
            if let Some(ref store_actor) = state.store_actor {
                if let Err(err) = store_actor.cast(StoreActorMessage::RequestBackup) {
                    error!("Failed to request store backup after on-chain settlement finalization: {err}");
                }
            }
        }
    }

    fn settle_onchain_fulfilled_invoice(&self, payment_hash: Hash256) {
        SettleOnChainFulfilledInvoiceCommand::new(payment_hash, &self.store).run();
    }

    fn retry_hold_tlc_sets(&self, myself: &FiberActorRef) {
        let current_time = now_timestamp_as_millis_u64();
        for (payment_hash, hold_tlcs) in self.store.get_node_hold_tlcs() {
            if self.store.get_preimage(&payment_hash).is_some() {
                myself
                    .send_message(FiberActorMessage::new_command(
                        FiberActorCommand::SettleReceivedHoldTlcSet(payment_hash),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                continue;
            }

            let already_timeout = hold_tlcs
                .iter()
                .any(|hold_tlc| current_time >= hold_tlc.hold_expire_at);
            if already_timeout {
                debug!("Timeout {payment_hash} hold tlcs {}", hold_tlcs.len());
                for hold_tlc in hold_tlcs {
                    myself
                        .send_message(FiberActorMessage::new_command(
                            FiberActorCommand::TimeoutHoldTlc(
                                payment_hash,
                                hold_tlc.channel_id,
                                hold_tlc.tlc_id,
                            ),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
            }
        }
    }

    fn timeout_hold_tlc(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
        channel_id: Hash256,
        tlc_id: u64,
    ) {
        let hold_still_owned = self
            .store
            .get_payment_hold_tlcs(payment_hash)
            .iter()
            .any(|hold| hold.channel_id == channel_id && hold.tlc_id == tlc_id);
        if !hold_still_owned {
            trace!(
                "Ignoring stale hold timeout after ownership handoff: payment_hash={:?} channel_id={:?} tlc_id={:?}",
                payment_hash,
                channel_id,
                tlc_id,
            );
            return;
        }

        if self.store.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received) {
            // When invoice is marked as received, we ignore the hold TLC timeout and only
            // remove the TLC when it actually expires. Once it is close enough to expiry,
            // the live ChannelActor removes it during periodic TLC maintenance.
            return;
        }

        let channel_actor_state = self.store.get_channel_actor_state(&channel_id);
        let tlc = channel_actor_state
            .as_ref()
            .and_then(|state| state.tlc_state.get(&TLCId::Received(tlc_id)));
        let Some(tlc) = tlc else {
            trace!(
                "Timeout tlc {:?} (payment hash {:?}) for channel {:?}: tlc is settled or not found, just unhold it",
                tlc_id, payment_hash, channel_id
            );
            // remove hold tlc from store
            self.store
                .remove_payment_hold_tlc(&payment_hash, &channel_id, tlc_id);
            return;
        };

        debug!(
            "Removing timeout hold tlc: payment_hash={:?} channel_id={:?} tlc_id={:?}",
            payment_hash, channel_id, tlc_id
        );

        self.forward_remove_tlc_to_channel(
            myself,
            state,
            channel_id,
            RemoveTlcCommand {
                id: tlc.id(),
                reason: RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                    TlcErr::new(TlcErrorCode::HoldTlcTimeout),
                    &tlc.shared_secret,
                )),
            },
            move |result| FiberActorCommand::RemoveTlcResult {
                channel_id,
                tlc_id,
                hold_payment_hash: Some(payment_hash),
                result,
            },
        );
    }

    fn settle_hold_tlc_set(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
    ) {
        let settlements = SettleTlcSetCommand::new_hold_tlc_set(payment_hash, &self.store).run();
        self.apply_tlc_settlements(myself, state, settlements, Some(payment_hash));
    }

    fn settle_received_hold_tlc_set(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
    ) {
        let settlements =
            SettleTlcSetCommand::new_received_hold_tlc_set(payment_hash, &self.store).run();
        self.apply_tlc_settlements(myself, state, settlements, Some(payment_hash));
    }

    fn settle_tlc_set(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
        channel_tlc_ids: Vec<(Hash256, u64)>,
    ) {
        let settle_command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &self.store);

        self.apply_tlc_settlements(myself, state, settle_command.run(), None);
    }

    fn apply_tlc_settlements(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        settlements: Vec<TlcSettlement>,
        hold_payment_hash: Option<Hash256>,
    ) {
        for tlc_settlement in settlements {
            let channel_id = tlc_settlement.channel_id();
            let tlc_id = tlc_settlement.tlc_id();
            self.forward_remove_tlc_to_channel(
                myself.clone(),
                state,
                channel_id,
                tlc_settlement.remove_tlc_command().clone(),
                move |result| FiberActorCommand::RemoveTlcResult {
                    channel_id,
                    tlc_id,
                    hold_payment_hash,
                    result,
                },
            );
        }
    }

    /// Async version of check_channel_shutdown that runs in spawned task.
    /// Checks if the channel funding cell has been spent (indicating remote force close).
    async fn check_channel_shutdown(
        chain_client: C,
        myself: FiberActorRef,
        channel_id: Hash256,
        funding_lock_script: Script,
    ) {
        match chain_client.get_shutdown_tx(funding_lock_script).await {
            Ok(shutdown_tx) => {
                let _ = myself.send_message(FiberActorMessage::Command(
                    FiberActorCommand::RemoteForceShutdownChannel(channel_id, shutdown_tx),
                ));
            }
            Err(err) => {
                tracing::error!("Failed to check shutdown tx for channel {channel_id:?}: {err:?}");
            }
        }
    }

    /// Async version of check_channel_shutdown_settlement that runs in spawned task.
    /// Checks if the commitment transaction outputs have been spent (indicating settlement complete).
    async fn check_channel_shutdown_settlement(
        chain_client: C,
        myself: FiberActorRef,
        state: ChannelActorState,
    ) {
        let channel_id = state.get_id();
        let ChannelState::Closed(flags) = state.state else {
            return;
        };
        if !flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
            || flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED)
        {
            return;
        }

        let Some(tx_hash) = state.shutdown_transaction_hash.clone() else {
            debug!(
                "stop check channel settlement, {:?} missing shutdown tx hash",
                channel_id
            );
            return;
        };

        let tx_response = match chain_client.get_transaction(tx_hash.clone()).await {
            Ok(response) => response,
            Err(err) => {
                error!(
                    "Failed to load commitment tx {:?} during settlement check: {:?}",
                    tx_hash, err
                );
                return;
            }
        };

        let Some(tx) = tx_response.transaction else {
            debug!(
                "Commitment tx {:?} not available when checking settlement",
                tx_hash
            );
            return;
        };

        let Some(output) = tx.outputs().get(0) else {
            warn!(
                "Commitment tx {:?} has no outputs when checking settlement",
                tx_hash
            );
            return;
        };

        let lock = output.lock();
        let lock_args = lock.args().raw_data();
        if lock_args.len() < 36 {
            warn!(
                "Commitment tx {:?} lock args too short: {:?}",
                tx_hash, lock_args
            );
            return;
        }
        let prefix_lock = lock
            .as_builder()
            .args(lock_args[0..36].to_vec().pack())
            .build();

        let search_key = SearchKey {
            script: prefix_lock.into(),
            script_type: ScriptType::Lock,
            script_search_mode: Some(SearchMode::Prefix),
            with_data: Some(false),
            filter: None,
            group_by_transaction: None,
        };

        match chain_client
            .get_cells(search_key, Order::Desc, 1, None)
            .await
        {
            Ok(response) => {
                let response = crate::ckb::GetCellsResponse::from(response);
                if response.objects.is_empty() {
                    // Notify actor that settlement is complete
                    let _ = myself.send_message(FiberActorMessage::new_event(
                        FiberActorEvent::ChannelSettlementCompleted(channel_id),
                    ));
                }
            }
            Err(err) => {
                error!(
                    "Failed to check commitment cells for {:?}: {:?}",
                    channel_id, err
                );
            }
        }
    }

    // Check shutdown tx of a channel, shutdown channel if channel is force closed by remote
    async fn handle_remote_channel_shutdown(
        &self,
        myself: FiberActorRef,
        channel_id: Hash256,
        response: GetShutdownTxResponse,
    ) {
        let Some(state) = self.store.get_channel_actor_state(&channel_id) else {
            tracing::debug!("skip check channel shutdown, can't find {channel_id:?} actor state");
            return;
        };

        if !matches!(
            state.state,
            ChannelState::ChannelReady | ChannelState::ShuttingDown(..)
        ) {
            return;
        }

        if let GetShutdownTxResponse {
            transaction: Some(tx),
            tx_status: TxStatus::Committed(..),
        } = response
        {
            // we only check remote sent force close transaction here
            if tx.outputs().len() == 1 {
                if let Some(output) = tx.outputs().get(0) {
                    // Check if channel is force closed by counter party
                    let lock_args =
                        &blake2b_256(state.get_commitment_lock_script_xonly(true))[0..20];
                    if &output.lock().args().raw_data()[0..20] == lock_args {
                        let channel_id = state.get_id();
                        let pubkey = state.get_remote_pubkey();
                        let tx_hash = tx.hash();
                        tracing::debug!("channel {channel_id:?} is shutdown by remote");
                        myself
                            .send_message(FiberActorMessage::Event(
                                FiberActorEvent::ClosingTransactionConfirmed(
                                    pubkey, channel_id, tx_hash, true, false,
                                ),
                            ))
                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    }
                }
            }
        }
    }

    pub fn add_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let payment_hash = invoice.payment_hash();
        if self.store.get_invoice(payment_hash).is_some() {
            return Err(InvoiceError::InvoiceAlreadyExists);
        }
        self.store.insert_invoice(invoice, preimage)
    }

    pub fn settle_invoice(
        &self,
        myself: &FiberActorRef,
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<(), SettleInvoiceError> {
        let invoice = self
            .store
            .get_invoice(&payment_hash)
            .ok_or(SettleInvoiceError::InvoiceNotFound)?;

        let hash_algorithm = invoice.hash_algorithm().copied().unwrap_or_default();
        let hash = hash_algorithm.hash(payment_preimage);
        if hash.as_slice() != payment_hash.as_ref() {
            return Err(SettleInvoiceError::HashMismatch);
        }

        // Allow only settling Received invoice. When the invoice is Received, it's safe to notify
        // that the preimage can be revealed.
        match self.store.get_invoice_status(&payment_hash) {
            Some(CkbInvoiceStatus::Received) => {}
            Some(CkbInvoiceStatus::Open) => {
                if invoice.is_expired() {
                    return Err(SettleInvoiceError::InvoiceAlreadyExpired);
                }
                return Err(SettleInvoiceError::InvoiceStillOpen);
            }
            Some(CkbInvoiceStatus::Cancelled) => {
                return Err(SettleInvoiceError::InvoiceAlreadyCancelled);
            }
            Some(CkbInvoiceStatus::Expired) => {
                return Err(SettleInvoiceError::InvoiceAlreadyExpired);
            }
            Some(CkbInvoiceStatus::Paid) => return Err(SettleInvoiceError::InvoiceAlreadyPaid),
            None => return Err(SettleInvoiceError::InvoiceNotFound),
        }

        self.store.insert_preimage(payment_hash, payment_preimage);
        // Notify watchtower about the preimage so it can settle TLCs on-chain if needed
        // (e.g., after force close).
        myself
            .send_message(FiberActorMessage::new_notification(
                NetworkServiceEvent::PreimageCreated(payment_hash, payment_preimage),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        // We will send network actor a message to settle the invoice immediately if possible.
        let _ = myself.send_message(FiberActorMessage::new_command(
            FiberActorCommand::SettleReceivedHoldTlcSet(payment_hash),
        ));

        Ok(())
    }

    pub fn cancel_invoice(
        &self,
        myself: &FiberActorRef,
        payment_hash: Hash256,
    ) -> Result<(), CancelInvoiceError> {
        let invoice = self
            .store
            .get_invoice(&payment_hash)
            .ok_or(CancelInvoiceError::InvoiceNotFound)?;
        let status = match self
            .store
            .get_invoice_status(&payment_hash)
            .ok_or(CancelInvoiceError::InvoiceNotFound)?
        {
            CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
            status => status,
        };

        match status {
            CkbInvoiceStatus::Paid => return Err(CancelInvoiceError::InvoiceAlreadyPaid),
            CkbInvoiceStatus::Cancelled => return Err(CancelInvoiceError::InvoiceAlreadyCancelled),
            CkbInvoiceStatus::Received if self.store.get_preimage(&payment_hash).is_some() => {
                return Err(CancelInvoiceError::PaymentPreimageAlreadyExists);
            }
            _ => {}
        }

        self.store
            .update_invoice_status(&payment_hash, CkbInvoiceStatus::Cancelled)
            .map_err(|err| CancelInvoiceError::InternalError(err.to_string()))?;

        let _ = myself.send_message(FiberActorMessage::new_command(
            FiberActorCommand::SettleHoldTlcSet(payment_hash),
        ));

        Ok(())
    }

    async fn handle_send_onion_packet_command(
        &self,
        state: &mut FiberActorState<S, C>,
        command: SendOnionPacketCommand,
    ) -> Result<(), TlcErr> {
        trace!("Entering handle_send_onion_packet_command");
        let SendOnionPacketCommand {
            peeled_onion_packet,
            previous_tlc,
            payment_hash,
            attempt_id,
        } = command;

        // Trampoline forwarding: the onion for this node is the last hop, but contains an
        // encrypted payload telling us the real final recipient and parameters.
        if let Some(trampoline_bytes) = peeled_onion_packet.current.trampoline_onion() {
            return self
                .forward_trampoline_packet(
                    state,
                    &trampoline_bytes,
                    previous_tlc,
                    payment_hash,
                    peeled_onion_packet.current.amount,
                )
                .await;
        }

        let info = peeled_onion_packet.current.clone();
        let shared_secret = peeled_onion_packet.shared_secret;
        let channel_outpoint = OutPoint::new(info.funding_tx_hash.into(), 0);
        let channel_id = match state.outpoint_channel_map.get(&channel_outpoint) {
            Some(channel_id) if state.is_channel_online(channel_id) => *channel_id,
            _ => {
                error!(
                    "Channel id not found in outpoint_channel_map with {:?}, are we connected to the peer?",
                    channel_outpoint
                );
                let tlc_err = TlcErr::new_channel_fail(
                    TlcErrorCode::UnknownNextPeer,
                    state.get_public_key(),
                    channel_outpoint.clone(),
                    None,
                );
                return Err(tlc_err);
            }
        };

        let (send, _recv) = oneshot::channel::<Result<AddTlcResponse, TlcErr>>();
        // explicitly don't wait for the response, we will handle the result in AddTlcResult
        let rpc_reply = RpcReplyPort::from(send);
        let command = ChannelCommand::AddTlc(
            AddTlcCommand {
                amount: info.amount,
                payment_hash,
                attempt_id,
                expiry: info.expiry,
                hash_algorithm: info.hash_algorithm,
                onion_packet: peeled_onion_packet.next.clone(),
                shared_secret,
                is_trampoline_hop: false,
                previous_tlc,
            },
            rpc_reply,
        );
        trace!(
            "Sending AddTlcCommand to {}, command {:?}",
            channel_id,
            command
        );
        // we have already checked the channel_id is valid,
        match state.send_command_to_channel(channel_id, command).await {
            Ok(_) => {
                return Ok(());
            }
            Err(err) => {
                error!(
                    "Failed to send onion packet to channel: {:?} with err: {:?}",
                    channel_id, err
                );
                let tlc_error = self.get_tlc_error(state, &err, &channel_outpoint);
                return Err(tlc_error);
            }
        }
    }

    async fn forward_trampoline_packet(
        &self,
        state: &mut FiberActorState<S, C>,
        trampoline_bytes: &[u8],
        previous_tlc: Option<PrevTlcInfo>,
        payment_hash: Hash256,
        incoming_amount: u128,
    ) -> Result<(), TlcErr> {
        if !state.features.supports_trampoline_routing() {
            error!(
                "Trampoline forwarding rejected: local node does not support trampoline routing"
            );
            return Err(TlcErr::new_node_fail(
                TlcErrorCode::RequiredNodeFeatureMissing,
                state.get_public_key(),
            ));
        }
        let Some(prev_tlc) = previous_tlc else {
            error!("Trampoline forwarding rejected: missing previous TLC");
            return Err(TlcErr::new_node_fail(
                TlcErrorCode::InvalidOnionPayload,
                state.get_public_key(),
            ));
        };
        let trampoline_packet = TrampolineOnionPacket::new(trampoline_bytes.to_vec());
        let prev_channel_state = self
            .store
            .get_channel_actor_state(&prev_tlc.prev_channel_id)
            .ok_or_else(|| {
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            })?;
        let udt_type_script = prev_channel_state.funding_udt_type_script.clone();
        let peeled_trampoline = trampoline_packet
            .peel(&state.private_key, Some(payment_hash.as_ref()), SECP256K1)
            .map_err(|_| {
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            })?;
        match peeled_trampoline.current {
            TrampolineHopPayload::Forward {
                next_node_id,
                amount_to_forward,
                hash_algorithm,
                build_max_fee_amount,
                tlc_expiry_delta,
                tlc_expiry_limit,
                max_parts,
            } => {
                if incoming_amount <= amount_to_forward {
                    error!(
                        "Trampoline forwarding fee insufficient: incoming {}, forward {}",
                        incoming_amount, amount_to_forward
                    );
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::FeeInsufficient,
                        state.get_public_key(),
                    ));
                }
                let available_fee_amount = incoming_amount.saturating_sub(amount_to_forward);
                if available_fee_amount != build_max_fee_amount {
                    error!(
                        "Trampoline forwarding fee mismatch: available {}, build max {}",
                        available_fee_amount, build_max_fee_amount
                    );
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::InvalidOnionPayload,
                        state.get_public_key(),
                    ));
                }

                let Some(remaining_trampoline_onion) =
                    peeled_trampoline.next.map(|p| p.into_bytes())
                else {
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::InvalidOnionPayload,
                        state.get_public_key(),
                    ));
                };

                let Some(prev_tlc_info) = prev_channel_state
                    .tlc_state
                    .get(&TLCId::Received(prev_tlc.prev_tlc_id))
                else {
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::TemporaryNodeFailure,
                        state.get_public_key(),
                    ));
                };
                if prev_tlc_info.payment_hash != payment_hash {
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::TemporaryNodeFailure,
                        state.get_public_key(),
                    ));
                }

                let max_outgoing_tlc_expiry = prev_tlc_info
                    .expiry
                    .checked_sub(prev_channel_state.local_tlc_info.tlc_expiry_delta)
                    .ok_or_else(|| TlcErr::new(TlcErrorCode::IncorrectTlcExpiry))?;
                let min_outgoing_tlc_expiry = now_timestamp_as_millis_u64()
                    .checked_add(tlc_expiry_delta)
                    .ok_or_else(|| TlcErr::new(TlcErrorCode::IncorrectTlcExpiry))?;
                if min_outgoing_tlc_expiry > max_outgoing_tlc_expiry {
                    return Err(TlcErr::new(TlcErrorCode::IncorrectTlcExpiry));
                }

                let request = TrampolineForwardingRequest {
                    payment_hash,
                    next_node_id,
                    amount_to_forward,
                    hash_algorithm,
                    build_max_fee_amount,
                    tlc_expiry_delta,
                    tlc_expiry_limit,
                    max_parts,
                    udt_type_script,
                    remaining_trampoline_onion,
                    previous_tlc: prev_tlc,
                    max_outgoing_tlc_expiry,
                };

                self.dispatch_trampoline_forwarding(state, request).await
            }
            TrampolineHopPayload::Final { .. } => {
                // The channel actor should directly settle when this node is the final recipient.
                // This case should not happen.
                Err(TlcErr::new_node_fail(
                    TlcErrorCode::TemporaryNodeFailure,
                    state.get_public_key(),
                ))
            }
        }
    }

    async fn dispatch_trampoline_forwarding(
        &self,
        state: &mut FiberActorState<S, C>,
        request: TrampolineForwardingRequest,
    ) -> Result<(), TlcErr> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(lsp_service) = state.lsp_service.clone() {
            return self
                .accept_or_dispatch_lsp_trampoline(state, lsp_service, request)
                .await;
        }

        self.dispatch_trampoline_payment(state, request).await
    }

    async fn dispatch_trampoline_payment(
        &self,
        state: &mut FiberActorState<S, C>,
        request: TrampolineForwardingRequest,
    ) -> Result<(), TlcErr> {
        self.try_dispatch_trampoline_payment(state, request)
            .await
            .map_err(|error| {
                error!("Failed to start trampoline payment: {error}");
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            })
    }

    async fn try_dispatch_trampoline_payment(
        &self,
        state: &mut FiberActorState<S, C>,
        request: TrampolineForwardingRequest,
    ) -> Result<(), LspPaymentDispatchError> {
        let payment_hash = request.payment_hash;
        let payment_data = request.into_send_payment_data().map_err(|reason| {
            LspPaymentDispatchError::Permanent {
                reason: format!("invalid hosted payment request: {reason}"),
                error_code: TlcErrorCode::InvalidOnionPayload,
            }
        })?;
        let (send, _recv) = oneshot::channel();
        let rpc_reply = RpcReplyPort::from(send);

        match self
            .start_payment_actor(
                state.network.clone(),
                state,
                payment_hash,
                PaymentActorMessage::SendPayment(payment_data, rpc_reply),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => Err(LspPaymentDispatchError::Temporary {
                reason: format!("failed to start hosted payment: {error}"),
            }),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn accept_or_dispatch_lsp_trampoline(
        &self,
        state: &mut FiberActorState<S, C>,
        lsp_service: ActorRef<LspServiceMessage>,
        request: TrampolineForwardingRequest,
    ) -> Result<(), TlcErr> {
        let decision = ractor::call_t!(
            lsp_service,
            |reply| LspServiceMessage::AcceptTrampolineDelivery(request.clone(), reply),
            5_000
        );
        match decision {
            Ok(Ok(LspDeliveryDecision::NotHosted)) => {
                self.dispatch_trampoline_payment(state, request).await
            }
            Ok(Ok(LspDeliveryDecision::Buffered)) => Ok(()),
            Ok(Err(error)) => {
                warn!("Hosted trampoline delivery rejected: {error}");
                Err(TlcErr::new_node_fail(
                    TlcErrorCode::TemporaryNodeFailure,
                    state.get_public_key(),
                ))
            }
            Err(error) => {
                error!("Failed to consult LSP delivery service: {error}");
                Err(TlcErr::new_node_fail(
                    TlcErrorCode::TemporaryNodeFailure,
                    state.get_public_key(),
                ))
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn fail_buffered_trampoline(
        &self,
        state: &mut FiberActorState<S, C>,
        lsp_service: Option<ActorRef<LspServiceMessage>>,
        request: TrampolineForwardingRequest,
        reason: String,
        error_code: TlcErrorCode,
    ) -> Result<bool, String> {
        if let Some(session) = state.store.get_payment_session(request.payment_hash) {
            if matches!(
                session.status,
                PaymentStatus::Created | PaymentStatus::Inflight
            ) {
                return Ok(false);
            }
            return Ok(session.status == PaymentStatus::Failed);
        }

        let payment_hash = request.payment_hash;
        let payment_data = request.into_send_payment_data()?;
        let now = now_timestamp_as_millis_u64();
        let mut session = PaymentSession::new_session(&state.store, payment_data, 0);
        session.status = PaymentStatus::Failed;
        session.last_error = Some(reason);
        session.last_error_code = Some(error_code);
        session.last_updated_at = now;
        state.store.insert_payment_session(session.clone());
        let settlement = state.settle_trampoline_payment(&session, None, None).await;
        if settlement.is_ok() {
            if let Some(lsp_service) = lsp_service {
                let _ = lsp_service.send_message(LspServiceMessage::PaymentOutcomeSettled {
                    payment_hash,
                    payment_status: PaymentStatus::Failed,
                    failure: session.last_error.clone(),
                });
            }
        }
        settlement?;
        Ok(true)
    }

    fn get_tlc_error(
        &self,
        state: &mut FiberActorState<S, C>,
        error: &Error,
        channel_outpoint: &OutPoint,
    ) -> TlcErr {
        let node_id = state.get_public_key();
        match error {
            Error::ChannelNotFound(_) | Error::PeerNotFound(_) | Error::NoSupportedAddress(_) => {
                TlcErr::new_channel_fail(
                    TlcErrorCode::UnknownNextPeer,
                    node_id,
                    channel_outpoint.clone(),
                    None,
                )
            }
            Error::ChannelError(_) => TlcErr::new_channel_fail(
                TlcErrorCode::TemporaryChannelFailure,
                node_id,
                channel_outpoint.clone(),
                None,
            ),
            _ => {
                error!(
                    "Failed to send onion packet to channel: {:?} with err: {:?}",
                    channel_outpoint, error
                );
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            }
        }
    }

    async fn on_remove_tlc_event(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
    ) {
        self.resume_payment_actor_and_send_command(
            myself,
            state,
            payment_hash,
            PaymentActorMessage::OnRemoveTlcEvent { attempt_id, reason },
        )
        .await;
    }

    /// Serialize an on-chain payer outcome through the PaymentActor and wait until the exact
    /// attempt and aggregate session have been persisted. This acknowledgement lets channel
    /// reconciliation safely finalize without losing the higher-level payment update on restart.
    async fn reconcile_onchain_payer_tlc(
        &self,
        state: &mut FiberActorState<S, C>,
        channel_id: Hash256,
        tlc_id: TLCId,
        payment_hash: Hash256,
        attempt_id: u64,
        payment_preimage: Hash256,
    ) -> Result<(), String> {
        let source_channel = self
            .store
            .get_channel_actor_state(&channel_id)
            .ok_or_else(|| {
                format!("source channel not found for on-chain payer TLC: channel_id={channel_id:?}, tlc_id={tlc_id:?}")
            })?;
        let source_tlc = source_channel.tlc_state.get(&tlc_id).ok_or_else(|| {
            format!("source TLC not found for on-chain payer reconciliation: channel_id={channel_id:?}, tlc_id={tlc_id:?}")
        })?;
        if !source_tlc.is_offered()
            || source_tlc.forwarding_tlc.is_some()
            || source_tlc.payment_hash != payment_hash
            || source_tlc.attempt_id != Some(attempt_id)
        {
            return Err(format!(
                "source TLC metadata does not match on-chain payer reconciliation: channel_id={channel_id:?}, tlc_id={tlc_id:?}, payment_hash={payment_hash:?}, attempt_id={attempt_id}"
            ));
        }
        let source_channel_outpoint = source_channel
            .get_funding_transaction_outpoint()
            .ok_or_else(|| {
                format!("source channel funding outpoint not found for on-chain payer TLC: channel_id={channel_id:?}, tlc_id={tlc_id:?}")
            })?;

        if let Some(attempt) = self.store.get_attempt(payment_hash, attempt_id) {
            // Attempt ids are local to one payment generation and are reused after retry deletes
            // the previous attempts. A proof from the old force-closed channel must therefore be
            // acknowledged as stale instead of being applied to a replacement attempt that only
            // happens to have the same `(payment_hash, attempt_id)`.
            if !attempt.first_hop_channel_outpoint_eq(&source_channel_outpoint) {
                warn!(
                    "Ignoring stale on-chain payer TLC {:?} in channel {:?}: attempt {:?} now belongs to first-hop channel {:?}",
                    tlc_id,
                    channel_id,
                    attempt_id,
                    attempt.first_hop_channel_outpoint()
                );
                return Ok(());
            }

            if let Some(existing_preimage) = attempt.preimage {
                if existing_preimage != payment_preimage {
                    return Err(format!(
                        "on-chain fulfill preimage conflicts with payment attempt: payment_hash={payment_hash:?}, attempt_id={attempt_id}"
                    ));
                }
            }

            if attempt.is_success() && attempt.preimage == Some(payment_preimage) {
                let aggregate_is_fully_paid = self
                    .store
                    .get_payment_session(payment_hash)
                    .is_some_and(|session| {
                        session
                            .attempts()
                            .filter(|attempt| attempt.is_success())
                            .map(|attempt| attempt.route.receiver_amount())
                            .sum::<u128>()
                            >= session.request.amount
                    });
                let persisted_status = self.store.get_persisted_payment_status(payment_hash);

                // Per-attempt acknowledgement is independent of the aggregate payment. A
                // successfully persisted MPP shard is already reconciled while other shards are
                // still in flight or have left the aggregate Failed. If successful shards cover
                // the full amount, however, a non-Success session is the crash window between the
                // attempt and session writes and must still be repaired through PaymentActor.
                if !aggregate_is_fully_paid || persisted_status == Some(PaymentStatus::Success) {
                    if persisted_status == Some(PaymentStatus::Success) {
                        // This also closes the crash window between persisting the final session
                        // and removing the retry index. Clearing an already-empty index is harmless.
                        self.store.clear_attempts_channel_index(payment_hash);
                    }
                    return Ok(());
                }
            }
        }

        let (send, recv) = oneshot::channel();
        let message = PaymentActorMessage::ReconcileOnChainFulfill {
            attempt_id,
            payment_preimage,
            reply: RpcReplyPort::from(send),
        };

        if let Some(actor) = state.inflight_payments.get(&payment_hash) {
            actor.send_message(message).map_err(|err| {
                format!(
                    "failed to send on-chain fulfill to payment actor for {payment_hash:?}: {err}"
                )
            })?;
        } else {
            self.start_payment_actor(state.network.clone(), state, payment_hash, message)
                .await?;
        }

        tokio::time::timeout(Duration::from_millis(DEFAULT_CHAIN_ACTOR_TIMEOUT), recv)
            .await
            .map_err(|_| {
                format!(
                    "timed out reconciling on-chain fulfill for {payment_hash:?} attempt {attempt_id}"
                )
            })?
            .map_err(|err| {
                format!(
                    "payment actor stopped while reconciling {payment_hash:?} attempt {attempt_id}: {err}"
                )
            })?
    }

    fn on_get_payment(&self, payment_hash: &Hash256) -> Result<SendPaymentResponse, Error> {
        match self.store.get_payment_session(*payment_hash) {
            Some(session_state) => Ok(session_state.into()),
            None => Err(Error::InvalidParameter(format!(
                "Payment session not found: {:?}",
                payment_hash
            ))),
        }
    }

    async fn on_add_tlc_result_event(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
        add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
        previous_tlc: Option<PrevTlcInfo>,
    ) {
        if let Some(PrevTlcInfo {
            prev_channel_id: channel_id,
            prev_tlc_id: tlc_id,
            ..
        }) = previous_tlc
        {
            myself
                .send_message(FiberActorMessage::new_command(
                    FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                        channel_id,
                        command: ChannelCommand::NotifyEvent(ChannelEvent::ForwardTlcResult(
                            ForwardTlcResult {
                                payment_hash,
                                channel_id,
                                tlc_id,
                                add_tlc_result: add_tlc_result.clone(),
                            },
                        )),
                    }),
                ))
                .expect("network actor alive");
            return;
        }

        self.resume_payment_actor_and_send_command(
            myself,
            state,
            payment_hash,
            PaymentActorMessage::OnAddTlcResultEvent {
                attempt_id,
                add_tlc_result,
            },
        )
        .await;
    }

    async fn resume_payment_actor_and_send_command(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
        message: PaymentActorMessage,
    ) {
        if let Some(actor) = state.inflight_payments.get(&payment_hash) {
            if let Err(err) = actor.send_message(message) {
                debug!(
                    "PaymentActor message dropped because payment actor is likely stopping, error: {err}"
                );
            }
        } else {
            debug!(
                "Can't find inflight payment actor for {payment_hash:?}, start a new payment actor"
            );

            if let Err(e) = self
                .start_payment_actor(myself, state, payment_hash, message)
                .await
            {
                warn!("Failed to resume payment actor: {}", e);
            }
        }
    }

    async fn start_payment_actor(
        &self,
        myself: FiberActorRef,
        state: &mut FiberActorState<S, C>,
        payment_hash: Hash256,
        init_command: PaymentActorMessage,
    ) -> Result<(), String> {
        if state.inflight_payments.contains_key(&payment_hash) {
            error!("Already had a payment actor with the same hash {payment_hash:?}");

            if let PaymentActorMessage::SendPayment(_, reply) = init_command {
                let _ = reply.send(Err(format!(
                    "Payment session already exists, stop start new payment actor for {payment_hash:?}"
                )));
            }
            return Err(format!(
                "Payment session already exists for {payment_hash:?}"
            ));
        }

        let args = PaymentActorArguments {
            payment_hash,
            init_command,
        };
        match Actor::spawn_linked(
            Some(format!(
                "Payment-{} Node({:?})",
                payment_hash,
                myself.get_name(),
            )),
            PaymentActor::new(
                self.store.clone(),
                self.network_graph.clone(),
                myself.clone(),
            ),
            args,
            myself.get_cell(),
        )
        .await
        {
            Ok((actor, _handle)) => {
                debug!("Payment actor start {payment_hash}");
                #[cfg(debug_assertions)]
                debug_event!(
                    state.network,
                    format!("payment actor start: {payment_hash:?}")
                );
                state.inflight_payments.insert(payment_hash, actor);
                Ok(())
            }
            Err(err) => {
                error!("Failed to start payment actor: {:?}", err);
                Err(format!("Failed to start payment actor: {:?}", err))
            }
        }
    }

    async fn on_build_payment_router(
        &self,
        command: BuildRouterCommand,
    ) -> Result<PaymentRouter, Error> {
        // Only proceed if we have at least one hop requirement
        let Some(_last_hop) = command.hops_info.last() else {
            return Err(Error::InvalidParameter(
                "No hop requirements provided".to_string(),
            ));
        };

        let source = self.network_graph.read().await.get_source_pubkey();
        let router_hops = self
            .network_graph
            .read()
            .await
            .build_path(source, command)?;

        Ok(PaymentRouter { router_hops })
    }

    /// Core logic for funding a channel transaction and sending the TxUpdate.
    /// Used by both `UpdateChannelFunding` (retry_count=0) and
    /// `RetryUpdateChannelFunding` (retry_count>0).
    async fn do_update_channel_funding(
        &self,
        myself: &FiberActorRef,
        state: &mut FiberActorState<S, C>,
        channel_id: Hash256,
        retry_count: u32,
        transaction: Transaction,
        request: FundingRequest,
    ) -> crate::Result<()> {
        debug!(
            "do_update_channel_funding: channel_id={:?}, attempt={}/{}, local_amount={}, remote_amount={}, fee_rate={}, has_udt={}",
            channel_id,
            retry_count + 1,
            FUNDING_RETRY_MAX_TOTAL_ATTEMPTS,
            request.local_amount,
            request.remote_amount,
            request.funding_fee_rate,
            request.udt_type_script.is_some(),
        );
        let prev_funding_tx_hash = state
            .store
            .get_channel_actor_state(&channel_id)
            .and_then(|s| {
                s.funding_tx.as_ref().map(|tx| {
                    let hash: Hash256 = tx.calc_tx_hash().into();
                    hash
                })
            });
        let tx_for_retry = transaction.clone();
        let request_for_retry = request.clone();
        let old_tx = transaction.into_view();
        let mut tx = FundingTx::new();
        tx.update_for_self(old_tx);
        let tx = match self
            .fund(tx, request)
            .await
            .and_then(|tx| tx.into_inner().ok_or(FundingError::AbsentTx))
        {
            Ok(tx) => {
                let new_funding_tx_hash: Hash256 = tx.hash().into();
                if let Some(prev_hash) = prev_funding_tx_hash.filter(|h| *h != new_funding_tx_hash)
                {
                    let _ = self
                        .chain_actor
                        .send_message(CkbChainMessage::RemoveFundingTx(prev_hash));
                }
                tx
            }
            Err(err) => {
                let should_abort = schedule_funding_retry(
                    myself,
                    &err,
                    retry_count,
                    channel_id,
                    "fund channel",
                    move |next| {
                        FiberActorCommand::RetryUpdateChannelFunding(
                            channel_id,
                            tx_for_retry,
                            request_for_retry,
                            next,
                        )
                    },
                );
                if should_abort {
                    state.abort_funding(Either::Left(channel_id)).await;
                }
                return Ok(());
            }
        };
        if tracing::enabled!(target: "fnn::fiber::network::funding", tracing::Level::DEBUG) {
            let tx_json: ckb_jsonrpc_types::Transaction = tx.data().into();
            let tx_json = serde_json::to_string(&tx_json).unwrap_or_default();
            debug!(target: "fnn::fiber::network::funding", "Funding transaction updated on our part (attempt {}/{}): {}", retry_count + 1, FUNDING_RETRY_MAX_TOTAL_ATTEMPTS, tx_json);
        }
        state
            .send_command_to_channel(
                channel_id,
                ChannelCommand::TxCollaborationCommand(TxCollaborationCommand::TxUpdate(
                    TxUpdateCommand {
                        transaction: tx.data(),
                    },
                )),
            )
            .await?;
        Ok(())
    }

    /// Core logic for signing a funding transaction and sending TxSignatures.
    /// Used by both `SignFundingTx` (retry_count=0) and
    /// `RetrySignFundingTx` (retry_count>0).
    #[allow(clippy::too_many_arguments)]
    async fn do_sign_funding_tx(
        &self,
        myself: &FiberActorRef,
        state: &mut FiberActorState<S, C>,
        channel_id: Hash256,
        retry_count: u32,
        target: Pubkey,
        funding_tx: Transaction,
        partial_witnesses: Option<Vec<Vec<u8>>>,
    ) -> crate::Result<()> {
        // Guard against stale retries: if the channel has been aborted
        // or closed since the retry was scheduled, skip signing.
        if state.store.get_channel_actor_state(&channel_id).is_none() {
            debug!(
                "Skipping do_sign_funding_tx: channel {:?} no longer exists (likely aborted)",
                channel_id
            );
            return Ok(());
        }
        let tx_hash: Hash256 = funding_tx.calc_tx_hash().into();
        let has_partial_witnesses = partial_witnesses.is_some();
        debug!(
            "do_sign_funding_tx: channel_id={:?}, target={:?}, tx_hash={:?}, has_partial_witnesses={}, attempt={}/{}",
            channel_id,
            target,
            tx_hash,
            has_partial_witnesses,
            retry_count + 1,
            FUNDING_RETRY_MAX_TOTAL_ATTEMPTS,
        );

        let funding_tx_for_retry = funding_tx.clone();
        let partial_witnesses_for_retry = partial_witnesses.clone();

        let funding_tx = match partial_witnesses {
            Some(partial_witnesses) => funding_tx
                .into_view()
                .as_advanced_builder()
                .set_witnesses(partial_witnesses.into_iter().map(|x| x.pack()).collect())
                .build(),
            None => funding_tx.into_view(),
        };

        let mut signed_funding_tx = match call_t!(
            self.chain_actor,
            CkbChainMessage::Sign,
            DEFAULT_CHAIN_ACTOR_TIMEOUT,
            funding_tx.into()
        )
        .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
        {
            Ok(funding_tx) => funding_tx,
            Err(err) => {
                let should_abort = schedule_funding_retry(
                    myself,
                    &err,
                    retry_count,
                    channel_id,
                    "sign funding transaction",
                    move |next| {
                        FiberActorCommand::RetrySignFundingTx(
                            target,
                            channel_id,
                            funding_tx_for_retry,
                            partial_witnesses_for_retry,
                            next,
                        )
                    },
                );
                if should_abort {
                    let abort_msg = FiberMessageWithTarget {
                        target,
                        message: FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::TxAbort(TxAbort {
                                channel_id,
                                message: format!("Failed to sign funding transaction: {}", err)
                                    .as_bytes()
                                    .to_vec(),
                            }),
                        ),
                    };
                    myself
                        .send_message(FiberActorMessage::new_command(
                            FiberActorCommand::SendFiberMessage(abort_msg),
                        ))
                        .expect("network actor alive");
                    state.abort_funding(Either::Left(channel_id)).await;
                }
                return Ok(());
            }
        };
        debug!(
            "Funding transaction signed (attempt {}/{}): {:?}",
            retry_count + 1,
            FUNDING_RETRY_MAX_TOTAL_ATTEMPTS,
            &signed_funding_tx
        );

        let funding_tx = signed_funding_tx.take().expect("take tx");
        let witnesses = funding_tx.witnesses();

        let Some(channel_actor) = state.channels.get(&channel_id).cloned() else {
            debug!(
                "Skipping signed funding tx for channel {:?}: channel actor no longer exists",
                channel_id
            );
            return Ok(());
        };

        match call_t!(
            channel_actor,
            |reply| ChannelActorMessage::Command(ChannelCommand::FundingTxSigned(
                funding_tx.data(),
                reply,
            )),
            PEER_CHANNEL_RESPONSE_TIMEOUT
        ) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(
                    "Discarding signed funding tx for channel {:?}: channel rejected FundingTxSigned: {}",
                    channel_id, err
                );
                return Ok(());
            }
            Err(err) => {
                warn!(
                    "Discarding signed funding tx for channel {:?}: failed to acknowledge FundingTxSigned: {}",
                    channel_id, err
                );
                return Ok(());
            }
        }

        if has_partial_witnesses {
            let outpoint = funding_tx
                .output_pts_iter()
                .next()
                .expect("funding tx output exists");

            myself
                .send_message(FiberActorMessage::new_event(
                    FiberActorEvent::FundingTransactionPending(
                        funding_tx.data(),
                        outpoint,
                        channel_id,
                    ),
                ))
                .expect("network actor alive");
            debug!("Fully signed funding tx {:?}", &funding_tx);
        } else {
            debug!("Partially signed funding tx {:?}", &funding_tx);
        }

        let msg = FiberMessageWithTarget {
            target,
            message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxSignatures(
                TxSignatures {
                    channel_id,
                    witnesses: witnesses.into_iter().map(|x| x.unpack()).collect(),
                },
            )),
        };

        state
            .trace_tx(tx_hash, InFlightCkbTxKind::Funding(channel_id))
            .await?;

        myself
            .send_message(FiberActorMessage::new_command(
                FiberActorCommand::SendFiberMessage(msg),
            ))
            .expect("network actor alive");
        Ok(())
    }

    async fn fund(
        &self,
        tx: FundingTx,
        request: FundingRequest,
    ) -> Result<FundingTx, FundingError> {
        trace!(
            "Forwarding Fund request to ckb chain actor: local_amount={}, remote_amount={}, fee_rate={}",
            request.local_amount,
            request.remote_amount,
            request.funding_fee_rate,
        );
        call_t!(
            self.chain_actor.clone(),
            CkbChainMessage::Fund,
            DEFAULT_CHAIN_ACTOR_TIMEOUT,
            tx,
            request
        )?
    }
}

/// The public Fiber node actor. Public P2P, gossip and peer-session lifecycle
/// live in this wrapper; channel/payment behavior lives in the core.
pub struct NetworkActor<S, C> {
    core: FiberActorCore<S, C>,
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
    pub fn new(
        event_sender: mpsc::Sender<NetworkServiceEvent>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        store_actor: Option<ActorRef<StoreActorMessage>>,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
        chain_client: C,
    ) -> Self {
        Self {
            core: FiberActorCore::new(
                event_sender,
                chain_actor,
                store,
                store_actor,
                network_graph,
                chain_client,
            ),
        }
    }

    async fn handle_public_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        event: PublicNetworkEvent,
    ) -> crate::Result<()> {
        match event {
            PublicNetworkEvent::PeerConnected(pubkey, session) => {
                state.on_peer_connected(pubkey, &session).await;
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::PeerConnected(pubkey, session.address),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                Ok(())
            }
            PublicNetworkEvent::PeerDisconnected(pubkey, session) => {
                state.on_peer_disconnected(pubkey, session.id);
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::PeerDisConnected(pubkey, session.address),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                Ok(())
            }
            PublicNetworkEvent::GossipMessageUpdates(gossip_message_updates) => {
                let mut graph = self.core.network_graph.write().await;
                graph.update_for_messages(gossip_message_updates.messages);
                debug_event!(
                    FiberActorRef::from_network(&myself),
                    "Received gossip message updates"
                );
                Ok(())
            }
            PublicNetworkEvent::FiberMessage(pubkey, FiberMessage::Init(init), ingress_permit) => {
                let result = state
                    .on_init_msg(myself, pubkey, init)
                    .await
                    .map_err(Error::from);
                drop(ingress_permit);
                result
            }
            PublicNetworkEvent::FiberMessage(pubkey, message, ingress_permit) => {
                let fiber = FiberActorRef::from_network(&myself);
                let result = self
                    .core
                    .handle_peer_message(fiber, &mut state.fiber, pubkey, message)
                    .await;
                drop(ingress_permit);
                if matches!(result, Ok(FiberMessageDisposition::UnknownChannel)) {
                    let banned = state.record_invalid_peer_message(pubkey);
                    if banned {
                        state.disconnect_peer_for_message_policy(pubkey).await;
                    }
                }
                result.map(|_| ())
            }
        }
    }

    async fn handle_public_command(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        command: PublicNetworkCommand,
    ) -> crate::Result<()> {
        match command {
            PublicNetworkCommand::ConnectPeer(addr, save, source, rpc_reply) => {
                let control = state.public.control.clone();
                if matches!(source, PeerConnectSource::Manual) {
                    state.resume_peer_auto_reconnect_by_address(&addr);
                }
                if save {
                    state.enqueue_peer_address_to_save(addr.clone());
                }
                match control.dial(addr, TargetProtocol::All).await {
                    Ok(()) => {
                        if let Some(reply) = rpc_reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    Err(err) => {
                        if let Some(reply) = rpc_reply {
                            let _ = reply.send(Err(err.to_string()));
                        }
                        return Err(err.into());
                    }
                }
                Ok(())
            }
            PublicNetworkCommand::ConnectPeerWithPubkey(pubkey, addr_type, source, reply) => {
                let control = state.public.control.clone();
                let addresses = state.get_peer_addresses_by_pubkey(&pubkey);
                let has_known_addresses = !addresses.is_empty();
                let Some(addr) = select_connect_peer_address(addresses, addr_type) else {
                    let err = if let Some(transport) = addr_type {
                        Error::NoMatchingAddress(pubkey, transport)
                    } else if has_known_addresses {
                        Error::NoSupportedAddress(pubkey)
                    } else {
                        Error::PeerNotFound(pubkey)
                    };
                    let _ = reply.send(Err(err.to_string()));
                    return Ok(());
                };
                if matches!(source, PeerConnectSource::Manual) {
                    state.resume_peer_auto_reconnect(pubkey);
                }
                match control.dial(addr, TargetProtocol::All).await {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
                Ok(())
            }
            PublicNetworkCommand::DisconnectPeer(pubkey, reason, reply) => {
                let session = state
                    .public
                    .peer_session_map
                    .get(&pubkey)
                    .map(|peer| peer.session_id);
                if matches!(reason, PeerDisconnectReason::Requested) {
                    state.public.peer_reconnect_backoff_attempts.remove(&pubkey);
                    state.public.requested_disconnect_peers.insert(pubkey);
                }
                if let Some(session) = session {
                    debug!(
                        "Disconnecting peer {:?} session {:?} with reason {:?}",
                        &pubkey, &session, &reason
                    );
                    state.public.control.disconnect(session).await?;
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                } else if let Some(reply) = reply {
                    let _ = reply.send(Err(format!("peer {:?} is not connected", pubkey)));
                }
                Ok(())
            }
            PublicNetworkCommand::SeedPeerReconnectBackoff(peer_id, trigger) => {
                state.seed_peer_reconnect_backoff_if_needed(&peer_id, trigger);
                Ok(())
            }
            PublicNetworkCommand::PeerReconnectBackoffTick(peer_id, attempt) => {
                let Some(pubkey) = state.fiber.peer_channel_index.get_pubkey(&peer_id) else {
                    debug_event!(
                        FiberActorRef::from_network(&myself),
                        "PeerReconnectBackoffSkippedNoDirectChannel"
                    );
                    return Ok(());
                };
                if state.public.peer_session_map.contains_key(&pubkey) {
                    state.public.peer_reconnect_backoff_attempts.remove(&pubkey);
                    return Ok(());
                }
                if state.public.requested_disconnect_peers.contains(&pubkey) {
                    state.public.peer_reconnect_backoff_attempts.remove(&pubkey);
                    debug_event!(
                        FiberActorRef::from_network(&myself),
                        "PeerReconnectBackoffSkippedRequested"
                    );
                    return Ok(());
                }
                let Some(current_attempt) = state
                    .public
                    .peer_reconnect_backoff_attempts
                    .get(&pubkey)
                    .copied()
                else {
                    return Ok(());
                };
                if current_attempt != attempt {
                    return Ok(());
                }
                debug_event!(
                    FiberActorRef::from_network(&myself),
                    "PeerReconnectBackoffAttempt"
                );
                let addresses = state.get_peer_addresses_by_pubkey(&pubkey);
                if let Some(addr) = addresses.iter().choose(&mut rand::thread_rng()) {
                    myself
                        .send_message(NetworkActorMessage::new_command(
                            PublicNetworkCommand::ConnectPeer(
                                addr.clone(),
                                false,
                                PeerConnectSource::Automatic,
                                None,
                            ),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
                let next_attempt = current_attempt.saturating_add(1);
                state
                    .public
                    .peer_reconnect_backoff_attempts
                    .insert(pubkey, next_attempt);
                state
                    .fiber
                    .schedule_peer_reconnect_backoff(peer_id, next_attempt);
                Ok(())
            }
            PublicNetworkCommand::SavePeerAddress(addr) => {
                state.enqueue_peer_address_to_save(addr);
                Ok(())
            }
            PublicNetworkCommand::RemovePendingSavePeerAddress(peer_id) => {
                state.public.pending_save_peer_addresses.remove(&peer_id);
                Ok(())
            }
            PublicNetworkCommand::MaintainConnections => {
                self.maintain_public_connections(myself, state).await;
                Ok(())
            }
            PublicNetworkCommand::CheckPeerInit(pubkey, session_id) => {
                if state
                    .public
                    .peer_session_map
                    .get(&pubkey)
                    .is_some_and(|session| {
                        session.session_id == session_id && session.features.is_none()
                    })
                {
                    state
                        .fiber
                        .network
                        .send_public_command(PublicNetworkCommand::DisconnectPeer(
                            pubkey,
                            PeerDisconnectReason::InitMessageTimeout,
                            None,
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
                Ok(())
            }
            PublicNetworkCommand::ReestablishChannels(pubkey, session_id, mut channel_ids) => {
                if !matches!(
                    state.public.peer_session_map.get(&pubkey),
                    Some(peer) if peer.session_id == session_id && peer.features.is_some()
                ) {
                    debug!(
                        peer = format!("{pubkey:?}"),
                        session = format!("{session_id:?}"),
                        "Dropping stale channel reestablishment continuation"
                    );
                    return Ok(());
                }
                if let Some(channel_id) = channel_ids.pop() {
                    if let Err(err) = state.fiber.reestablish_channel(channel_id).await {
                        error!("Failed to reestablish channel {:x}: {:?}", channel_id, err);
                    }
                }
                if !channel_ids.is_empty() {
                    myself.send_after(CHANNEL_REESTABLISH_INTERVAL, move || {
                        NetworkActorMessage::new_command(PublicNetworkCommand::ReestablishChannels(
                            pubkey,
                            session_id,
                            channel_ids,
                        ))
                    });
                }
                Ok(())
            }
            PublicNetworkCommand::BroadcastMessages(message) => {
                if let Some(gossip_actor) = state.public.gossip_actor.as_ref() {
                    gossip_actor
                        .send_message(GossipActorMessage::TryBroadcastMessages(message))
                        .expect(ASSUME_GOSSIP_ACTOR_ALIVE);
                } else {
                    debug!("Gossip actor is not available, skipping broadcast message");
                }
                Ok(())
            }
            PublicNetworkCommand::BroadcastLocalInfo(LocalInfoKind::NodeAnnouncement) => {
                if let Some(message) = state.get_or_create_new_node_announcement_message() {
                    myself
                        .send_message(NetworkActorMessage::new_command(
                            PublicNetworkCommand::BroadcastMessages(vec![
                                BroadcastMessageWithTimestamp::NodeAnnouncement(message),
                            ]),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
                Ok(())
            }
            PublicNetworkCommand::NodeInfo(_, rpc) => {
                let response = NodeInfoResponse {
                    node_name: state.public.node_name,
                    node_id: state.fiber.get_public_key(),
                    features: state.fiber.features.clone(),
                    addresses: state.public.announced_addrs.clone(),
                    chain_hash: get_chain_hash(),
                    open_channel_auto_accept_min_ckb_funding_amount: state
                        .fiber
                        .open_channel_auto_accept_min_ckb_funding_amount,
                    auto_accept_channel_ckb_funding_amount: state
                        .fiber
                        .auto_accept_channel_ckb_funding_amount,
                    tlc_expiry_delta: state.fiber.tlc_expiry_delta,
                    tlc_min_value: state.fiber.tlc_min_value,
                    tlc_fee_proportional_millionths: state.fiber.tlc_fee_proportional_millionths,
                    channel_count: state.fiber.channels.len() as u32,
                    pending_channel_count: state.fiber.pending_channels.len() as u32,
                    peers_count: state.public.peer_session_map.len() as u32,
                    udt_cfg_infos: get_udt_whitelist(),
                };
                let _ = rpc.send(Ok(response));
                Ok(())
            }
            PublicNetworkCommand::ListPeers(_, rpc) => {
                let peers = state
                    .public
                    .peer_session_map
                    .iter()
                    .map(|(pubkey, peer)| PeerInfo {
                        pubkey: *pubkey,
                        address: peer.address.clone(),
                    })
                    .collect();
                let _ = rpc.send(Ok(peers));
                Ok(())
            }
            #[cfg(not(target_arch = "wasm32"))]
            PublicNetworkCommand::SetLspService(lsp_service) => {
                state.fiber.lsp_service = Some(lsp_service);
                Ok(())
            }
            #[cfg(any(debug_assertions, feature = "bench"))]
            PublicNetworkCommand::UpdateFeatures(features) => {
                state.fiber.features = features;
                state.public.last_node_announcement_message = None;
                myself
                    .send_message(NetworkActorMessage::new_command(
                        PublicNetworkCommand::BroadcastLocalInfo(LocalInfoKind::NodeAnnouncement),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                Ok(())
            }
        }
    }

    async fn maintain_public_connections(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
    ) {
        debug!("Trying to connect to peers with mutual channels");
        for (pubkey, channel_id, channel_state) in self.core.store.get_channel_states(None) {
            if state.fiber.is_peer_available(&pubkey)
                || state.public.requested_disconnect_peers.contains(&pubkey)
            {
                continue;
            }
            let addresses = state.get_peer_addresses_by_pubkey(&pubkey);
            debug!(
                "Reconnecting channel {:x} peers {:?} in state {:?} with addresses {:?}",
                channel_id, pubkey, channel_state, addresses
            );
            if let Some(addr) = addresses.iter().choose(&mut rand::thread_rng()) {
                myself
                    .send_message(NetworkActorMessage::new_command(
                        PublicNetworkCommand::ConnectPeer(
                            addr.clone(),
                            false,
                            PeerConnectSource::Automatic,
                            None,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }

        let inbound_no_channel_peers = state.inbound_no_channel_peers_in_connected_order();
        let num_outbound_peers = state.num_of_outbound_peers();
        debug!(
            "Maintaining network connections ticked: current num inbound no-channel peers {}, current num outbound peers {}",
            inbound_no_channel_peers.len(), num_outbound_peers
        );
        if num_outbound_peers >= state.public.min_outbound_peers {
            return;
        }

        let (saved_peers, graph_peers) = {
            let graph = self.core.network_graph.read().await;
            let count = state.public.min_outbound_peers - num_outbound_peers;
            let graph_count = graph.num_of_nodes();
            let saved_count = state.public.state_to_be_persisted.num_of_saved_nodes();
            let total = graph_count + saved_count;
            if total == 0 {
                return;
            }
            let from_saved = count * saved_count / total;
            (
                state
                    .public
                    .state_to_be_persisted
                    .sample_n_peers_to_connect(from_saved),
                graph.sample_n_peers_to_connect(count - from_saved),
            )
        };

        let mut rng = rand::thread_rng();
        for (pubkey, addresses) in saved_peers.into_iter().chain(graph_peers) {
            if state.public.peer_session_map.contains_key(&pubkey)
                || state.public.requested_disconnect_peers.contains(&pubkey)
            {
                continue;
            }
            if let Some(addr) = addresses.choose(&mut rng) {
                state
                    .fiber
                    .network
                    .send_public_command(PublicNetworkCommand::ConnectPeer(
                        addr.clone(),
                        false,
                        PeerConnectSource::Automatic,
                        None,
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }
    }
}

pub struct FiberActorState<S, C> {
    store: S,
    store_actor: Option<ActorRef<StoreActorMessage>>,
    // We need to keep private key here in order to sign node announcement messages.
    private_key: Privkey,
    // This is the entropy used to generate various random values.
    // Must be kept secret.
    // TODO: Maybe we should abstract this into a separate trait.
    entropy: [u8; 32],
    // The default lock script to be used when closing a channel, may be overridden by the shutdown command.
    default_shutdown_script: Script,
    network: FiberActorRef,
    // Outbound transport capabilities required by channel/payment logic. Public peer metadata,
    // admission policy and reconnect state remain in `PublicNetworkRuntimeState`.
    p2p_peers: HashMap<Pubkey, P2pFiberPeer>,
    p2p_peer_features: HashMap<Pubkey, FeatureVector>,
    in_process_peers: HashMap<Pubkey, InProcessPeer>,
    peer_channel_index: PeerChannelIndex,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    // Channels funding lock script cache
    channels_funding_lock_script_cache: HashMap<Hash256, Script>,
    // Outpoint to channel id mapping for channels that have reached ChannelReady.
    // Keep this mapping across transient disconnects so retries / reconnect flows can still
    // resolve a channel id while the actor is offline or syncing.
    outpoint_channel_map: HashMap<OutPoint, Hash256>,
    // Channels in this hashmap are pending for acceptance. The user needs to
    // issue an AcceptChannelCommand with the amount of funding to accept the channel.
    to_be_accepted_channels: ToBeAcceptedChannels,
    // Channels in this hashmap are pending for funding transaction confirmation.
    pending_channels: HashMap<OutPoint, Hash256>,
    // Used to broadcast and query network info.
    chain_actor: ActorRef<CkbChainMessage>,
    // Used to query on-chain info.
    chain_client: C,
    // If the other party funding more than this amount, we will automatically accept the channel.
    open_channel_auto_accept_min_ckb_funding_amount: u64,
    // The default amount of CKB to be funded when auto accepting a channel.
    auto_accept_channel_ckb_funding_amount: u64,
    pending_channels_number_limit: usize,
    // The default expiry delta to forward tlcs.
    tlc_expiry_delta: u64,
    // The default tlc min and max value of tlcs to be accepted.
    tlc_min_value: u128,
    // The default tlc fee proportional millionths to be used when auto accepting a channel.
    tlc_fee_proportional_millionths: u128,
    // The features of the node, used to indicate the capabilities of the node.
    features: FeatureVector,
    channel_ephemeral_config: ChannelEphemeralConfig,

    // Inflight payment actors
    inflight_payments: HashMap<Hash256, ActorRef<PaymentActorMessage>>,
    // Final trampoline payments that still have an unresolved upstream TLC, indexed by channel.
    pending_trampoline_settlements: HashMap<Hash256, HashSet<Hash256>>,
    // Pending replies for external funding channel requests.
    // When a user requests to open a channel with external funding, we store the reply port here
    // until the peer accepts the channel and we build the unsigned funding tx.
    pending_external_funding_replies:
        HashMap<Hash256, RpcReplyPort<Result<OpenChannelWithExternalFundingResponse, String>>>,

    last_channel_ready_scan: HashMap<OutPoint, u64>,
    pending_channel_ready_retry_scans: HashSet<OutPoint>,
    // RemoveTlc commands awaiting a live ChannelActor response.
    pending_remove_tlcs: HashSet<(Hash256, u64)>,
    // Active in-flight CKB tx tracers by tx_hash. Stores actor refs so
    // send_tx can upgrade a trace-only actor with the actual transaction.
    inflight_tracers: HashMap<Hash256, ActorRef<InFlightCkbTxActorMessage>>,
    // Optional trampoline-delivery policy. This is data-plane behavior rather than
    // public P2P runtime state, and is absent for ordinary nodes and hosted tenants.
    #[cfg(not(target_arch = "wasm32"))]
    lsp_service: Option<ActorRef<LspServiceMessage>>,
    #[cfg(test)]
    test_fiber_message_hold: Option<TestFiberMessageHold>,
    #[cfg(test)]
    test_held_fiber_messages: VecDeque<FiberMessageWithTarget>,
    #[cfg(test)]
    test_trampoline_settlement_paused: bool,
}

#[derive(Debug, Clone)]
pub struct ConnectedPeer {
    pub session_id: SessionId,
    pub session_type: SessionType,
    pub address: Multiaddr,
    pub features: Option<FeatureVector>,
}

#[derive(Clone, Debug)]
struct InProcessPeer {
    actor: FiberActorRef,
    features: FeatureVector,
}

#[derive(Clone)]
struct P2pFiberPeer {
    control: ServiceAsyncControl,
    session_id: SessionId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FiberMessageDisposition {
    Processed,
    UnknownChannel,
}

/// Work that must drain before a hosted tenant runtime can be safely stopped.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HostedTenantActivity {
    pub inflight_payments: usize,
    pub active_tlcs: usize,
    pub pending_channel_operations: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl HostedTenantActivity {
    pub fn is_idle(self) -> bool {
        self.inflight_payments == 0 && self.active_tlcs == 0 && self.pending_channel_operations == 0
    }
}

pub struct NetworkActorState<S, C> {
    fiber: FiberActorState<S, C>,
    public: PublicNetworkRuntimeState,
}

impl<S, C> NetworkActorState<S, C>
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
    fn get_or_create_new_node_announcement_message(&mut self) -> Option<NodeAnnouncement> {
        if self.public.announced_addrs.is_empty() {
            debug!("Skipping node announcement because no announced address is configured");
            self.public.last_node_announcement_message = None;
            return None;
        }

        let now = now_timestamp_as_millis_u64();
        match self.public.last_node_announcement_message {
            Some(ref message) if now.saturating_sub(message.timestamp) < 3600 * 1000 => {
                debug!("Returning old node announcement message as it is still valid");
            }
            _ => {
                let announcement = NodeAnnouncement::new_signed(
                    self.public.node_name.unwrap_or_default(),
                    self.fiber.features.clone(),
                    self.public.announced_addrs.clone(),
                    &self.fiber.private_key,
                    get_chain_hash(),
                    now,
                    self.fiber.open_channel_auto_accept_min_ckb_funding_amount,
                    get_udt_whitelist(),
                    env!("CARGO_PKG_VERSION").to_string(),
                );
                debug!(
                    "Created new node announcement message: {:?}, previous {:?}",
                    &announcement, self.public.last_node_announcement_message
                );
                self.public.last_node_announcement_message = Some(announcement);
            }
        }
        self.public.last_node_announcement_message.clone()
    }

    fn session_has_channels(&self, session_id: &SessionId) -> bool {
        self.public
            .peer_session_map
            .iter()
            .find_map(|(pubkey, peer)| (peer.session_id == *session_id).then_some(pubkey))
            .is_some_and(|pubkey| self.fiber.peer_channel_index.has_channels(pubkey))
    }

    fn inbound_no_channel_peers_in_connected_order(&self) -> Vec<(Pubkey, SessionId)> {
        let mut peers = self
            .public
            .peer_session_map
            .iter()
            .filter_map(|(pubkey, peer)| {
                (peer.session_type == SessionType::Inbound
                    && !self.session_has_channels(&peer.session_id))
                .then_some((*pubkey, peer.session_id))
            })
            .collect::<Vec<_>>();
        peers.sort_by_key(|(_, session_id)| *session_id);
        peers
    }

    async fn enforce_inbound_peer_budget(&mut self) {
        let peers = self.inbound_no_channel_peers_in_connected_order();
        if peers.len() <= self.public.max_inbound_peers {
            return;
        }
        let excess = peers.len() - self.public.max_inbound_peers;
        for (pubkey, session_id) in peers.into_iter().take(excess) {
            debug!(
                "Disconnecting inbound no-channel peer {:?} on session {:?} immediately after connect",
                pubkey, session_id
            );
            match self.public.control.disconnect(session_id).await {
                Ok(()) => {
                    if matches!(
                        self.public.peer_session_map.get(&pubkey),
                        Some(peer) if peer.session_id == session_id
                    ) {
                        self.public.peer_session_map.remove(&pubkey);
                    }
                }
                Err(err) => error!(
                    "Failed to disconnect inbound no-channel peer {:?} on session {:?}: {}",
                    pubkey, session_id, err
                ),
            }
        }
    }

    fn num_of_outbound_peers(&self) -> usize {
        self.public
            .peer_session_map
            .values()
            .filter(|peer| peer.session_type == SessionType::Outbound)
            .count()
    }

    fn get_connected_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.public.peer_session_map.keys().find_map(|pubkey| {
            let peer_pubkey = super::types::pubkey_to_tentacle(*pubkey);
            (PeerId::from_public_key(&peer_pubkey) == *peer_id).then_some(*pubkey)
        })
    }

    fn get_known_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.fiber
            .peer_channel_index
            .get_pubkey(peer_id)
            .or_else(|| self.get_connected_peer_pubkey(peer_id))
    }

    fn resume_peer_auto_reconnect(&mut self, pubkey: Pubkey) {
        self.public.requested_disconnect_peers.remove(&pubkey);
        self.public.peer_reconnect_backoff_attempts.remove(&pubkey);
    }

    fn resume_peer_auto_reconnect_by_address(&mut self, address: &Multiaddr) {
        let Some(peer_id) = extract_peer_id(address) else {
            return;
        };
        if let Some(pubkey) = self.get_known_peer_pubkey(&peer_id) {
            self.resume_peer_auto_reconnect(pubkey);
        }
    }

    fn get_peer_addresses_by_pubkey(&self, pubkey: &Pubkey) -> HashSet<Multiaddr> {
        self.fiber
            .store
            .get_latest_node_announcement(pubkey)
            .map(|announcement| announcement.addresses)
            .unwrap_or_default()
            .into_iter()
            .chain(self.public.state_to_be_persisted.get_peer_addresses(pubkey))
            .collect()
    }

    fn persist_state(&self) {
        self.fiber.store.insert_network_actor_state(
            &self.fiber.get_public_key(),
            self.public.state_to_be_persisted.clone(),
        );
    }

    fn save_peer_address(&mut self, pubkey: Pubkey, address: Multiaddr) -> bool {
        if self
            .public
            .state_to_be_persisted
            .save_peer_address(pubkey, address)
        {
            self.persist_state();
            true
        } else {
            false
        }
    }

    fn enqueue_peer_address_to_save(&mut self, address: Multiaddr) {
        let Some(peer_id) = extract_peer_id(&address) else {
            error!(
                "Failed to save address to peer store: unable to extract peer id from address {:?}",
                address
            );
            return;
        };
        if let Some(pubkey) = self.get_connected_peer_pubkey(&peer_id) {
            debug!("Saved peer {:?} with address {:?}", pubkey, address);
            self.save_peer_address(pubkey, address);
            return;
        }
        let pending = self
            .public
            .pending_save_peer_addresses
            .entry(peer_id)
            .or_default();
        if !pending.contains(&address) {
            pending.push(address.clone());
            debug!(
                "Queued peer address {:?} for persistence after handshake",
                address
            );
        }
    }

    fn seed_peer_reconnect_backoff_if_needed(
        &mut self,
        peer_id: &PeerId,
        trigger: PeerReconnectTrigger,
    ) {
        if !self.public.enable_peer_reconnect_backoff {
            debug_event!(self.fiber.network, "PeerReconnectBackoffSkippedDisabled");
            return;
        }
        let Some(pubkey) = self.fiber.peer_channel_index.get_pubkey(peer_id) else {
            debug_event!(
                self.fiber.network,
                "PeerReconnectBackoffSkippedNoDirectChannel"
            );
            return;
        };
        if self.public.requested_disconnect_peers.contains(&pubkey) {
            debug_event!(self.fiber.network, "PeerReconnectBackoffSkippedRequested");
            return;
        }
        if self.public.peer_session_map.contains_key(&pubkey)
            || self
                .public
                .peer_reconnect_backoff_attempts
                .contains_key(&pubkey)
        {
            return;
        }
        self.public
            .peer_reconnect_backoff_attempts
            .insert(pubkey, 0);
        match trigger {
            PeerReconnectTrigger::Disconnected => {
                debug_event!(self.fiber.network, "PeerReconnectBackoffSeededByDisconnect");
            }
            PeerReconnectTrigger::DialError => {
                debug_event!(self.fiber.network, "PeerReconnectBackoffSeededByDialError");
            }
        }
        self.fiber
            .schedule_peer_reconnect_backoff(peer_id.clone(), 0);
    }

    async fn on_peer_connected(&mut self, remote_pubkey: Pubkey, session: &SessionContext) {
        debug!("Peer {:?} connected", remote_pubkey);
        self.fiber.p2p_peers.insert(
            remote_pubkey,
            P2pFiberPeer {
                control: self.public.control.clone(),
                session_id: session.id,
            },
        );
        self.fiber.p2p_peer_features.remove(&remote_pubkey);
        self.public.peer_session_map.insert(
            remote_pubkey,
            ConnectedPeer {
                session_id: session.id,
                session_type: session.ty,
                address: session.address.clone(),
                features: None,
            },
        );
        self.public
            .peer_reconnect_backoff_attempts
            .remove(&remote_pubkey);
        let peer_id = PeerId::from_public_key(&super::types::pubkey_to_tentacle(remote_pubkey));
        if let Some(addresses) = self.public.pending_save_peer_addresses.remove(&peer_id) {
            let mut changed = false;
            for address in addresses {
                changed |= self
                    .public
                    .state_to_be_persisted
                    .save_peer_address(remote_pubkey, address);
            }
            if changed {
                self.persist_state();
            }
        }

        self.enforce_inbound_peer_budget().await;
        if !matches!(
            self.public.peer_session_map.get(&remote_pubkey),
            Some(peer) if peer.session_id == session.id
        ) {
            self.fiber.p2p_peers.remove(&remote_pubkey);
            debug!(
                "Peer {:?} session {:?} was disconnected by inbound peer admission control",
                remote_pubkey, session.id
            );
            return;
        }
        if self.public.auto_announce {
            if let Some(message) = self.get_or_create_new_node_announcement_message() {
                debug!(
                    "Auto announcing our node to peer {:?} (message: {:?})",
                    remote_pubkey, &message
                );
                let _ = self.fiber.network.send_public_command(
                    PublicNetworkCommand::BroadcastMessages(vec![
                        BroadcastMessageWithTimestamp::NodeAnnouncement(message),
                    ]),
                );
            }
        } else {
            debug!(
                "Auto announcing is disabled, skipping node announcement to peer {:?}",
                remote_pubkey
            );
        }
        self.fiber
            .send_fiber_message(
                &remote_pubkey,
                FiberMessage::init(Init {
                    features: self.fiber.features.clone(),
                    chain_hash: get_chain_hash(),
                }),
            )
            .await
            .expect("send Init message to peer must succeed");
        let session_id = session.id;
        self.fiber
            .network
            .send_public_after(CHECK_PEER_INIT_INTERVAL, move || {
                PublicNetworkCommand::CheckPeerInit(remote_pubkey, session_id)
            });
    }

    fn on_peer_disconnected(&mut self, pubkey: Pubkey, session_id: SessionId) {
        debug!("Peer {pubkey:?} disconnected on session {session_id:?}");
        let Some(current_peer) = self.public.peer_session_map.get(&pubkey).cloned() else {
            debug!("Ignoring disconnect for peer {pubkey:?} on unknown session {session_id:?}");
            return;
        };
        if current_peer.session_id != session_id {
            debug!(
                "Ignoring stale disconnect for peer {pubkey:?}: old session {session_id:?}, current session {:?}",
                current_peer.session_id
            );
            return;
        }
        self.public.peer_session_map.remove(&pubkey);
        self.fiber.p2p_peers.remove(&pubkey);
        self.fiber.p2p_peer_features.remove(&pubkey);
        self.public
            .peer_message_policy
            .lock()
            .expect("peer message policy lock")
            .on_disconnected(&pubkey, now_timestamp_as_millis_u64());
        if self.fiber.in_process_peers.contains_key(&pubkey) {
            return;
        }
        if let Some(channel_ids) = self.fiber.peer_channel_index.get_channels(&pubkey) {
            for channel_id in channel_ids {
                if let Some(channel) = self.fiber.channels.get(&channel_id) {
                    if let Err(err) = channel
                        .send_message(ChannelActorMessage::Event(ChannelEvent::PeerDisconnected))
                    {
                        error!("Failed to send PeerDisconnected event to channel actor: {err:?}");
                    }
                }
            }
        }
        let failed_channels = self
            .fiber
            .to_be_accepted_channels
            .map
            .iter()
            .filter(|(_, (peer_pubkey, _))| *peer_pubkey == pubkey)
            .map(|(channel_id, _)| *channel_id)
            .collect::<Vec<_>>();
        for channel_id in failed_channels {
            self.fiber.store.delete_channel_open_record(&channel_id);
            self.fiber.to_be_accepted_channels.remove(&channel_id);
        }
        let peer_id = PeerId::from_public_key(&super::types::pubkey_to_tentacle(pubkey));
        if self.public.requested_disconnect_peers.contains(&pubkey) {
            debug_event!(self.fiber.network, "PeerReconnectBackoffSkippedRequested");
            return;
        }
        self.seed_peer_reconnect_backoff_if_needed(&peer_id, PeerReconnectTrigger::Disconnected);
    }

    fn record_invalid_peer_message(&self, pubkey: Pubkey) -> bool {
        self.public
            .peer_message_policy
            .lock()
            .expect("peer message policy lock")
            .record_invalid(&pubkey, now_timestamp_as_millis_u64())
    }

    async fn disconnect_peer_for_message_policy(&mut self, pubkey: Pubkey) {
        let Some(session_id) = self
            .public
            .peer_session_map
            .get(&pubkey)
            .map(|peer| peer.session_id)
        else {
            return;
        };
        warn!(
            peer = format!("{pubkey:?}"),
            session = format!("{session_id:?}"),
            "Temporarily banning peer after repeated invalid Fiber messages"
        );
        if let Err(err) = self.public.control.disconnect(session_id).await {
            error!(
                peer = format!("{pubkey:?}"),
                session = format!("{session_id:?}"),
                %err,
                "Failed to disconnect peer banned by Fiber message policy"
            );
        }
    }

    async fn on_init_msg(
        &mut self,
        myself: ActorRef<NetworkActorMessage>,
        peer_pubkey: Pubkey,
        init_msg: Init,
    ) -> ProcessingChannelResult {
        match self.public.peer_session_map.get(&peer_pubkey) {
            None => {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} is not connected",
                    peer_pubkey
                )));
            }
            Some(info) if info.features.is_some() => {
                warn!("Peer {peer_pubkey:?} sent a duplicate Init message, disconnecting");
                self.fiber
                    .network
                    .send_public_command(PublicNetworkCommand::DisconnectPeer(
                        peer_pubkey,
                        PeerDisconnectReason::DuplicateInitMessage,
                        None,
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                return Ok(());
            }
            Some(_) => {}
        }
        check_chain_hash(&init_msg.chain_hash).map_err(|error| {
            self.fiber
                .network
                .send_public_command(PublicNetworkCommand::DisconnectPeer(
                    peer_pubkey,
                    PeerDisconnectReason::ChainHashMismatch,
                    None,
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            error!(
                "chain hash mismatch with peer {:?}: {:?}, disconnect now...",
                peer_pubkey, error
            );
            ProcessingChannelError::InvalidParameter(error.to_string())
        })?;
        let info = self
            .public
            .peer_session_map
            .get_mut(&peer_pubkey)
            .expect("peer session checked above");
        self.fiber
            .p2p_peer_features
            .insert(peer_pubkey, init_msg.features.clone());
        info.features = Some(init_msg.features);
        debug_event!(FiberActorRef::from_network(&myself), "PeerInit");
        if let Some(channels) = self.fiber.peer_channel_index.get_channels(&peer_pubkey) {
            let channel_ids = channels.into_iter().collect::<Vec<_>>();
            if !channel_ids.is_empty() {
                let session_id = self
                    .public
                    .peer_session_map
                    .get(&peer_pubkey)
                    .expect("peer session checked above")
                    .session_id;
                myself
                    .send_message(NetworkActorMessage::new_command(
                        PublicNetworkCommand::ReestablishChannels(
                            peer_pubkey,
                            session_id,
                            channel_ids,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        }
        Ok(())
    }
}

pub trait NetworkActorStateStore {
    fn get_network_actor_state(&self, id: &Pubkey) -> Option<PersistentNetworkActorState>;
    fn insert_network_actor_state(&self, id: &Pubkey, state: PersistentNetworkActorState);
}

static CHANNEL_ACTOR_NAME_PREFIX: AtomicU64 = AtomicU64::new(0u64);

// ractor requires that the actor name is unique, so we add a prefix to the actor name.
fn generate_channel_actor_name(local_pubkey: &Pubkey, remote_pubkey: &Pubkey) -> String {
    format!(
        "Channel-{} {:?} <-> {:?}",
        CHANNEL_ACTOR_NAME_PREFIX.fetch_add(1, Ordering::AcqRel),
        local_pubkey,
        remote_pubkey
    )
}

impl<S, C> FiberActorState<S, C>
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
    #[cfg(not(target_arch = "wasm32"))]
    fn hosted_tenant_activity(&self) -> HostedTenantActivity {
        let active_tlcs = self
            .store
            .get_all_channel_states()
            .into_iter()
            .map(|channel| channel.tlc_state.all_tlcs().count())
            .sum();
        HostedTenantActivity {
            inflight_payments: self.inflight_payments.len(),
            active_tlcs,
            pending_channel_operations: self.pending_channels.len()
                + self.to_be_accepted_channels.map.len()
                + self.pending_external_funding_replies.len()
                + self.pending_remove_tlcs.len(),
        }
    }

    fn retry_pending_payments_for_channel(
        &self,
        myself: &FiberActorRef,
        channel_outpoint: &OutPoint,
    ) {
        for attempt in self
            .store
            .get_pending_attempts_by_channel_outpoint(channel_outpoint)
        {
            debug!(
                "Retrying payment attempt {:?} for channel {:?} reestablished",
                attempt.payment_hash, channel_outpoint
            );
            if let Err(err) = myself.send_message(FiberActorMessage::new_event(
                FiberActorEvent::RetrySendPayment(attempt.payment_hash, Some(attempt.id)),
            )) {
                debug!(
                    "Failed to register payment retry for {:?}: {:?}",
                    attempt.payment_hash, err
                );
            }
        }
    }

    pub fn get_public_key(&self) -> Pubkey {
        self.private_key.pubkey()
    }

    pub fn generate_channel_seed(&mut self) -> [u8; 32] {
        let channel_user_id = self.channels.len();
        let seed = channel_user_id
            .to_be_bytes()
            .into_iter()
            .chain(self.entropy.iter().cloned())
            .collect::<Vec<u8>>();
        let result = blake2b_hash_with_salt(&seed, b"FIBER_CHANNEL_SEED");
        self.entropy = blake2b_hash_with_salt(&result, b"FIBER_NETWORK_ENTROPY_UPDATE");
        result
    }

    fn move_channel_open_record_to_final_id(
        &self,
        temporary_channel_id: &Hash256,
        final_channel_id: Hash256,
    ) {
        let Some(mut record) = self.store.get_channel_open_record(temporary_channel_id) else {
            return;
        };

        self.store.delete_channel_open_record(temporary_channel_id);
        record.channel_id = final_channel_id;
        record.update_status(ChannelOpeningStatus::FundingTxBuilding);
        self.store.insert_channel_open_record(record);
    }

    /// Check peer's node announcement and log warnings if funding amount is insufficient for auto-accept
    fn check_and_log_peer_auto_accept_requirements(
        node_info: &super::graph::NodeInfo,
        pubkey: &Pubkey,
        funding_amount: u128,
        funding_udt_type_script: &Option<Script>,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if let Some(udt_type_script) = funding_udt_type_script.as_ref() {
            Self::log_sender_udt_funding_warning(
                node_info,
                pubkey,
                funding_amount,
                udt_type_script,
            );
        } else {
            Self::log_sender_ckb_funding_warning(node_info, pubkey, funding_amount);
        }
    }

    /// Log warning when opening channel with UDT funding amount is insufficient for peer's auto-accept
    fn log_sender_udt_funding_warning(
        node_info: &super::graph::NodeInfo,
        pubkey: &Pubkey,
        funding_amount: u128,
        udt_type_script: &Script,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if let Some(udt_cfg_info) = node_info.udt_cfg_infos.find_matching_udt(udt_type_script) {
            if let Some(auto_accept_amount) = udt_cfg_info.auto_accept_amount {
                if funding_amount < auto_accept_amount {
                    warn!(
                        "Opening channel to peer {:?} (node: {:?}) with UDT {:?} (name: {:?}) funding amount {} is less than peer's announced auto-accept minimum {}. The channel may not be auto-accepted.",
                        pubkey,
                        node_info.node_name,
                        udt_type_script,
                        udt_cfg_info.name,
                        funding_amount,
                        auto_accept_amount
                    );
                }
            } else {
                warn!(
                    "Opening channel to peer {:?} (node: {:?}) with UDT {:?} (name: {:?}). Peer has this UDT configured but auto-accept is not enabled. The channel may not be auto-accepted.",
                    pubkey, node_info.node_name, udt_type_script, udt_cfg_info.name
                );
            }
        } else {
            warn!(
                "Opening channel to peer {:?} (node: {:?}) with UDT {:?}. UDT type not found in peer's udt_cfg_infos. The channel may not be auto-accepted.",
                pubkey, node_info.node_name, udt_type_script
            );
        }
    }

    /// Log warning when opening channel with CKB funding amount is insufficient for peer's auto-accept
    fn log_sender_ckb_funding_warning(
        node_info: &super::graph::NodeInfo,
        pubkey: &Pubkey,
        funding_amount: u128,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if node_info.auto_accept_min_ckb_funding_amount == 0 {
            warn!(
                "Opening channel to peer {:?} (node: {:?}) with CKB funding amount {}. Auto-accept is disabled (auto_accept_min_ckb_funding_amount is 0). The channel may not be auto-accepted.",
                pubkey, node_info.node_name, funding_amount
            );
        } else if funding_amount < node_info.auto_accept_min_ckb_funding_amount as u128 {
            warn!(
                "Opening channel to peer {:?} (node: {:?}) with CKB funding amount {} is less than peer's announced auto-accept minimum {}. The channel may not be auto-accepted.",
                pubkey,
                node_info.node_name,
                funding_amount,
                node_info.auto_accept_min_ckb_funding_amount
            );
        }
    }

    /// Log warning when auto-accept fails for a received OpenChannel request
    fn log_receiver_auto_accept_failure(
        &self,
        pubkey: &Pubkey,
        open_channel: &OpenChannel,
        temp_channel_id: Hash256,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if let Some(udt_type_script) = open_channel.funding_udt_type_script.as_ref() {
            Self::log_receiver_udt_auto_accept_failure(
                pubkey,
                udt_type_script,
                open_channel.funding_amount,
                temp_channel_id,
            );
        } else {
            Self::log_receiver_ckb_auto_accept_failure(
                pubkey,
                open_channel.funding_amount,
                temp_channel_id,
                self.auto_accept_channel_ckb_funding_amount,
                self.open_channel_auto_accept_min_ckb_funding_amount,
            );
        }
    }

    /// Log warning when auto-accept fails for UDT channel
    fn log_receiver_udt_auto_accept_failure(
        pubkey: &Pubkey,
        udt_type_script: &Script,
        funding_amount: u128,
        temp_channel_id: Hash256,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        // Find matching UDT in local whitelist
        if let Some(udt_info) = get_udt_info(udt_type_script) {
            if let Some(auto_accept_amount) = udt_info.auto_accept_amount {
                warn!(
                    "Received OpenChannel request from peer {:?} with UDT {:?} (name: {:?}) funding amount {} is less than required auto-accept minimum {}. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                    pubkey,
                    udt_type_script,
                    udt_info.name,
                    funding_amount,
                    auto_accept_amount,
                    temp_channel_id
                );
            } else {
                warn!(
                    "Received OpenChannel request from peer {:?} with UDT {:?} (name: {:?}). Auto-accept is not enabled for this UDT. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                    pubkey, udt_type_script, udt_info.name, temp_channel_id
                );
            }
        } else {
            warn!(
                "Received OpenChannel request from peer {:?} with UDT {:?} that is not configured for auto-accept. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                pubkey, udt_type_script, temp_channel_id
            );
        }
    }

    /// Log warning when auto-accept fails for CKB channel
    fn log_receiver_ckb_auto_accept_failure(
        pubkey: &Pubkey,
        funding_amount: u128,
        temp_channel_id: Hash256,
        auto_accept_channel_ckb_funding_amount: u64,
        open_channel_auto_accept_min_ckb_funding_amount: u64,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if auto_accept_channel_ckb_funding_amount == 0 {
            warn!(
                "Received OpenChannel request from peer {:?} with CKB funding amount {}. Auto-accept is disabled (auto_accept_channel_ckb_funding_amount is 0). Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                pubkey, funding_amount, temp_channel_id
            );
        } else {
            warn!(
                "Received OpenChannel request from peer {:?} with CKB funding amount {} is less than required auto-accept minimum {}. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                pubkey,
                funding_amount,
                open_channel_auto_accept_min_ckb_funding_amount,
                temp_channel_id
            );
        }
    }

    async fn create_outbound_channel(
        &mut self,
        open_channel: OpenChannelCommand,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256), ProcessingChannelError> {
        let store = self.store.clone();
        let network = self.network.clone();
        let OpenChannelCommand {
            pubkey,
            funding_amount,
            public,
            one_way,
            shutdown_script,
            funding_udt_type_script,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            tlc_expiry_delta,
            tlc_min_value,
            tlc_fee_proportional_millionths,
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
        } = open_channel;
        let remote_pubkey = pubkey;
        self.check_feature_compatibility(&remote_pubkey)?;

        if public && one_way {
            return Err(ProcessingChannelError::InvalidParameter(
                "An one-way channel cannot be public".to_string(),
            ));
        }

        // Check peer's node announcement for auto-accept requirements
        let graph = network_graph.read().await;
        if let Some(node_info) = graph.get_node(&remote_pubkey) {
            Self::check_and_log_peer_auto_accept_requirements(
                node_info,
                &remote_pubkey,
                funding_amount,
                &funding_udt_type_script,
            );
        }
        drop(graph);

        if let Some(udt_type_script) = funding_udt_type_script.as_ref() {
            if !check_udt_script(udt_type_script) {
                return Err(ProcessingChannelError::InvalidParameter(
                    "Invalid UDT type script".to_string(),
                ));
            }
        }

        if tlc_expiry_delta.is_some_and(|d| d < MIN_TLC_EXPIRY_DELTA) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "TLC expiry delta is too small, expect larger than {}, got {}",
                MIN_TLC_EXPIRY_DELTA,
                tlc_expiry_delta.unwrap()
            )));
        }

        let tlc_expiry_delta = tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta);
        let commitment_delay_epochs = commitment_delay_epoch.map_or_else(
            || EpochNumberWithFraction::new(DEFAULT_COMMITMENT_DELAY_EPOCHS, 0, 1).full_value(),
            |epochs| epochs.full_value(),
        );
        check_tlc_delta_with_epochs(tlc_expiry_delta, commitment_delay_epochs)?;

        let shutdown_script =
            shutdown_script.unwrap_or_else(|| self.default_shutdown_script.clone());

        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            Some(generate_channel_actor_name(
                &self.get_public_key(),
                &remote_pubkey,
            )),
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                network.clone(),
                store,
                self.store_actor.clone(),
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::OpenChannel(OpenChannelParameter {
                    funding_amount,
                    seed,
                    tlc_info: ChannelTlcInfo::new(
                        tlc_min_value.unwrap_or(self.tlc_min_value),
                        tlc_expiry_delta,
                        tlc_fee_proportional_millionths
                            .unwrap_or(self.tlc_fee_proportional_millionths),
                        now_timestamp_as_millis_u64(),
                    ),
                    public_channel_info: public.then_some(PublicChannelInfo::new()),
                    is_one_way: one_way,
                    funding_udt_type_script,
                    shutdown_script,
                    channel_id_sender: tx,
                    commitment_fee_rate,
                    commitment_delay_epoch,
                    funding_fee_rate,
                    max_tlc_value_in_flight: max_tlc_value_in_flight
                        .unwrap_or(DEFAULT_MAX_TLC_VALUE_IN_FLIGHT),
                    max_tlc_number_in_flight: max_tlc_number_in_flight
                        .unwrap_or(MAX_TLC_NUMBER_IN_FLIGHT),
                }),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
        .0;
        let temp_channel_id = rx.await.expect("msg received");
        self.on_channel_created(temp_channel_id, remote_pubkey, channel.clone());

        // Record the channel opening attempt so it can be queried via RPC.
        let record = ChannelOpenRecord::new(temp_channel_id, remote_pubkey, funding_amount);
        self.store.insert_channel_open_record(record);

        Ok((channel, temp_channel_id))
    }

    /// Create an outbound channel with external funding.
    /// Similar to create_outbound_channel, but the user will sign the funding transaction
    /// with their own wallet.
    async fn create_outbound_channel_with_external_funding(
        &mut self,
        command: OpenChannelWithExternalFundingCommand,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256), ProcessingChannelError> {
        let store = self.store.clone();
        let network = self.network.clone();
        let OpenChannelWithExternalFundingCommand {
            pubkey,
            funding_amount,
            public,
            shutdown_script,
            funding_lock_script,
            funding_lock_script_cell_deps,
            funding_udt_type_script,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            tlc_expiry_delta,
            tlc_min_value,
            tlc_fee_proportional_millionths,
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
            external_channel_signer,
        } = command;

        let remote_pubkey = self.is_peer_available(&pubkey).then_some(pubkey).ok_or(
            ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?} is not connected",
                &pubkey
            )),
        )?;

        self.check_feature_compatibility(&remote_pubkey)?;

        if let Some(udt_type_script) = funding_udt_type_script.as_ref() {
            if !check_udt_script(udt_type_script) {
                return Err(ProcessingChannelError::InvalidParameter(
                    "Invalid UDT type script".to_string(),
                ));
            }
        }

        if tlc_expiry_delta.is_some_and(|d| d < MIN_TLC_EXPIRY_DELTA) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "TLC expiry delta is too small, expect larger than {}, got {}",
                MIN_TLC_EXPIRY_DELTA,
                tlc_expiry_delta.unwrap()
            )));
        }

        let tlc_expiry_delta = tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta);
        let commitment_delay_epochs = commitment_delay_epoch.map_or_else(
            || EpochNumberWithFraction::new(DEFAULT_COMMITMENT_DELAY_EPOCHS, 0, 1).full_value(),
            |epochs| epochs.full_value(),
        );
        check_tlc_delta_with_epochs(tlc_expiry_delta, commitment_delay_epochs)?;

        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            Some(generate_channel_actor_name(
                &self.get_public_key(),
                &remote_pubkey,
            )),
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                network.clone(),
                store,
                self.store_actor.clone(),
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::OpenChannelWithExternalFunding(
                    OpenChannelWithExternalFundingParameter {
                        funding_amount,
                        seed,
                        tlc_info: ChannelTlcInfo::new(
                            tlc_min_value.unwrap_or(self.tlc_min_value),
                            tlc_expiry_delta,
                            tlc_fee_proportional_millionths
                                .unwrap_or(self.tlc_fee_proportional_millionths),
                            now_timestamp_as_millis_u64(),
                        ),
                        public_channel_info: public.then_some(PublicChannelInfo::new()),
                        funding_udt_type_script,
                        shutdown_script,
                        funding_lock_script,
                        funding_lock_script_cell_deps,
                        channel_id_sender: tx,
                        commitment_fee_rate,
                        commitment_delay_epoch,
                        funding_fee_rate,
                        max_tlc_value_in_flight: max_tlc_value_in_flight
                            .unwrap_or(DEFAULT_MAX_TLC_VALUE_IN_FLIGHT),
                        max_tlc_number_in_flight: max_tlc_number_in_flight
                            .unwrap_or(MAX_TLC_NUMBER_IN_FLIGHT),
                        external_channel_signer,
                    },
                ),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
        .0;
        let temp_channel_id = rx.await.expect("msg received");
        self.on_channel_created(temp_channel_id, remote_pubkey, channel.clone());

        // Record the external-funding opening attempt under the temporary id.
        // It will be re-keyed once the peer accepts and the final channel id is known.
        let record = ChannelOpenRecord::new(temp_channel_id, remote_pubkey, funding_amount);
        self.store.insert_channel_open_record(record);

        Ok((channel, temp_channel_id))
    }

    pub async fn create_inbound_channel(
        &mut self,
        accept_channel: AcceptChannelCommand,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256, Hash256), ProcessingChannelError> {
        let store = self.store.clone();
        let AcceptChannelCommand {
            temp_channel_id,
            funding_amount,
            shutdown_script,
            max_tlc_number_in_flight,
            max_tlc_value_in_flight,
            min_tlc_value,
            tlc_fee_proportional_millionths,
            tlc_expiry_delta,
        } = accept_channel;

        let (remote_pubkey, open_channel) = self
            .to_be_accepted_channels
            .remove(&temp_channel_id)
            .ok_or(ProcessingChannelError::InvalidParameter(
            format!("No channel with temp id {:?} found", &temp_channel_id),
        ))?;

        let shutdown_script =
            shutdown_script.unwrap_or_else(|| self.default_shutdown_script.clone());
        let (funding_amount, reserved_ckb_amount) = get_funding_and_reserved_amount(
            funding_amount,
            &shutdown_script,
            &open_channel.funding_udt_type_script,
        )?;

        let network = self.network.clone();
        let id = open_channel.channel_id;
        if let Some(channel) = self.channels.get(&id) {
            warn!("A channel of id {:?} is already created, returning it", &id);
            return Ok((channel.clone(), temp_channel_id, id));
        }

        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            Some(generate_channel_actor_name(
                &self.get_public_key(),
                &remote_pubkey,
            )),
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                network.clone(),
                store,
                self.store_actor.clone(),
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::AcceptChannel(AcceptChannelParameter {
                    funding_amount,
                    reserved_ckb_amount,
                    tlc_info: ChannelTlcInfo::new(
                        min_tlc_value.unwrap_or(self.tlc_min_value),
                        tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta),
                        tlc_fee_proportional_millionths
                            .unwrap_or(self.tlc_fee_proportional_millionths),
                        now_timestamp_as_millis_u64(),
                    ),
                    public_channel_info: open_channel
                        .is_public()
                        .then_some(PublicChannelInfo::new()),
                    seed,
                    open_channel,
                    shutdown_script,
                    channel_id_sender: Some(tx),
                    max_tlc_number_in_flight: max_tlc_number_in_flight
                        .unwrap_or(MAX_TLC_NUMBER_IN_FLIGHT),
                    max_tlc_value_in_flight: max_tlc_value_in_flight.unwrap_or(u128::MAX),
                }),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
        .0;
        let new_id = rx.await.expect("msg received");
        self.on_channel_created(new_id, remote_pubkey, channel.clone());

        self.move_channel_open_record_to_final_id(&temp_channel_id, new_id);

        Ok((channel, temp_channel_id, new_id))
    }

    fn is_channel_online(&self, channel_id: &Hash256) -> bool {
        self.peer_channel_index
            .get_peer_by_channel_id(channel_id)
            .is_some_and(|peer| self.is_peer_available(&peer))
    }

    fn is_peer_available(&self, pubkey: &Pubkey) -> bool {
        self.p2p_peers.contains_key(pubkey) || self.in_process_peers.contains_key(pubkey)
    }

    fn check_pending_channel_limit(&self, peer_pubkey: Pubkey) -> ProcessingChannelResult {
        let global_limit = self.pending_channels_number_limit;
        let total_count = self.peer_channel_index.opening_channel_count()
            + self.to_be_accepted_channels.map.len();

        if total_count.saturating_add(1) > global_limit {
            return Err(ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(
                format!("Global pending channel count exceeds the limit {global_limit}"),
            ));
        }

        let peer_limit = self.to_be_accepted_channels.total_number_limit;
        let peer_count = self
            .peer_channel_index
            .opening_channel_count_by_peer(&peer_pubkey)
            + self
                .to_be_accepted_channels
                .pending_accept_count(&peer_pubkey);

        if peer_count.saturating_add(1) > peer_limit {
            return Err(ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(
                format!("Peer pending channel count exceeds the limit {peer_limit}"),
            ));
        }

        Ok(())
    }

    fn check_feature_compatibility(&self, pubkey: &Pubkey) -> ProcessingChannelResult {
        let peer_features = self
            .in_process_peers
            .get(pubkey)
            .map(|peer| &peer.features)
            .or_else(|| self.p2p_peer_features.get(pubkey));
        if let Some(peer_features) = peer_features {
            // check peer features
            if !self.features.compatible_with(peer_features) {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} features {:?} are not compatible with our features {:?}",
                    pubkey, peer_features, self.features
                )));
            }
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?}'s feature not found, waiting for peer to send Init message",
                pubkey
            )));
        }
        Ok(())
    }

    pub async fn trace_tx(
        &mut self,
        tx_hash: Hash256,
        tx_kind: InFlightCkbTxKind,
    ) -> crate::Result<()> {
        if self.inflight_tracers.contains_key(&tx_hash) {
            debug!("Skipping duplicate tracer for tx {:?}", tx_hash);
            return Ok(());
        }
        let handler = InFlightCkbTxActor {
            chain_actor: self.chain_actor.clone(),
            chain_client: self.chain_client.clone(),
            network_actor: self.network.clone(),
            tx_hash,
            tx_kind,
            confirmations: CKB_TX_TRACING_CONFIRMATIONS,
        };

        let (actor_ref, _) = Actor::spawn_linked(
            None,
            handler,
            InFlightCkbTxActorArguments { transaction: None },
            self.network.get_cell(),
        )
        .await?;
        self.inflight_tracers.insert(tx_hash, actor_ref);

        Ok(())
    }

    pub async fn send_tx(
        &mut self,
        tx: TransactionView,
        tx_kind: InFlightCkbTxKind,
    ) -> crate::Result<()> {
        let tx_hash: Hash256 = tx.hash().into();
        if let Some(existing) = self.inflight_tracers.get(&tx_hash) {
            // A trace-only actor already exists for this tx_hash.
            // Upgrade it with the actual transaction for broadcasting.
            debug!(
                "Upgrading existing tracer for tx {:?} with transaction payload",
                tx_hash
            );
            existing.send_message(InFlightCkbTxActorMessage::SendTx(tx))?;
            return Ok(());
        }
        debug!(
            "Spawning InFlightCkbTxActor: tx_hash={:?}, tx_kind={:?}, confirmations={}, inputs={}, outputs={}",
            tx_hash,
            tx_kind,
            CKB_TX_TRACING_CONFIRMATIONS,
            tx.inputs().len(),
            tx.outputs().len(),
        );

        let handler = InFlightCkbTxActor {
            chain_actor: self.chain_actor.clone(),
            chain_client: self.chain_client.clone(),
            network_actor: self.network.clone(),
            tx_hash,
            tx_kind,
            confirmations: CKB_TX_TRACING_CONFIRMATIONS,
        };

        let (actor_ref, _) = Actor::spawn_linked(
            None,
            handler,
            InFlightCkbTxActorArguments {
                transaction: Some(tx),
            },
            self.network.get_cell(),
        )
        .await?;
        self.inflight_tracers.insert(tx_hash, actor_ref);

        Ok(())
    }

    pub async fn abort_funding(&mut self, channel_id_or_outpoint: Either<Hash256, OutPoint>) {
        debug!("abort_funding called with {:?}", channel_id_or_outpoint);
        let channel_id = match channel_id_or_outpoint {
            Either::Left(channel_id) => channel_id,
            Either::Right(outpoint) => match self.pending_channels.remove(&outpoint) {
                Some(channel_id) => channel_id,
                None => {
                    warn!(
                        "Funding transaction failed for outpoint {:?} but no channel found",
                        &outpoint
                    );
                    return;
                }
            },
        };

        self.send_message_to_channel_actor(
            channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::Stop(StopReason::FundingFailed)),
        )
        .await;
    }

    pub async fn abandon_channel(&mut self, channel_id: Hash256) -> ProcessingChannelResult {
        if let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) {
            match channel_actor_state.state {
                ChannelState::ChannelReady
                | ChannelState::ShuttingDown(_)
                | ChannelState::Closed(_)
                | ChannelState::AwaitingChannelReady(_) => {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Channel {} is in state {:?}, cannot be abandoned, please shutdown the channel instead",
                        channel_id, channel_actor_state.state
                    )));
                }
                ChannelState::AwaitingTxSignatures(flags)
                    if flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT) =>
                {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Channel {} is in state {:?} and our signature has been sent. It cannot be abandoned. please wait for chain commitment.",
                        channel_id, channel_actor_state.state
                    )));
                }
                _ => {
                    if channel_actor_state.funding_tx_confirmed_at.is_some() {
                        return Err(ProcessingChannelError::InvalidParameter(format!(
                            "Channel {} funding transaction is already confirmed, please shutdown the channel instead",
                            channel_id,
                        )));
                    }
                }
            }
        }

        if let Some(channel) = self.channels.get(&channel_id) {
            if channel
                .send_message(ChannelActorMessage::Event(ChannelEvent::Stop(
                    StopReason::Abandon,
                )))
                .is_err()
            {
                return Err(ProcessingChannelError::InternalError(format!(
                    "Failed to stop channel actor {}",
                    channel_id
                )));
            }
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Channel {} not found",
                channel_id
            )));
        }
        return Ok(());
    }

    fn schedule_peer_reconnect_backoff(&self, peer_id: PeerId, attempt: u32) {
        let delay = compute_peer_reconnect_delay(attempt);
        debug_event!(self.network, "PeerReconnectBackoffScheduled");
        self.network.send_public_after(delay, move || {
            PublicNetworkCommand::PeerReconnectBackoffTick(peer_id, attempt)
        });
    }

    async fn send_fiber_message(
        &self,
        pubkey: &Pubkey,
        message: FiberMessage,
    ) -> crate::Result<()> {
        if let Some(peer) = self.in_process_peers.get(pubkey) {
            peer.actor
                .send_message(FiberActorMessage::new_event(FiberActorEvent::PeerMessage(
                    self.get_public_key(),
                    message,
                )))
                .map_err(|error| Error::InternalError(anyhow::anyhow!(error.to_string())))?;
            return Ok(());
        }
        if let Some(peer) = self.p2p_peers.get(pubkey) {
            peer.control
                .send_message_to(
                    peer.session_id,
                    FIBER_PROTOCOL_ID,
                    message.to_molecule_bytes(),
                )
                .await?;
            return Ok(());
        }
        Err(Error::PeerNotFound(*pubkey))
    }

    fn queue_retryable_remove_tlc(
        &mut self,
        channel_id: Hash256,
        remove_tlc: &RemoveTlcCommand,
    ) -> bool {
        let Some(mut state) = self.store.get_channel_actor_state(&channel_id) else {
            return false;
        };
        if !matches!(
            state.state,
            ChannelState::ChannelReady | ChannelState::ShuttingDown(_)
        ) {
            return false;
        }

        if let RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }) =
            &remove_tlc.reason
        {
            if let Some(tlc) = state.tlc_state.get(&TLCId::Received(remove_tlc.id)) {
                let payment_hash = tlc.payment_hash;
                self.store.insert_preimage(payment_hash, *payment_preimage);
                self.network
                    .send_message(FiberActorMessage::new_notification(
                        NetworkServiceEvent::PreimageCreated(payment_hash, *payment_preimage),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        }

        let operation = RetryableTlcOperation::RemoveTlc(
            TLCId::Received(remove_tlc.id),
            remove_tlc.reason.clone(),
        );
        if !state.retryable_tlc_operations.contains(&operation) {
            state.retryable_tlc_operations.push_back(operation);
        }
        self.store.insert_channel_actor_state(state);
        true
    }

    async fn send_command_to_channel(
        &mut self,
        channel_id: Hash256,
        command: ChannelCommand,
    ) -> crate::Result<()> {
        match command {
            // Need to handle the force shutdown command specially because the ChannelActor
            // may not exist when remote peer is disconnected.
            ChannelCommand::Shutdown(shutdown, rpc_reply) if shutdown.force => {
                if let Some(actor) = self.channels.get(&channel_id) {
                    actor.send_message(ChannelActorMessage::Command(ChannelCommand::Shutdown(
                        shutdown, rpc_reply,
                    )))?;
                    Ok(())
                } else {
                    match self.store.get_channel_actor_state(&channel_id) {
                        Some(mut state) => {
                            match state.state {
                                ChannelState::ChannelReady => {
                                    debug!("Handling force shutdown command in ChannelReady state");
                                }
                                ChannelState::ShuttingDown(flags) => {
                                    debug!(
                                        "Handling force shutdown command in ShuttingDown state, flags: {:?}",
                                        &flags
                                    );
                                }
                                _ => {
                                    let error = Error::ChannelError(
                                        ProcessingChannelError::InvalidState(format!(
                                            "Handling force shutdown command invalid state {:?}",
                                            &state.state
                                        )),
                                    );

                                    let _ = rpc_reply.send(Err(error.to_string()));
                                    return Err(error);
                                }
                            };

                            let transaction = match state.get_latest_commitment_transaction().await
                            {
                                Ok(tx) => tx,
                                Err(e) => {
                                    let error = Error::ChannelError(e);
                                    let _ = rpc_reply.send(Err(error.to_string()));
                                    return Err(error);
                                }
                            };

                            self.network
                                .send_message(FiberActorMessage::new_event(
                                    FiberActorEvent::ClosingTransactionPending(
                                        state.get_id(),
                                        state.get_remote_pubkey(),
                                        transaction,
                                        true,
                                    ),
                                ))
                                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                            state.update_state(ChannelState::ShuttingDown(
                                ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
                            ));
                            self.store.insert_channel_actor_state(state);
                            if let Some(ref store_actor) = self.store_actor {
                                store_actor
                                    .cast(StoreActorMessage::RequestBackup)
                                    .map_err(|e| Error::DBInternalError(e.to_string()))?;
                            }

                            let _ = rpc_reply.send(Ok(()));
                            Ok(())
                        }
                        None => {
                            let error = Error::ChannelNotFound(channel_id);
                            let _ = rpc_reply.send(Err(error.to_string()));
                            Err(error)
                        }
                    }
                }
            }
            _ => match self.channels.get(&channel_id) {
                Some(actor) => {
                    actor.send_message(ChannelActorMessage::Command(command))?;
                    Ok(())
                }
                None => {
                    // if it's relay remove tlc, insert it into ChannelActorState's retryable queue
                    if let ChannelCommand::RemoveTlc(remove_tlc, _) = &command {
                        self.queue_retryable_remove_tlc(channel_id, remove_tlc);
                    }

                    let error = Error::ChannelNotFound(channel_id);
                    if let Some(rpc_reply) = command.rpc_reply_port() {
                        let _ = rpc_reply.send(Err(error.to_string()));
                    }
                    Err(error)
                }
            },
        }
    }

    async fn reestablish_channel(
        &mut self,
        channel_id: Hash256,
    ) -> Result<ActorRef<ChannelActorMessage>, Error> {
        if let Some(actor) = self.channels.get(&channel_id).cloned() {
            debug!(
                "Channel {:x} already exists, reusing live actor for reestablishment",
                &channel_id
            );
            if let Err(err) =
                actor.send_message(ChannelActorMessage::Event(ChannelEvent::PeerReconnected))
            {
                error!("Failed to send PeerReconnected error: {err:?}");
            }
            Ok(actor)
        } else {
            Err(Error::ChannelNotFound(channel_id))
        }
    }

    async fn restore_offline_channel(
        &mut self,
        remote_pubkey: Pubkey,
        channel_id: Hash256,
    ) -> Result<ActorRef<ChannelActorMessage>, Error> {
        if let Some(actor) = self.channels.get(&channel_id).cloned() {
            return Ok(actor);
        }

        let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) else {
            return Err(Error::ChannelNotFound(channel_id));
        };
        let Some(restore_mode) = channel_actor_state.offline_restore_mode() else {
            return Err(Error::ChannelError(ProcessingChannelError::InvalidState(
                format!(
                    "Channel {:x} cannot be restored offline from state {:?}",
                    &channel_id, channel_actor_state.state
                ),
            )));
        };

        debug!("Restoring persisted channel actor {:x}", &channel_id);
        let (channel, _) = Actor::spawn_linked(
            Some(generate_channel_actor_name(
                &self.get_public_key(),
                &remote_pubkey,
            )),
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                self.network.clone(),
                self.store.clone(),
                self.store_actor.clone(),
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::RestoreOfflineChannel(channel_id),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            self.network.get_cell(),
        )
        .await?;

        if channel_actor_state
            .offline_restore_mode()
            .is_some_and(|mode| mode == OfflineChannelRestoreMode::ReestablishPeer)
        {
            self.register_channel_actor(channel_id, remote_pubkey, channel.clone());
        } else {
            debug_assert_eq!(restore_mode, OfflineChannelRestoreMode::WatchChain);
            self.channels.insert(channel_id, channel.clone());
        }
        if let Some(outpoint) = channel_actor_state.get_funding_transaction_outpoint() {
            self.outpoint_channel_map.insert(outpoint, channel_id);
        }
        if is_pending_channel_state(&channel_actor_state.state) {
            self.peer_channel_index.mark_channel_opening(channel_id);
        }
        debug!("channel {:x} restored offline successfully", &channel_id);

        Ok(channel)
    }

    async fn restore_persisted_offline_channels(&mut self) {
        for (pubkey, channel_id, _channel_state) in self.store.get_channel_states(None) {
            let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) else {
                continue;
            };
            if channel_actor_state.offline_restore_mode().is_none() {
                continue;
            }

            if let Err(err) = self.restore_offline_channel(pubkey, channel_id).await {
                error!(
                    "Failed to restore persisted channel actor {:x}: {:?}",
                    channel_id, err
                );
            }
        }
    }

    fn disconnect_in_process_peer(&mut self, pubkey: Pubkey) {
        if self.in_process_peers.remove(&pubkey).is_none() {
            return;
        }
        if self.p2p_peers.contains_key(&pubkey) {
            return;
        }
        if let Some(channel_ids) = self.peer_channel_index.get_channels(&pubkey) {
            for channel_id in channel_ids {
                if let Some(channel) = self.channels.get(&channel_id) {
                    if let Err(error) = channel
                        .send_message(ChannelActorMessage::Event(ChannelEvent::PeerDisconnected))
                    {
                        error!(
                            "Failed to disconnect in-process channel actor {:x}: {:?}",
                            channel_id, error
                        );
                    }
                }
            }
        }
    }

    fn persist_live_channels_offline_for_shutdown(&self) {
        for channel_id in self.channels.keys().copied().collect::<Vec<_>>() {
            let Some(mut channel_state) = self.store.get_channel_actor_state(&channel_id) else {
                continue;
            };

            if channel_state.is_closed() {
                continue;
            }

            channel_state.mark_reestablishing_offline();
            self.store.insert_channel_actor_state(channel_state);
        }
    }

    fn register_channel_actor(
        &mut self,
        id: Hash256,
        pubkey: Pubkey,
        actor: ActorRef<ChannelActorMessage>,
    ) {
        self.channels.insert(id, actor.clone());
        self.peer_channel_index.add_channel(pubkey, id);
    }

    fn on_channel_created(
        &mut self,
        id: Hash256,
        pubkey: Pubkey,
        actor: ActorRef<ChannelActorMessage>,
    ) {
        self.register_channel_actor(id, pubkey, actor);
        self.peer_channel_index.mark_channel_opening(id);
        debug!("Channel {:x} created", &id);
        // Notify outside observers.
        self.network
            .send_message(FiberActorMessage::new_notification(
                NetworkServiceEvent::ChannelCreated(pubkey, id),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
    }

    async fn on_closing_transaction_pending(
        &mut self,
        channel_id: Hash256,
        pubkey: Pubkey,
        transaction: TransactionView,
        force: bool,
    ) {
        let tx_hash: Byte32 = transaction.hash();
        let force_flag = if force { "forcefully" } else { "cooperatively" };
        info!(
            "Channel ({:?}) to peer {:?} is closed {:?}. Broadcasting closing transaction ({:?}) now.",
            &channel_id, &pubkey, &tx_hash, force_flag
        );
        if let Err(err) = self
            .send_tx(
                transaction,
                InFlightCkbTxKind::Closing(pubkey, channel_id, force),
            )
            .await
        {
            error!("failed to send closing tx: {}", err);
        }
    }

    async fn on_closing_transaction_confirmed(
        &mut self,
        pubkey: &Pubkey,
        channel_id: &Hash256,
        tx_hash: Byte32,
        force: bool,
        close_by_us: bool,
    ) {
        match self.channels.get(channel_id) {
            Some(channel_actor) => {
                let _ = channel_actor.send_message(ChannelActorMessage::Event(
                    ChannelEvent::ClosingTransactionConfirmed(tx_hash.unpack(), force, close_by_us),
                ));
            }
            None => {
                debug!("Channel {channel_id} actor is exit, try to update channel state");
                // channel is already exit, we should not try to reestablish channel since we
                // received a close transaction, so we just update channel actor state
                if let Some(mut state) = self.store.get_channel_actor_state(channel_id) {
                    // setup required field:
                    state.network = Some(self.network.clone());
                    state.private_key = Some(self.private_key.clone());
                    match state
                        .update_close_transaction_confirmed(tx_hash.unpack(), force, close_by_us)
                        .await
                    {
                        Ok(_) => {
                            let should_restore_onchain_settlement_actor = matches!(
                                state.state,
                                ChannelState::Closed(flags)
                                    if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
                            );
                            self.store.insert_channel_actor_state(state);
                            if should_restore_onchain_settlement_actor {
                                if let Err(err) =
                                    self.restore_offline_channel(*pubkey, *channel_id).await
                                {
                                    error!(
                                        "failed to restore on-chain settlement actor for {:?}: {:?}",
                                        channel_id, err
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            error!("failed to update_close_transaction_confirmed {err:?}");
                        }
                    }
                }
            }
        }

        self.peer_channel_index.remove_channel(pubkey, channel_id);
        if !force {
            // Notify outside observers.
            self.network
                .send_message(FiberActorMessage::new_notification(
                    NetworkServiceEvent::ChannelClosed(*pubkey, *channel_id, tx_hash.clone()),
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        }
    }

    async fn on_channel_actor_stopped(&mut self, channel_id: Hash256, reason: StopReason) {
        // all check passed, now begin to remove from memory and DB
        if let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) {
            self.peer_channel_index
                .remove_channel(&channel_actor_state.remote_pubkey, &channel_id);
        }
        self.channels.remove(&channel_id);
        self.channels_funding_lock_script_cache.remove(&channel_id);
        if let Some(reply) = self.pending_external_funding_replies.remove(&channel_id) {
            let err = format!(
                "Channel {:?} stopped before unsigned external funding tx was returned: {:?}",
                channel_id, reason
            );
            warn!("{}", err);
            let _ = reply.send(Err(err));
        }

        if reason == StopReason::Abandon || reason.is_abort_funding() {
            if let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) {
                // remove from transaction track actor
                if let Some(funding_tx) = channel_actor_state.funding_tx.as_ref() {
                    self.chain_actor
                        .send_message(CkbChainMessage::RemoveFundingTx(
                            funding_tx.calc_tx_hash().into(),
                        ))
                        .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW);
                }
                self.store.delete_channel_actor_state(&channel_id);
            }
            // notify event observers, such as remove from watchtower
            self.network
                .send_message(FiberActorMessage::new_notification(
                    if reason == StopReason::Abandon {
                        NetworkServiceEvent::ChannelAbandon(channel_id)
                    } else {
                        NetworkServiceEvent::ChannelFundingAborted(channel_id)
                    },
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        }

        self.to_be_accepted_channels.remove(&channel_id);
        if let Some((outpoint, _)) = self
            .outpoint_channel_map
            .iter()
            .find(|(_, id)| *id == &channel_id)
        {
            self.pending_channels.remove(outpoint);
            self.last_channel_ready_scan.remove(outpoint);
            self.pending_channel_ready_retry_scans.remove(outpoint);
        }
        self.outpoint_channel_map.retain(|_, id| *id != channel_id);
    }

    pub async fn on_open_channel_msg(
        &mut self,
        peer_pubkey: Pubkey,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        let id = open_channel.channel_id;
        let remote_funding_amount = open_channel.funding_amount;

        if open_channel.chain_hash != get_chain_hash() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Invalid chain hash {:?}, expected {:?}",
                open_channel.chain_hash,
                get_chain_hash()
            )));
        }

        let result = check_open_channel_parameters(
            &open_channel.funding_udt_type_script,
            &open_channel.shutdown_script,
            open_channel.reserved_ckb_amount,
            open_channel.funding_fee_rate,
            open_channel.commitment_fee_rate,
            open_channel.commitment_delay_epoch,
            open_channel.max_tlc_number_in_flight,
        )
        .and_then(|_| {
            if !self.to_be_accepted_channels.map.contains_key(&id) {
                self.check_pending_channel_limit(peer_pubkey)?;
            }
            self.to_be_accepted_channels
                .try_insert(id, peer_pubkey, open_channel)
        });

        match result {
            Ok(_) => {
                // Create a persistent record so the accepting side can see this pending channel
                // via list_channels(only_pending=true) and across node restarts.
                let record = ChannelOpenRecord::new_inbound(id, peer_pubkey, remote_funding_amount);
                self.store.insert_channel_open_record(record);

                // Notify outside observers.
                self.network
                    .send_message(FiberActorMessage::new_notification(
                        NetworkServiceEvent::ChannelPendingToBeAccepted(peer_pubkey, id),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            Err(ProcessingChannelError::RepeatedProcessing(_)) => {
                // ignore duplicated open channel request
            }
            Err(_) => {
                debug_event!(self.network, "ChannelPendingToBeRejected");
            }
        };

        result
    }

    async fn on_funding_transaction_pending(
        &mut self,
        channel_id: Hash256,
        transaction: Transaction,
        outpoint: OutPoint,
    ) {
        // Just a sanity check to ensure that no two channels are associated with the same outpoint.
        if let Some(old) = self.pending_channels.remove(&outpoint) {
            if old != channel_id {
                panic!(
                    "Trying to associate a new channel id {:?} with the same outpoint {:?} when old channel id is {:?}. Rejecting.",
                    channel_id, outpoint, old
                );
            }
        }
        self.pending_channels.insert(outpoint.clone(), channel_id);
        let transaction = transaction.into_view();
        let tx_hash: Byte32 = transaction.hash();
        debug!(
            "Funding transaction (outpoint {:?}) for channel {:?} is now ready. Broadcast it {:?} now.",
            &outpoint, &channel_id, &tx_hash
        );

        if let Err(err) = self
            .send_tx(transaction, InFlightCkbTxKind::Funding(channel_id))
            .await
        {
            error!("failed to send funding tx: {}", err);
        }
    }

    async fn on_funding_transaction_confirmed(
        &mut self,
        outpoint: OutPoint,
        block_hash: H256,
        tx_index: u32,
        timestamp: u64,
    ) {
        debug!("Funding transaction is confirmed: {:?}", &outpoint);
        let channel_id = match self.pending_channels.remove(&outpoint) {
            Some(channel_id) => channel_id,
            None => {
                warn!(
                    "Funding transaction confirmed for outpoint {:?} but no channel found",
                    &outpoint
                );
                return;
            }
        };
        self.send_message_to_channel_actor(
            channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::FundingTransactionConfirmed(
                block_hash, tx_index, timestamp,
            )),
        )
        .await;
    }

    async fn on_payment_actor_stopped(
        &mut self,
        payment_hash: Hash256,
        last_error_packet: Option<TlcErrPacket>,
    ) {
        debug!("Payment actor stopped {payment_hash}");
        if self.inflight_payments.remove(&payment_hash).is_none() {
            error!("Can't find inflight payment actor");
        }
        // If this payment has associated previous TLCs,
        // meaning it's a trampoline forwarding payment,
        // we need to resolve those upstream TLCs based on the payment outcome.
        let Some(session) = self.store.get_payment_session(payment_hash) else {
            return;
        };

        #[cfg(test)]
        if self.test_trampoline_settlement_paused
            && session.status.is_final()
            && session.request.trampoline_context.is_some()
        {
            debug!("Test paused upstream trampoline settlement for {payment_hash}");
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        if session.status.is_final() {
            if let Some(lsp_service) = self.lsp_service.as_ref() {
                let ready = ractor::call_t!(
                    lsp_service,
                    |reply| LspServiceMessage::PaymentOutcomeReady {
                        payment_hash,
                        payment_status: session.status,
                        failure: session.last_error.clone(),
                        failure_code: session.last_error_code,
                        reply,
                    },
                    5_000
                );
                match ready {
                    Ok(Ok(LspPaymentOutcomeDecision::SettleUpstream)) => {}
                    Ok(Ok(LspPaymentOutcomeDecision::RetryDelivery)) => {
                        return;
                    }
                    ready => {
                        warn!(
                            %payment_hash,
                            ?ready,
                            "Failed to persist hosted payment outcome before upstream settlement"
                        );
                        return;
                    }
                }
            }
        }
        let settlement = self
            .settle_trampoline_payment(&session, last_error_packet.as_ref(), None)
            .await;
        if let Err(error) = &settlement {
            warn!(%payment_hash, %error, "Failed to settle upstream trampoline payment");
        }
        #[cfg(not(target_arch = "wasm32"))]
        if session.status.is_final() && settlement.is_ok() {
            if let Some(lsp_service) = self.lsp_service.as_ref() {
                let _ = lsp_service.send_message(LspServiceMessage::PaymentOutcomeSettled {
                    payment_hash,
                    payment_status: session.status,
                    failure: session.last_error.clone(),
                });
            }
        }
    }

    async fn settle_trampoline_payment(
        &mut self,
        session: &PaymentSession,
        last_error_packet: Option<&TlcErrPacket>,
        channel_filter: Option<Hash256>,
    ) -> Result<(), String> {
        let Some(context) = session.request.trampoline_context.as_ref() else {
            return Ok(());
        };
        let payment_hash = session.request.payment_hash;
        let success_preimage = if session.status == PaymentStatus::Success {
            session
                .attempts()
                .find(|attempt| attempt.is_success())
                .and_then(|attempt| attempt.preimage)
                .or_else(|| self.store.get_preimage(&payment_hash))
        } else {
            None
        };
        if session.status == PaymentStatus::Success {
            let Some(preimage) = success_preimage else {
                return Err(format!(
                    "payment success but no preimage found for {payment_hash}"
                ));
            };
            self.store.insert_preimage(payment_hash, preimage);
        } else if !session.status.is_final() {
            return Err(format!(
                "trampoline payment {payment_hash} has non-final status {:?}",
                session.status
            ));
        }

        let mut settlement_errors = Vec::new();
        for previous_tlc in &context.previous_tlcs {
            if channel_filter.is_some_and(|channel_id| channel_id != previous_tlc.prev_channel_id)
                || !self
                    .store
                    .get_channel_actor_state(&previous_tlc.prev_channel_id)
                    .is_some_and(|state| {
                        trampoline_upstream_tlc_needs_settlement(&state, payment_hash, previous_tlc)
                    })
            {
                continue;
            }

            let reason = match session.status {
                PaymentStatus::Success => RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                    payment_preimage: success_preimage.expect("success preimage checked above"),
                }),
                PaymentStatus::Failed => {
                    let Some(shared_secret) = previous_tlc.shared_secret else {
                        settlement_errors.push(format!(
                            "cannot fail upstream trampoline TLC without shared secret: payment_hash={:?}, channel_id={:?}, tlc_id={:?}",
                            payment_hash,
                            previous_tlc.prev_channel_id,
                            previous_tlc.prev_tlc_id
                        ));
                        continue;
                    };
                    let error_code = session
                        .last_error_code
                        .unwrap_or(TlcErrorCode::TemporaryNodeFailure);
                    let inner_error_packet = last_error_packet
                        .map(|packet| packet.onion_packet.clone())
                        .unwrap_or_else(|| {
                            TlcErrPacket::new(TlcErr::new(error_code), &shared_secret).onion_packet
                        });
                    RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new_trampoline_failed(
                        error_code,
                        self.get_public_key(),
                        inner_error_packet,
                        &shared_secret,
                    ))
                }
                PaymentStatus::Created | PaymentStatus::Inflight => unreachable!(),
            };
            let (send, _recv) = oneshot::channel();
            let command = ChannelCommand::RemoveTlc(
                RemoveTlcCommand {
                    id: previous_tlc.prev_tlc_id,
                    reason,
                },
                RpcReplyPort::from(send),
            );
            if let Err(error) = self
                .send_command_to_channel(previous_tlc.prev_channel_id, command)
                .await
            {
                settlement_errors.push(format!(
                    "failed to settle upstream trampoline TLC: payment_hash={:?}, channel_id={:?}, tlc_id={:?}, error={:?}",
                    payment_hash,
                    previous_tlc.prev_channel_id,
                    previous_tlc.prev_tlc_id,
                    error
                ));
            }
        }
        if settlement_errors.is_empty() {
            Ok(())
        } else {
            Err(settlement_errors.join("; "))
        }
    }

    async fn recover_trampoline_settlements_for_channel(&mut self, channel_id: Hash256) {
        let Some(payment_hashes) = self.pending_trampoline_settlements.remove(&channel_id) else {
            return;
        };
        for payment_hash in payment_hashes {
            let Some(session) = self.store.get_payment_session(payment_hash) else {
                continue;
            };
            if session.status.is_final() {
                if let Err(error) = self
                    .settle_trampoline_payment(&session, None, Some(channel_id))
                    .await
                {
                    warn!(
                        %payment_hash,
                        %channel_id,
                        %error,
                        "Failed to recover upstream trampoline settlement"
                    );
                    self.pending_trampoline_settlements
                        .entry(channel_id)
                        .or_default()
                        .insert(payment_hash);
                }
            }
        }
    }

    async fn send_message_to_channel_actor(
        &mut self,
        channel_id: Hash256,
        // Sometimes we need to know the remote pubkey in order to send the message to the channel actor.
        peer_pubkey: Option<Pubkey>,
        message: ChannelActorMessage,
    ) {
        match self.channels.get(&channel_id) {
            None => match (message, peer_pubkey) {
                (
                    ChannelActorMessage::PeerMessage(FiberChannelMessage::ReestablishChannel(r)),
                    Some(remote_pubkey),
                ) if self.store.get_channel_actor_state(&channel_id).is_some() => {
                    debug!(
                        "Received a ReestablishChannel message for channel {:?} which has persisted state, but no corresponding channel actor, starting it now",
                        &channel_id
                    );
                    match self
                        .restore_offline_channel(remote_pubkey, channel_id)
                        .await
                    {
                        Ok(actor) => {
                            actor
                                .send_message(ChannelActorMessage::PeerMessage(
                                    FiberChannelMessage::ReestablishChannel(r),
                                ))
                                .expect("channel actor alive");
                        }
                        Err(e) => {
                            error!("Failed to reestablish channel {:x}: {:?}", &channel_id, &e);
                        }
                    }
                }
                (message, _) => {
                    debug!(
                        "Failed to send message to channel actor: channel {:?} not found, message: {:?}",
                        &channel_id, &message,
                    );
                }
            },
            Some(actor) => {
                // There is a possibility that the channel actor is not alive, but we assume it is
                // alive for this moment. For example, in force shutdown case, the ChannelActor may
                // have already finished its on-chain settlement cleanup and stopped, while
                // NetworkActor has not yet processed ChannelActorStopped and removed it from
                // `channels`. We may still try to send another event message to that stopped actor.
                //
                // In short, it's safer to ignore sending message failure from NetworkActor
                // to ChannelActor, since NetworkActor is responsible for multiple channels and a lot of stuff.
                let _ = actor.send_message(message);
            }
        }
    }

    fn get_cached_channel_funding_lock_script(
        &mut self,
        channel_id: Hash256,
        state: &ChannelActorState,
    ) -> Script {
        if self.channels.contains_key(&channel_id) {
            self.channels_funding_lock_script_cache
                .entry(channel_id)
                .or_insert_with(|| state.get_funding_lock_script())
                .to_owned()
        } else {
            // To prevent potential memory leak, we do not cache this branch
            tracing::warn!("Get funding lock script for unknown channel {channel_id:?}");
            state.get_funding_lock_script()
        }
    }
}

pub struct NetworkActorStartArguments {
    pub config: FiberConfig,
    pub tracker: TaskTracker,
    pub default_shutdown_script: Script,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct HostedTenantActorStartArguments {
    pub config: FiberConfig,
    pub default_shutdown_script: Script,
}

/// A local-only Fiber data-plane actor for one hosted tenant.
///
/// It drives the shared data-plane core directly, without constructing
/// or wrapping a public `NetworkActor`. Its only available peer is the public
/// LSP node through an in-process transport.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct HostedTenantActor<S, C> {
    core: FiberActorCore<S, C>,
}

#[cfg(not(target_arch = "wasm32"))]
impl<S, C> HostedTenantActor<S, C>
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
    pub(crate) fn new(core: FiberActorCore<S, C>) -> Self {
        Self { core }
    }

    async fn build_state(
        &self,
        myself: ActorRef<FiberActorMessage>,
        args: HostedTenantActorStartArguments,
    ) -> Result<FiberActorState<S, C>, ActorProcessingErr> {
        let HostedTenantActorStartArguments {
            config,
            default_shutdown_script,
        } = args;
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate hosted tenant secret key");
        let private_key: Privkey = <[u8; 32]>::try_from(kp.as_ref())
            .expect("valid length for hosted tenant key")
            .into();
        let mut entropy_rand = [0u8; 32];
        getrandom::fill(&mut entropy_rand).expect("getrandom fill should not fail");
        let entropy = blake2b_hash_with_salt(
            [kp.as_ref(), entropy_rand.as_slice()].concat().as_slice(),
            b"FIBER_NETWORK_ENTROPY",
        );
        let state = self.core.build_actor_state(
            &config,
            FiberActorStateArgs {
                private_key,
                entropy,
                default_shutdown_script,
                network: FiberActorRef::from_fiber(&myself),
                peer_channel_index: PeerChannelIndex::build(&self.core.store),
                features: config.gen_node_features(),
            },
        );
        Ok(state)
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S, C> Actor for HostedTenantActor<S, C>
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
    type Msg = FiberActorMessage;
    type State = FiberActorState<S, C>;
    type Arguments = HostedTenantActorStartArguments;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        self.build_state(myself, args).await
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.restore_persisted_offline_channels().await;
        myself.send_interval(CHECK_CHANNELS_INTERVAL, || {
            FiberActorMessage::new_command(FiberActorCommand::CheckChannels)
        });
        myself.send_interval(CHECK_CHANNELS_SHUTDOWN_INTERVAL, || {
            FiberActorMessage::new_command(FiberActorCommand::CheckChannelsShutdown)
        });
        self.core
            .retry_hold_tlc_sets(&FiberActorRef::from_fiber(&myself));
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            FiberActorMessage::Event(event) => {
                if let Err(error) = self
                    .core
                    .handle_event(FiberActorRef::from_fiber(&myself), state, event)
                    .await
                {
                    error!("Failed to handle hosted tenant event: {error}");
                }
            }
            FiberActorMessage::Command(command) => {
                if let Err(error) = self
                    .core
                    .handle_command(FiberActorRef::from_fiber(&myself), state, command)
                    .await
                {
                    error!("Failed to handle hosted tenant command: {error}");
                }
            }
            FiberActorMessage::Notification(event) => {
                if let Err(error) = self.core.event_sender.send(event).await {
                    error!("Failed to notify hosted tenant observer: {error}");
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.persist_live_channels_offline_for_shutdown();
        myself
            .get_cell()
            .stop_children_and_wait(Some("Hosted tenant actor stopped".to_string()), None)
            .await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl<S, C> Actor for NetworkActor<S, C>
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
    type Msg = NetworkActorMessage;
    type State = NetworkActorState<S, C>;
    type Arguments = NetworkActorStartArguments;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let NetworkActorStartArguments {
            config,
            #[cfg(not(target_arch = "wasm32"))]
            tracker,
            default_shutdown_script,
            ..
        } = args;
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let private_key: Privkey = <[u8; 32]>::try_from(kp.as_ref())
            .expect("valid length for key")
            .into();
        let mut entropy_rand = [0u8; 32];
        getrandom::fill(&mut entropy_rand).expect("getrandom fill should not fail");
        let entropy = blake2b_hash_with_salt(
            [kp.as_ref(), entropy_rand.as_slice()].concat().as_slice(),
            b"FIBER_NETWORK_ENTROPY",
        );
        let secio_kp = SecioKeyPair::from(kp);
        let secio_pk = secio_kp.public_key();
        let my_peer_id: PeerId = PeerId::from(secio_pk);
        let peer_message_policy = Arc::new(StdMutex::new(PeerMessagePolicy::new()));
        let handle = NetworkServiceHandle::new(myself.clone(), peer_message_policy.clone());
        let fiber_handle = FiberProtocolHandle::from(&handle);
        let peer_channel_index = PeerChannelIndex::build(&self.core.store);

        // Conditionally start GossipService based on sync_network_graph config
        let (gossip_actor, gossip_handle_opt) = if config.sync_network_graph() {
            let mut gossip_config = GossipConfig::from(&config);
            gossip_config.pubkey = Some(private_key.pubkey());
            let (gossip_service, gossip_handle) = GossipService::start(
                gossip_config,
                self.core.store.clone(),
                self.core.chain_actor.clone(),
                self.core.chain_client.clone(),
                Some(myself.clone()),
                peer_channel_index.clone(),
                myself.get_cell(),
            )
            .await;

            let graph_subscribing_cursor = get_latest_startup_broadcast_message_cursor(
                &self.core.store,
                Some(&private_key.pubkey()),
            )
            .go_back_for_some_time(MAX_GRAPH_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT);

            gossip_service
                .get_subscriber()
                .subscribe(graph_subscribing_cursor, myself.clone(), |m| {
                    Some(NetworkActorMessage::new_event(
                        PublicNetworkEvent::GossipMessageUpdates(m),
                    ))
                })
                .await
                .expect("subscribe to gossip store updates");
            (Some(gossip_handle.actor().clone()), Some(gossip_handle))
        } else {
            info!("Gossip network synchronization is disabled (sync_network_graph = false)");
            (None, None)
        };

        // Build service with or without gossip protocol based on configuration
        #[cfg(not(target_arch = "wasm32"))]
        let mut service = {
            let mut builder = ServiceBuilder::default()
                .insert_protocol(fiber_handle.create_meta())
                .handshake_type(secio_kp.into());
            if let Some(gossip_handle) = gossip_handle_opt {
                builder = builder.insert_protocol(gossip_handle.create_meta());
            }

            // Set SOCKS5 proxy config
            if let Some(proxy_url) = &config.proxy.proxy_url {
                match super::proxy::check_proxy_url(proxy_url) {
                    Ok(()) => {
                        builder = builder
                            .tcp_proxy_config(proxy_url)
                            .tcp_proxy_random_auth(config.proxy.proxy_random_auth);
                        info!(
                            "Set tcp_proxy_config: {:?}, proxy_random_auth: {}",
                            proxy_url, config.proxy.proxy_random_auth
                        );
                    }
                    Err(err) => {
                        error!(
                            "Invalid proxy_url in config, skipping tcp_proxy_config. proxy_url={:?}, error={}",
                            proxy_url, err
                        );
                    }
                }
            }

            // Set onion proxy config (for .onion address connections via Tor SOCKS5)
            let onion_proxy_url = config.onion.onion_server.clone().map(|s| {
                if s.starts_with("socks5://") {
                    s
                } else {
                    format!("socks5://{}", s)
                }
            });
            if let Some(ref onion_proxy_url) = onion_proxy_url {
                use crate::fiber::proxy::check_proxy_url;

                check_proxy_url(onion_proxy_url)
                    .map_err(|e| anyhow::anyhow!("Invalid onion proxy url: {}", e))?;

                info!("Set tcp_onion_config: {:?}", onion_proxy_url);
                builder = builder.tcp_onion_config(onion_proxy_url);
            }

            builder.build(handle)
        };
        #[cfg(target_arch = "wasm32")]
        let mut service = {
            let mut builder = ServiceBuilder::default()
                .insert_protocol(fiber_handle.create_meta())
                .handshake_type(secio_kp.into())
                // Sets forever to true so the network service won't be shutdown due to no incoming connections
                .forever(true);
            if let Some(gossip_handle) = gossip_handle_opt {
                builder = builder.insert_protocol(gossip_handle.create_meta());
            }
            builder.build(handle)
        };

        let mut announced_addrs = Vec::with_capacity(config.announced_addrs.len() + 1);

        #[cfg(not(target_arch = "wasm32"))]
        let listening_addr = {
            let mut addresses_to_listen = vec![MultiAddr::from_str(config.listening_addr())
                .expect("valid tentacle listening address")];
            if config.reuse_port_for_websocket {
                // Re-use the same port for websocket
                let ws_listens = addresses_to_listen
                    .iter()
                    .cloned()
                    .filter_map(|mut addr| {
                        if matches!(find_type(&addr), TransportType::Tcp) {
                            addr.push(Protocol::Ws);
                            Some(addr)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                addresses_to_listen.extend(ws_listens);
            }
            let mut listening_addr = vec![];
            for addr in addresses_to_listen.into_iter() {
                let mut current_addr = service.listen(addr).await.expect("listen tentacle");

                current_addr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
                if config.announce_listening_addr() {
                    announced_addrs.push(current_addr.clone());
                }
                listening_addr.push(current_addr);
            }

            listening_addr
        };
        #[cfg(target_arch = "wasm32")]
        // There is no listening_addr on wasm, since it can't listen to anything
        let listening_addr = vec![];
        for announced_addr in &config.announced_addrs {
            let mut multiaddr =
                MultiAddr::from_str(announced_addr.as_str()).expect("valid announced listen addr");
            match multiaddr.pop() {
                Some(Protocol::P2P(c)) if c.as_ref() != my_peer_id.as_bytes() => {
                    panic!(
                        "Announced listen addr is using invalid peer id: announced addr {}, actual peer id {:?}",
                        announced_addr, my_peer_id
                    );
                }
                Some(Protocol::P2P(_)) => {
                    // Peer id matches, continue.
                }
                Some(component) => {
                    // Push this unrecognized component back to the multiaddr.
                    multiaddr.push(component);
                }
                None => {
                    // Should never happen
                }
            }
            // Push our peer id to the multiaddr.
            multiaddr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
            announced_addrs.push(multiaddr);
        }

        if !config.announce_private_addr.unwrap_or_default() {
            announced_addrs.retain(crate::utils::is_addr_reachable);
        }

        // Start Tor onion hidden service if configured
        #[cfg(not(target_arch = "wasm32"))]
        let onion_service_token = if config.onion.listen_on_onion {
            match self
                .core
                .start_onion_service(
                    &config,
                    &listening_addr,
                    &my_peer_id,
                    &tracker,
                    myself.clone(),
                )
                .await
            {
                Ok(Some((addr, token))) => {
                    info!("Onion service address: {}", addr);
                    announced_addrs.push(addr);
                    Some(token)
                }
                Ok(None) => {
                    info!("Onion service not started: missing onion_server or proxy_url");
                    None
                }
                Err(err) => {
                    error!("Failed to start onion service: {}", err);
                    None
                }
            }
        } else {
            None
        };

        #[cfg(not(target_arch = "wasm32"))]
        info!(
            "Started listening tentacle on {:?}, peer id {:?}, announced addresses {:?}",
            &listening_addr, &my_peer_id, &announced_addrs
        );

        #[cfg(target_arch = "wasm32")]
        info!(
            "Started fiber network service peer id {:?}, announced addresses {:?}",
            &my_peer_id, &announced_addrs
        );
        let control = service.control().to_owned();
        myself
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::NetworkStarted(
                    private_key.pubkey(),
                    listening_addr.clone(),
                    announced_addrs.clone(),
                ),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);

        #[cfg(not(target_arch = "wasm32"))]
        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service stopped");
        });
        #[cfg(target_arch = "wasm32")]
        ractor::concurrency::spawn(async move {
            service.run().await;
            debug!("Tentacle service stopped");
        });
        let features = config.gen_node_features();
        let state_to_be_persisted = self
            .core
            .store
            .get_network_actor_state(&private_key.pubkey())
            .unwrap_or_default();
        let fiber = self.core.build_actor_state(
            &config,
            FiberActorStateArgs {
                private_key,
                entropy,
                default_shutdown_script,
                network: FiberActorRef::from_network(&myself),
                peer_channel_index,
                features,
            },
        );
        let public = PublicNetworkRuntimeState {
            state_to_be_persisted,
            node_name: config.announced_node_name,
            announced_addrs,
            auto_announce: config.auto_announce_node(),
            last_node_announcement_message: None,
            control,
            peer_message_policy,
            #[cfg(not(target_arch = "wasm32"))]
            onion_service_token,
            peer_session_map: Default::default(),
            pending_save_peer_addresses: Default::default(),
            gossip_actor,
            max_inbound_peers: config.max_inbound_peers(),
            min_outbound_peers: config.min_outbound_peers(),
            enable_peer_reconnect_backoff: config.enable_peer_reconnect_backoff(),
            peer_reconnect_backoff_attempts: Default::default(),
            requested_disconnect_peers: Default::default(),
        };
        let mut state = NetworkActorState { fiber, public };

        if let Some(node_announcement) = state.get_or_create_new_node_announcement_message() {
            let mut graph = self.core.network_graph.write().await;
            graph.process_node_announcement(node_announcement);
        }
        let announce_node_interval_seconds = config.announce_node_interval_seconds();
        if announce_node_interval_seconds > 0 {
            myself.send_interval(Duration::from_secs(announce_node_interval_seconds), || {
                NetworkActorMessage::new_command(PublicNetworkCommand::BroadcastLocalInfo(
                    LocalInfoKind::NodeAnnouncement,
                ))
            });
        }

        // Persist initial network actor state.
        state.persist_state();

        for bootnode in &config.bootnode_addrs {
            match Multiaddr::from_str(bootnode.as_str()) {
                Ok(addr) => {
                    myself
                        .send_message(NetworkActorMessage::new_command(
                            PublicNetworkCommand::ConnectPeer(
                                addr,
                                false,
                                PeerConnectSource::Automatic,
                                None,
                            ),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
                Err(err) => {
                    error!("Failed to parse bootnode address {:?}: {}", bootnode, err);
                }
            }
        }

        Ok(state)
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.fiber.restore_persisted_offline_channels().await;

        // MAINTAINING_CONNECTIONS_INTERVAL is long, we need to trigger when start
        myself
            .send_message(NetworkActorMessage::new_command(
                PublicNetworkCommand::MaintainConnections,
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        myself.send_interval(MAINTAINING_CONNECTIONS_INTERVAL, || {
            NetworkActorMessage::new_command(PublicNetworkCommand::MaintainConnections)
        });
        myself.send_interval(CHECK_CHANNELS_INTERVAL, || {
            NetworkActorMessage::new_command(FiberActorCommand::CheckChannels)
        });
        myself.send_interval(CHECK_CHANNELS_SHUTDOWN_INTERVAL, || {
            NetworkActorMessage::new_command(FiberActorCommand::CheckChannelsShutdown)
        });

        // Trigger hold tlc fulfill retry and timeout checks at startup.
        self.core
            .retry_hold_tlc_sets(&FiberActorRef::from_network(&myself));
        debug_event!(
            FiberActorRef::from_network(&myself),
            "network actor started"
        );
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "NetworkActor",
            message.to_string(),
            "fiber.network_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            NetworkActorMessage::PublicEvent(event) => {
                if let Err(err) = self.handle_public_event(myself, state, event).await {
                    error!("Failed to handle fiber network event: {}", err);
                }
            }
            NetworkActorMessage::PublicCommand(command) => {
                if let Err(err) = self.handle_public_command(myself, state, command).await {
                    error!("Failed to handle fiber network command: {}", err);
                }
            }
            NetworkActorMessage::Fiber(FiberActorMessage::Command(command)) => {
                if let Err(err) = self
                    .core
                    .handle_command(
                        FiberActorRef::from_network(&myself),
                        &mut state.fiber,
                        command,
                    )
                    .await
                {
                    error!("Failed to handle Fiber core command: {}", err);
                }
            }
            NetworkActorMessage::Fiber(FiberActorMessage::Event(event)) => {
                if let Err(err) = self
                    .core
                    .handle_event(
                        FiberActorRef::from_network(&myself),
                        &mut state.fiber,
                        event,
                    )
                    .await
                {
                    error!("Failed to handle Fiber core event: {}", err);
                }
            }
            NetworkActorMessage::Fiber(FiberActorMessage::Notification(event)) => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(lsp_service) = state.fiber.lsp_service.as_ref() {
                    match &event {
                        NetworkServiceEvent::ChannelReady(pubkey, channel_id, ..)
                        | NetworkServiceEvent::ChannelOnline(pubkey, channel_id, ..) => {
                            let _ = lsp_service.send_message(
                                LspServiceMessage::TenantChannelOnline(*pubkey, *channel_id),
                            );
                        }
                        NetworkServiceEvent::ChannelOffline(pubkey, channel_id, ..) => {
                            let _ = lsp_service.send_message(
                                LspServiceMessage::TenantChannelOffline(*pubkey, *channel_id),
                            );
                        }
                        _ => {}
                    }
                }
                if let Err(err) = self.core.event_sender.send(event).await {
                    error!("Failed to notify outside observers: {}", err);
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.fiber.persist_live_channels_offline_for_shutdown();

        // Cancel the onion service background task if running
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(token) = state.public.onion_service_token.take() {
            debug!("Cancelling onion service...");
            token.cancel();
        }
        myself
            .get_cell()
            .stop_children_and_wait(Some("Network actor stopped".to_string()), None)
            .await;

        if let Err(err) = state.public.control.close().await {
            error!("Failed to close tentacle service: {}", err);
        }
        let local_pubkey = state.fiber.get_public_key();
        debug!("Saving network actor state for {:?}", local_pubkey);
        state.persist_state();
        debug!("Network service for {:?} shutdown", local_pubkey);
        // The event receiver may have been closed already.
        // We ignore the error here.
        let _ = self
            .core
            .event_sender
            .send(NetworkServiceEvent::NetworkStopped(local_pubkey))
            .await;
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(who, _state, reason) => {
                debug!("Actor {:?} terminated with reason {:?}", who, reason);
            }
            SupervisionEvent::ActorFailed(who, err) => {
                log_actor_failed(who, err);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct FiberProtocolHandle {
    actor: ActorRef<NetworkActorMessage>,
    peer_message_policy: Arc<StdMutex<PeerMessagePolicy>>,
}

impl FiberProtocolHandle {
    fn create_meta(self) -> ProtocolMeta {
        MetaBuilder::new()
            .id(FIBER_PROTOCOL_ID)
            .codec(move || {
                Box::new(
                    length_delimited::Builder::new()
                        .max_frame_length(MAX_SERVICE_PROTOCOAL_DATA_SIZE)
                        .new_codec(),
                )
            })
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }
}

#[async_trait]
impl ServiceProtocol for FiberProtocolHandle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, _version: &str) {
        let _session = context.session;
        if let Some(remote_pubkey) = context.session.remote_pubkey.clone() {
            let pubkey = super::types::pubkey_from_tentacle(remote_pubkey);
            let banned = self
                .peer_message_policy
                .lock()
                .expect("peer message policy lock")
                .is_banned(&pubkey, now_timestamp_as_millis_u64());
            if banned {
                debug!(
                    peer = format!("{pubkey:?}"),
                    session = format!("{:?}", context.session.id),
                    "Disconnecting peer while its Fiber message ban is active"
                );
                if let Err(err) = context.disconnect(context.session.id).await {
                    error!(
                        peer = format!("{pubkey:?}"),
                        %err,
                        "Failed to disconnect peer with active Fiber message ban"
                    );
                }
                return;
            }
            try_send_actor_message(
                &self.actor,
                NetworkActorMessage::new_event(PublicNetworkEvent::PeerConnected(
                    pubkey,
                    context.session.clone(),
                )),
            );
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        match context.session.remote_pubkey.as_ref() {
            Some(pubkey) => {
                try_send_actor_message(
                    &self.actor,
                    NetworkActorMessage::new_event(PublicNetworkEvent::PeerDisconnected(
                        super::types::pubkey_from_tentacle(pubkey.clone()),
                        context.session.clone(),
                    )),
                );
            }
            None => {
                debug!(
                    "Peer disconnected without remote pubkey {:?}",
                    context.session
                );
            }
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        let Some(remote_pubkey) = context.session.remote_pubkey.clone() else {
            debug!(
                "Received message without remote pubkey {:?}",
                context.session
            );
            return;
        };
        let pubkey = super::types::pubkey_from_tentacle(remote_pubkey);
        let now_ms = now_timestamp_as_millis_u64();
        let admission = admit_inbound_fiber_message(
            &self.peer_message_policy,
            &pubkey,
            data.len() as u64,
            now_ms,
        );
        let permit = match admission {
            InboundFiberAdmission::Admitted(permit) => permit,
            InboundFiberAdmission::Disconnect => {
                debug!(
                    peer = format!("{pubkey:?}"),
                    "Disconnecting Fiber peer after ingress admission overflow"
                );
                if let Err(err) = context.disconnect(context.session.id).await {
                    error!(
                        peer = format!("{pubkey:?}"),
                        %err,
                        "Failed to disconnect Fiber peer after ingress admission overflow"
                    );
                }
                return;
            }
            InboundFiberAdmission::Ban => {
                debug!(
                    peer = format!("{pubkey:?}"),
                    "Disconnecting peer after repeated Fiber message rate-limit violations"
                );
                if let Err(err) = context.disconnect(context.session.id).await {
                    error!(
                        peer = format!("{pubkey:?}"),
                        %err,
                        "Failed to disconnect rate-limited Fiber peer"
                    );
                }
                return;
            }
        };

        let msg = match FiberMessage::from_molecule_slice(&data) {
            Ok(msg) => msg,
            Err(err) => {
                let banned = self
                    .peer_message_policy
                    .lock()
                    .expect("peer message policy lock")
                    .record_invalid(&pubkey, now_ms);
                debug!(
                    peer = format!("{pubkey:?}"),
                    %err,
                    banned,
                    "Dropping malformed Fiber message"
                );
                if banned {
                    if let Err(disconnect_err) = context.disconnect(context.session.id).await {
                        error!(
                            peer = format!("{pubkey:?}"),
                            %disconnect_err,
                            "Failed to disconnect peer sending malformed Fiber messages"
                        );
                    }
                }
                return;
            }
        };
        try_send_actor_message(
            &self.actor,
            NetworkActorMessage::new_event(PublicNetworkEvent::FiberMessage(
                pubkey,
                msg,
                Some(permit),
            )),
        );
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[derive(Clone, Debug)]
struct NetworkServiceHandle {
    actor: ActorRef<NetworkActorMessage>,
    peer_message_policy: Arc<StdMutex<PeerMessagePolicy>>,
}

impl NetworkServiceHandle {
    fn new(
        actor: ActorRef<NetworkActorMessage>,
        peer_message_policy: Arc<StdMutex<PeerMessagePolicy>>,
    ) -> Self {
        NetworkServiceHandle {
            actor,
            peer_message_policy,
        }
    }
}

impl From<&NetworkServiceHandle> for FiberProtocolHandle {
    fn from(handle: &NetworkServiceHandle) -> Self {
        FiberProtocolHandle {
            actor: handle.actor.clone(),
            peer_message_policy: handle.peer_message_policy.clone(),
        }
    }
}

#[async_trait]
impl ServiceHandle for NetworkServiceHandle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        debug!("Service error: {:?}", error);
        if let ServiceError::DialerError { address, error } = &error {
            if let Some(peer_id) = extract_peer_id(address) {
                try_send_actor_message(
                    &self.actor,
                    NetworkActorMessage::new_command(
                        PublicNetworkCommand::RemovePendingSavePeerAddress(peer_id.clone()),
                    ),
                );
                debug!(
                    "DialerError for peer {:?} address {:?}: {:?}",
                    peer_id, address, error
                );
                try_send_actor_message(
                    &self.actor,
                    NetworkActorMessage::new_command(
                        PublicNetworkCommand::SeedPeerReconnectBackoff(
                            peer_id,
                            PeerReconnectTrigger::DialError,
                        ),
                    ),
                );
            } else {
                debug!(
                    "DialerError on address {:?} without peer id: {:?}",
                    address, error
                );
            }
        }
        // TODO
        // ServiceError::ProtocolError => ban peer
    }

    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        debug!("Service event: {:?}", event);
    }
}

// If we are closing the whole network service, we may have already stopped the network actor.
// In that case the send_message will fail.
// Ideally, we should close tentacle network service first, then stop the network actor.
// But ractor provides only api for `post_stop` instead of `pre_stop`.
fn try_send_actor_message(actor: &ActorRef<NetworkActorMessage>, message: NetworkActorMessage) {
    let _ = actor.send_message(message);
}

#[allow(clippy::too_many_arguments)]
pub async fn start_network<
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
>(
    config: FiberConfig,
    chain_client: C,
    chain_actor: ActorRef<CkbChainMessage>,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    tracker: TaskTracker,
    root_actor: ActorCell,
    store: S,
    store_actor: Option<ActorRef<StoreActorMessage>>,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    default_shutdown_script: Script,
) -> ActorRef<NetworkActorMessage> {
    let my_pubkey = config.public_key();

    let (actor, _handle) = Actor::spawn_linked(
        Some(format!("Network {:?}", my_pubkey)),
        NetworkActor::new(
            event_sender,
            chain_actor,
            store,
            store_actor,
            network_graph,
            chain_client,
        ),
        NetworkActorStartArguments {
            config,
            tracker,
            default_shutdown_script,
        },
        root_actor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_hosted_tenant_actor<
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
>(
    config: FiberConfig,
    chain_client: C,
    chain_actor: ActorRef<CkbChainMessage>,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    root_actor: ActorCell,
    store: S,
    store_actor: Option<ActorRef<StoreActorMessage>>,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    default_shutdown_script: Script,
) -> Result<FiberActorRef, String> {
    let actor_name = format!("HostedTenant {:?}", config.public_key());
    Actor::spawn_linked(
        Some(actor_name),
        HostedTenantActor::new(FiberActorCore::new(
            event_sender,
            chain_actor,
            store,
            store_actor,
            network_graph,
            chain_client,
        )),
        HostedTenantActorStartArguments {
            config,
            default_shutdown_script,
        },
        root_actor,
    )
    .await
    .map(|(actor, _)| FiberActorRef::from_fiber(&actor))
    .map_err(|error| format!("failed to start hosted tenant actor: {error}"))
}

pub(crate) fn find_type(addr: &Multiaddr) -> TransportType {
    let mut iter = addr.iter();

    iter.find_map(|proto| match proto {
        Protocol::Ws => Some(TransportType::Ws),
        Protocol::Wss => Some(TransportType::Wss),
        Protocol::Onion3(_) => Some(TransportType::Onion),
        _ => None,
    })
    .unwrap_or(TransportType::Tcp)
}

pub(crate) fn select_connect_peer_address<I>(
    addresses: I,
    addr_type: Option<TransportType>,
) -> Option<Multiaddr>
where
    I: IntoIterator<Item = Multiaddr>,
{
    let mut rng = rand::thread_rng();

    match addr_type {
        Some(transport) => addresses
            .into_iter()
            .filter(|addr| find_type(addr) == transport)
            .choose(&mut rng),
        None => addresses
            .into_iter()
            .filter(target_default_transport_matches)
            .choose(&mut rng),
    }
}

#[cfg(target_arch = "wasm32")]
fn target_default_transport_matches(addr: &Multiaddr) -> bool {
    matches!(find_type(addr), TransportType::Ws | TransportType::Wss)
}

#[cfg(not(target_arch = "wasm32"))]
fn target_default_transport_matches(addr: &Multiaddr) -> bool {
    find_type(addr) == TransportType::Tcp
}

struct ToBeAcceptedChannels {
    total_number_limit: usize,
    total_bytes_limit: usize,
    map: HashMap<Hash256, (Pubkey, OpenChannel)>,
}

impl Default for ToBeAcceptedChannels {
    fn default() -> Self {
        Self {
            total_number_limit: usize::MAX,
            total_bytes_limit: usize::MAX,
            map: HashMap::default(),
        }
    }
}

// Remember to sync fiber/config.rs
const DEFAULT_TO_BE_ACCEPTED_CHANNELS_NUMBER_LIMIT: usize = 20;
// Remember to sync fiber/config.rs. 50KB.
const DEFAULT_TO_BE_ACCEPTED_CHANNELS_BYTES_LIMIT: usize = 51200;
// Remember to sync fiber/config.rs
const DEFAULT_PENDING_CHANNELS_NUMBER_LIMIT: usize = 100;

impl ToBeAcceptedChannels {
    fn new_with_config(config: &FiberConfig) -> Self {
        Self {
            total_number_limit: config
                .to_be_accepted_channels_number_limit
                .unwrap_or(DEFAULT_TO_BE_ACCEPTED_CHANNELS_NUMBER_LIMIT),
            total_bytes_limit: config
                .to_be_accepted_channels_bytes_limit
                .unwrap_or(DEFAULT_TO_BE_ACCEPTED_CHANNELS_BYTES_LIMIT),
            map: HashMap::default(),
        }
    }

    fn remove(&mut self, id: &Hash256) -> Option<(Pubkey, OpenChannel)> {
        self.map.remove(id)
    }

    fn pending_accept_count(&self, pubkey: &Pubkey) -> usize {
        self.map
            .values()
            .filter(|(saved_pubkey, _)| saved_pubkey == pubkey)
            .count()
    }

    // insert and apply throttle control
    fn try_insert(
        &mut self,
        id: Hash256,
        pubkey: Pubkey,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        if let Some(existing_value) = self.map.get(&id) {
            let err_message = format!(
                "A channel from {:?} of id {:?} is already awaiting to be accepted",
                &pubkey, &id,
            );
            warn!("{}: {:?}", err_message, existing_value);
            return Err(ProcessingChannelError::RepeatedProcessing(err_message));
        }

        // The map should be small because of the flow control, so calculate the total number and
        // bytes on the fly.
        let (total_number, total_bytes) = self
            .map
            .values()
            .filter(|(saved_pubkey, _)| *saved_pubkey == pubkey)
            .fold(
                (1, open_channel.mem_size()),
                |(count, size), (_, saved_open_channel)| {
                    (count + 1, size + saved_open_channel.mem_size())
                },
            );

        if total_number > self.total_number_limit {
            return Err(ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(
                format!("Total number exceeds the limit {}", self.total_number_limit),
            ));
        }
        if total_bytes > self.total_bytes_limit {
            return Err(ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(
                format!("Total bytes exceeds the limit {}", self.total_bytes_limit),
            ));
        }

        debug!(
            "Channel from {:?} of id {:?} is now awaiting to be accepted: {:?}",
            &pubkey, &id, &open_channel
        );
        self.map.insert(id, (pubkey, open_channel));
        Ok(())
    }
}
