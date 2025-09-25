use super::config::{
    DEFAULT_COMMITMENT_DELAY_EPOCHS, DEFAULT_FUNDING_TIMEOUT_SECONDS, DEFAULT_HOLD_TLC_TIMEOUT,
};
use super::{
    gossip::SOFT_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION, graph::ChannelUpdateInfo,
    types::ForwardTlcResult,
};
use crate::fiber::config::MILLI_SECONDS_PER_EPOCH;
use crate::fiber::fee::{check_open_channel_parameters, check_tlc_delta_with_epochs};
#[cfg(any(debug_assertions, feature = "bench"))]
use crate::fiber::network::DebugEvent;
use crate::fiber::network::PaymentCustomRecords;
use crate::fiber::types::TxSignatures;
use crate::{debug_event, fiber::types::TxAbort, utils::tx::compute_tx_message};
#[cfg(test)]
use musig2::BinaryEncoding;
use musig2::SecNonceBuilder;
use secp256k1::{Secp256k1, XOnlyPublicKey};
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(test)]
use std::{
    backtrace::Backtrace,
    sync::{LazyLock, Mutex},
};
use tracing::{debug, error, info, trace, warn};

use super::types::{ChannelUpdateChannelFlags, ChannelUpdateMessageFlags, UpdateTlcInfo};
use crate::time::{SystemTime, UNIX_EPOCH};
use crate::utils::payment::is_invoice_fulfilled;
use crate::{
    ckb::{
        contracts::{get_cell_deps, get_script_by_contract, Contract},
        FundingRequest,
    },
    fiber::{
        config::{DEFAULT_MIN_SHUTDOWN_FEE, MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA},
        fee::{
            calculate_commitment_tx_fee, calculate_shutdown_tx_fee, calculate_tlc_forward_fee,
            shutdown_tx_size,
        },
        hash_algorithm::HashAlgorithm,
        key::blake2b_hash_with_salt,
        network::{
            get_chain_hash, sign_network_message, FiberMessageWithPeerId, SendOnionPacketCommand,
        },
        serde_utils::{CompactSignatureAsBytes, EntityHex, PubNonceAsBytes},
        types::{
            AcceptChannel, AddTlc, AnnouncementSignatures, BroadcastMessageWithTimestamp,
            ChannelAnnouncement, ChannelReady, ChannelUpdate, ClosingSigned, CommitmentNonce,
            CommitmentSigned, EcdsaSignature, FiberChannelMessage, FiberMessage, Hash256, HoldTlc,
            OpenChannel, PaymentOnionPacket, PeeledPaymentOnionPacket, Privkey, Pubkey,
            ReestablishChannel, RemoveTlc, RemoveTlcFulfill, RemoveTlcReason, RevocationNonce,
            RevokeAndAck, Shutdown, TlcErr, TlcErrPacket, TlcErrorCode, TxCollaborationMsg,
            TxComplete, TxUpdate, NO_SHARED_SECRET,
        },
        NetworkActorCommand, NetworkActorEvent, NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE,
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceStore, PreimageStore},
    now_timestamp_as_millis_u64, NetworkServiceEvent,
};
use bitflags::bitflags;
use ckb_hash::{blake2b_256, new_blake2b};
use ckb_sdk::{util::blake160, Since, SinceType};
use ckb_types::{
    core::{
        Capacity, CapacityError, EpochNumberWithFraction, FeeRate, TransactionBuilder,
        TransactionView,
    },
    packed::{self, Bytes, CellInput, CellOutput, OutPoint, Script, Transaction},
    prelude::{AsTransactionBuilder, IntoTransactionView, Pack, Unpack},
    H256,
};
use molecule::prelude::{Builder, Entity};
use musig2::{
    aggregate_partial_signatures,
    errors::{RoundFinalizeError, SigningError, VerifyError},
    secp::Point,
    sign_partial, verify_partial, AggNonce, CompactSignature, KeyAggContext, PartialSignature,
    PubNonce, SecNonce,
};
use ractor::call;
use ractor::{
    concurrency::{Duration, JoinHandle},
    Actor, ActorProcessingErr, ActorRef, MessagingErr, OutputPort, RpcReplyPort,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::{
    fmt::{self, Debug, Display},
    sync::Arc,
};
use tentacle::secio::PeerId;
use thiserror::Error;
use tokio::sync::oneshot;

// - `empty_witness_args`: 16 bytes, fixed to 0x10000000100000001000000010000000, for compatibility with the xudt
// - `pubkey`: 32 bytes, x only aggregated public key
// - `signature`: 64 bytes, aggregated signature
pub const FUNDING_CELL_WITNESS_LEN: usize = 16 + 32 + 64;

// - `empty_witness_args`: 16 bytes, fixed to 0x10000000100000001000000010000000, for compatibility with the xudt
// - `unlock_type`: 1 byte
// - `pubkey`: 32 bytes, x only aggregated public key
// - `signature`: 64 bytes, aggregated signature
pub const COMMITMENT_CELL_WITNESS_LEN: usize = 16 + 1 + 32 + 64;

// Some part of the code liberally gets previous commitment number, which is
// the current commitment number minus 1. We deliberately set initial commitment number to 1,
// so that we can get previous commitment point/number without checking if the channel
// is funded or not.
pub const INITIAL_COMMITMENT_NUMBER: u64 = 0;

// The current goal throughput is about 20 TPS, set interval to 100 to limit retryable task
// triggered 10 times per second, plus we also trigger `apply_retryable_tlc_operations` when
// receiving ACK from peer, so it's a reason number for 20 TPS
const RETRYABLE_TLC_OPS_INTERVAL: Duration = Duration::from_millis(100);
const WAITING_REESTABLISH_FINISH_TIMEOUT: Duration = Duration::from_millis(4000);

// if a important TLC operation is not acked in 30 seconds, we will try to disconnect the peer.
#[cfg(not(any(test, feature = "bench")))]
pub const PEER_CHANNEL_RESPONSE_TIMEOUT: u64 = 30 * 1000;
#[cfg(any(test, feature = "bench"))]
pub const PEER_CHANNEL_RESPONSE_TIMEOUT: u64 = 10 * 1000;

#[derive(Debug)]
pub enum ChannelActorMessage {
    /// Command are the messages that are sent to the channel actor to perform some action.
    /// It is normally generated from a user request.
    Command(ChannelCommand),
    /// Some system events associated to a channel, such as the funding transaction confirmed.
    Event(ChannelEvent),
    /// PeerMessage are the messages sent from the peer.
    PeerMessage(FiberChannelMessage),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddTlcResponse {
    pub tlc_id: u64,
}

#[derive(Clone)]
pub struct TlcNotifyInfo {
    pub payment_hash: Hash256,
    pub tlc_id: TLCId,
    pub amount: u128,
    pub payment_preimage: Option<Hash256>,
}

#[derive(Clone)]
pub struct TlcNotification {
    pub channel_id: Hash256,
    pub tlc: TlcNotifyInfo,
    pub script: Script,
}

#[derive(Debug)]
pub enum ChannelCommand {
    TxCollaborationCommand(TxCollaborationCommand),
    FundingTxSigned(Transaction),
    CommitmentSigned(),
    AddTlc(AddTlcCommand, RpcReplyPort<Result<AddTlcResponse, TlcErr>>),
    RemoveTlc(RemoveTlcCommand, RpcReplyPort<ProcessingChannelResult>),
    Shutdown(ShutdownCommand, RpcReplyPort<Result<(), String>>),
    BroadcastChannelUpdate(),
    Update(UpdateCommand, RpcReplyPort<Result<(), String>>),
    NotifyEvent(ChannelEvent),
    #[cfg(any(test, feature = "bench"))]
    ReloadState(ReloadParams),
}

impl Display for ChannelCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelCommand::TxCollaborationCommand(_) => write!(f, "TxCollaborationCommand"),
            ChannelCommand::FundingTxSigned(_) => write!(f, "FundingTxSigned"),
            ChannelCommand::CommitmentSigned() => write!(f, "CommitmentSigned"),
            ChannelCommand::AddTlc(_, _) => write!(f, "AddTlc"),
            ChannelCommand::RemoveTlc(_, _) => write!(f, "RemoveTlc"),
            ChannelCommand::Shutdown(_, _) => write!(f, "Shutdown"),
            ChannelCommand::BroadcastChannelUpdate() => write!(f, "BroadcastChannelUpdate"),
            ChannelCommand::Update(_, _) => write!(f, "Update"),
            ChannelCommand::NotifyEvent(event) => write!(f, "NotifyEvent [{:?}]", event),
            #[cfg(any(test, feature = "bench"))]
            ChannelCommand::ReloadState(_) => write!(f, "ReloadState"),
        }
    }
}

impl ChannelCommand {
    pub fn rpc_reply_port(self) -> Option<RpcReplyPort<Result<(), String>>> {
        match self {
            ChannelCommand::Shutdown(_, port) => Some(port),
            ChannelCommand::Update(_, port) => Some(port),
            _ => None,
        }
    }
}

#[cfg(any(test, feature = "bench"))]
#[derive(Debug)]
pub struct ReloadParams {
    pub notify_changes: bool,
}

#[cfg(any(test, feature = "bench"))]
impl Default for ReloadParams {
    fn default() -> Self {
        Self {
            notify_changes: true,
        }
    }
}

#[derive(Debug)]
pub enum TxCollaborationCommand {
    TxUpdate(TxUpdateCommand),
    TxComplete(),
}

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct AddTlcCommand {
    pub amount: u128,
    pub payment_hash: Hash256,
    /// The attempt id associate with the tlc
    pub attempt_id: Option<u64>,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    /// Onion packet for the next node
    pub onion_packet: Option<PaymentOnionPacket>,
    /// Shared secret used in forwarding.
    ///
    /// Save it for outbound (offered) TLC to backward errors.
    /// Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
    pub previous_tlc: Option<PrevTlcInfo>,
}

impl Debug for AddTlcCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AddTlcCommand")
            .field("amount", &self.amount)
            .field("payment_hash", &self.payment_hash)
            .field("attempt_id", &self.attempt_id)
            .field("expiry", &self.expiry)
            .field("hash_algorithm", &self.hash_algorithm)
            .field("previous_tlc", &self.previous_tlc)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RemoveTlcCommand {
    pub id: u64,
    pub reason: RemoveTlcReason,
}

#[derive(Debug)]
pub struct ShutdownCommand {
    pub close_script: Option<Script>,
    pub fee_rate: Option<FeeRate>,
    pub force: bool,
}

#[derive(Debug)]
pub struct UpdateCommand {
    pub enabled: Option<bool>,
    pub tlc_expiry_delta: Option<u64>,
    pub tlc_minimum_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
}

#[derive(Debug)]
pub struct ChannelCommandWithId {
    pub channel_id: Hash256,
    pub command: ChannelCommand,
}

pub const DEFAULT_FEE_RATE: u64 = 1_000;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 1_000;
// The min commitment delay is 1 epoch = 4 hours.
pub const MIN_COMMITMENT_DELAY_EPOCHS: u64 = 1;
// The max commitment delay is 84 epochs = 14 days.
pub const MAX_COMMITMENT_DELAY_EPOCHS: u64 = 84;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u128 = u128::MAX;
pub const DEFAULT_MIN_TLC_VALUE: u128 = 0;
pub const SYS_MAX_TLC_NUMBER_IN_FLIGHT: u64 = 253;
pub const MAX_TLC_NUMBER_IN_FLIGHT: u64 = 125;

#[derive(Debug)]
pub struct TxUpdateCommand {
    pub transaction: Transaction,
}

pub struct OpenChannelParameter {
    pub funding_amount: u128,
    pub seed: [u8; 32],
    pub tlc_info: ChannelTlcInfo,
    pub public_channel_info: Option<PublicChannelInfo>,
    pub funding_udt_type_script: Option<Script>,
    pub shutdown_script: Script,
    pub channel_id_sender: oneshot::Sender<Hash256>,
    pub commitment_fee_rate: Option<u64>,
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    pub funding_fee_rate: Option<u64>,
    pub max_tlc_value_in_flight: u128,
    pub max_tlc_number_in_flight: u64,
}

pub struct AcceptChannelParameter {
    pub funding_amount: u128,
    pub reserved_ckb_amount: u64,
    pub tlc_info: ChannelTlcInfo,
    pub public_channel_info: Option<PublicChannelInfo>,
    pub seed: [u8; 32],
    pub open_channel: OpenChannel,
    pub shutdown_script: Script,
    pub channel_id_sender: Option<oneshot::Sender<Hash256>>,
    pub max_tlc_value_in_flight: u128,
    pub max_tlc_number_in_flight: u64,
}

// Ephemeral config for channel which does not need to persist.
#[derive(Clone, Debug)]
pub struct ChannelEphemeralConfig {
    // Timeout to auto close a funding channel
    pub funding_timeout_seconds: u64,
}

impl Default for ChannelEphemeralConfig {
    fn default() -> Self {
        Self {
            funding_timeout_seconds: DEFAULT_FUNDING_TIMEOUT_SECONDS,
        }
    }
}

pub enum ChannelInitializationOperation {
    /// To open a new channel to another peer, the funding amount,
    /// the temporary channel id a unique channel seed to generate
    /// channel secrets must be given.
    OpenChannel(OpenChannelParameter),
    /// To accept a new channel from another peer, the funding amount,
    /// a unique channel seed to generate unique channel id,
    /// original OpenChannel message and an oneshot
    /// channel to receive the new channel ID must be given.
    AcceptChannel(AcceptChannelParameter),
    /// Reestablish a channel with given channel id.
    ReestablishChannel(Hash256),
}

pub struct ChannelInitializationParameter {
    pub operation: ChannelInitializationOperation,
    pub ephemeral_config: ChannelEphemeralConfig,
    pub private_key: Privkey,
}

#[derive(Clone)]
pub struct ChannelSubscribers {
    pub pending_received_tlcs_subscribers: Arc<OutputPort<TlcNotification>>,
    pub settled_tlcs_subscribers: Arc<OutputPort<TlcNotification>>,
}

impl Default for ChannelSubscribers {
    fn default() -> Self {
        Self {
            pending_received_tlcs_subscribers: Arc::new(OutputPort::default()),
            settled_tlcs_subscribers: Arc::new(OutputPort::default()),
        }
    }
}

pub struct ChannelActor<S> {
    local_pubkey: Pubkey,
    remote_pubkey: Pubkey,
    network: ActorRef<NetworkActorMessage>,
    store: S,
    subscribers: ChannelSubscribers,
}

impl<S> ChannelActor<S>
where
    S: ChannelActorStateStore + InvoiceStore + PreimageStore,
{
    pub fn new(
        local_pubkey: Pubkey,
        remote_pubkey: Pubkey,
        network: ActorRef<NetworkActorMessage>,
        store: S,
        subscribers: ChannelSubscribers,
    ) -> Self {
        Self {
            local_pubkey,
            remote_pubkey,
            network,
            store,
            subscribers,
        }
    }

    pub fn get_local_pubkey(&self) -> Pubkey {
        self.local_pubkey
    }

    pub fn get_remote_pubkey(&self) -> Pubkey {
        self.remote_pubkey
    }

    pub fn get_local_peer_id(&self) -> PeerId {
        self.local_pubkey.tentacle_peer_id()
    }

    pub fn get_remote_peer_id(&self) -> PeerId {
        self.remote_pubkey.tentacle_peer_id()
    }

    pub async fn handle_peer_message(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        message: FiberChannelMessage,
    ) -> ProcessingChannelResult {
        if state.reestablishing {
            match message {
                FiberChannelMessage::ReestablishChannel(ref reestablish_channel) => {
                    state.handle_reestablish_channel_message(myself, reestablish_channel)?;
                    if !state.reestablishing {
                        state.schedule_next_retry_task(myself);
                    }
                }
                _ => {
                    debug!("Ignoring message while reestablishing: {:?}", message);
                }
            }
            return Ok(());
        }

        match message {
            FiberChannelMessage::AnnouncementSignatures(announcement_signatures) => {
                if !state.is_public() {
                    return Err(ProcessingChannelError::InvalidState(
                        "Received AnnouncementSignatures message, but the channel is not public"
                            .to_string(),
                    ));
                }
                match state.state {
                    ChannelState::ChannelReady => {}
                    ChannelState::AwaitingChannelReady(flags)
                        if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) => {}
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                                "Received unexpected AnnouncementSignatures message in state {:?}, expecting state AwaitingChannelReady::CHANNEL_READY or ChannelReady",
                                state.state
                            )));
                    }
                }

                // TODO: check announcement_signatures validity here.
                let AnnouncementSignatures {
                    node_signature,
                    partial_signature,
                    ..
                } = announcement_signatures;
                state.update_remote_channel_announcement_signature(
                    node_signature,
                    partial_signature,
                );
                state.maybe_public_channel_is_ready(myself);
                Ok(())
            }
            FiberChannelMessage::AcceptChannel(accept_channel) => {
                state.handle_accept_channel_message(accept_channel)?;
                let old_id = state.get_id();
                state.fill_in_channel_id();
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelAccepted(
                            state.get_remote_peer_id(),
                            state.get_id(),
                            old_id,
                            state.to_local_amount,
                            state.to_remote_amount,
                            state.get_funding_lock_script(),
                            state.funding_udt_type_script.clone(),
                            state.local_reserved_ckb_amount,
                            state.remote_reserved_ckb_amount,
                            state.funding_fee_rate,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                Ok(())
            }
            FiberChannelMessage::TxUpdate(tx) => {
                state
                    .handle_tx_collaboration_msg(myself, TxCollaborationMsg::TxUpdate(tx))
                    .await
            }
            FiberChannelMessage::TxComplete(tx) => {
                state
                    .handle_tx_collaboration_msg(myself, TxCollaborationMsg::TxComplete(tx))
                    .await?;
                if let ChannelState::CollaboratingFundingTx(flags) = state.state {
                    if flags.contains(CollaboratingFundingTxFlags::COLLABORATION_COMPLETED) {
                        state.increment_local_commitment_number();
                        self.handle_commitment_signed_command(myself, state).await?;
                    }
                }
                Ok(())
            }
            FiberChannelMessage::CommitmentSigned(commitment_signed) => {
                self.handle_commitment_signed_peer_message(myself, state, commitment_signed)
                    .await
            }
            FiberChannelMessage::TxSignatures(tx_signatures) => {
                // We're the one who sent tx_signature first, and we received a tx_signature message.
                // This means that the tx_signature procedure is now completed. Just change state,
                // and exit.

                if state.should_local_send_tx_signatures_first() {
                    let Some(funding_tx) = state.funding_tx.clone() else {
                        return Err(ProcessingChannelError::InvalidState(format!(
                        "Received TxSignatures message, but the channel's funding tx is none {:?}",
                        state.state
                    )));
                    };
                    let new_witnesses: Vec<_> = tx_signatures
                        .witnesses
                        .into_iter()
                        .map(|x| x.pack())
                        .collect();
                    debug!(
                        "Updating funding tx witnesses of {:?} to {:?}",
                        funding_tx.calc_tx_hash(),
                        new_witnesses.iter().map(|x| hex::encode(x.as_slice()))
                    );
                    state.funding_tx = Some(
                        funding_tx
                            .as_advanced_builder()
                            .set_witnesses(new_witnesses)
                            .build()
                            .data(),
                    );
                    self.network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionPending(
                                state.must_get_funding_transaction().clone(),
                                state.must_get_funding_transaction_outpoint(),
                                state.get_id(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                    state.update_state(ChannelState::AwaitingChannelReady(
                        AwaitingChannelReadyFlags::empty(),
                    ));
                    return Ok(());
                } else {
                    state.handle_tx_signatures(Some(tx_signatures.witnesses))?;
                }
                Ok(())
            }
            FiberChannelMessage::RevokeAndAck(revoke_and_ack) => {
                state.handle_revoke_and_ack_peer_message(myself, revoke_and_ack)?;
                self.update_tlc_status_on_ack(myself, state).await;
                if state.tlc_state.need_another_commitment_signed() {
                    self.handle_commitment_signed_command(myself, state).await?;
                }
                if !state.is_waiting_tlc_ack() {
                    self.apply_retryable_tlc_operations(myself, state, false)
                        .await;
                }
                Ok(())
            }
            FiberChannelMessage::ChannelReady(_channel_ready) => {
                let flags = match state.state {
                    ChannelState::AwaitingTxSignatures(flags) => {
                        if flags.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) {
                            AwaitingChannelReadyFlags::empty()
                        } else {
                            return Err(ProcessingChannelError::InvalidState(format!(
                                "received ChannelReady message, but we're not ready for ChannelReady, state is currently {:?}",
                                state.state
                            )));
                        }
                    }
                    ChannelState::AwaitingChannelReady(flags) => flags,
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "received ChannelReady message, but we're not ready for ChannelReady, state is currently {:?}", state.state
                        )));
                    }
                };
                let flags = flags | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY;
                state.update_state(ChannelState::AwaitingChannelReady(flags));
                state.maybe_channel_is_ready(myself);
                Ok(())
            }
            FiberChannelMessage::UpdateTlcInfo(update_tlc_info) => {
                state.remote_tlc_info = Some(update_tlc_info.into());
                state.update_graph_for_remote_channel_change();
                Ok(())
            }
            FiberChannelMessage::AddTlc(add_tlc) => {
                self.handle_add_tlc_peer_message(state, add_tlc)
            }
            FiberChannelMessage::RemoveTlc(remove_tlc) => {
                self.handle_remove_tlc_peer_message(state, remove_tlc)
            }
            FiberChannelMessage::Shutdown(shutdown) => {
                self.handle_shutdown_peer_message(state, shutdown).await
            }
            FiberChannelMessage::ClosingSigned(closing) => {
                let ClosingSigned {
                    partial_signature,
                    channel_id,
                } = closing;

                if channel_id != state.get_id() {
                    return Err(ProcessingChannelError::InvalidParameter(
                        "Channel id mismatch".to_string(),
                    ));
                }

                // Note that we don't check the validity of the signature here.
                // we will check the validity when we're about to build the shutdown tx.
                // This may be or may not be a problem.
                // We do this to simplify the handling of the message.
                // We may change this in the future.
                // We also didn't check the state here.
                if let Some(shutdown_info) = state.remote_shutdown_info.as_mut() {
                    shutdown_info.signature = Some(partial_signature);
                }

                state.maybe_transfer_to_shutdown().await?;
                Ok(())
            }
            FiberChannelMessage::ReestablishChannel(ref reestablish_channel) => {
                state.handle_reestablish_channel_message(myself, reestablish_channel)?;
                Ok(())
            }
            FiberChannelMessage::TxAbort(_) => {
                if state.state.can_abort_funding() {
                    state.update_state(ChannelState::Closed(CloseFlags::FUNDING_ABORTED));
                    myself.stop(Some("Funding abort".to_string()));
                }
                Ok(())
            }
            FiberChannelMessage::TxInitRBF(_) | FiberChannelMessage::TxAckRBF(_) => {
                warn!("Received unsupported message: {:?}", &message);
                Ok(())
            }
        }
    }

    fn get_tlc_error(
        &self,
        state: &mut ChannelActorState,
        error: &ProcessingChannelError,
    ) -> TlcErr {
        let error_code = match error {
            ProcessingChannelError::PeelingOnionPacketError(_) => TlcErrorCode::InvalidOnionPayload,
            ProcessingChannelError::TlcForwardFeeIsTooLow => TlcErrorCode::FeeInsufficient,
            ProcessingChannelError::TlcExpirySoon => TlcErrorCode::ExpiryTooSoon,
            ProcessingChannelError::TlcExpiryTooFar => TlcErrorCode::ExpiryTooFar,
            ProcessingChannelError::FinalInvoiceInvalid(status) => match status {
                CkbInvoiceStatus::Expired => TlcErrorCode::InvoiceExpired,
                CkbInvoiceStatus::Cancelled => TlcErrorCode::InvoiceCancelled,
                _ => TlcErrorCode::IncorrectOrUnknownPaymentDetails,
            },
            ProcessingChannelError::FinalIncorrectPreimage
            | ProcessingChannelError::FinalIncorrectPaymentHash
            | ProcessingChannelError::FinalIncorrectMPPInfo(_) => {
                TlcErrorCode::IncorrectOrUnknownPaymentDetails
            }
            ProcessingChannelError::FinalIncorrectHTLCAmount => {
                TlcErrorCode::FinalIncorrectTlcAmount
            }
            ProcessingChannelError::IncorrectTlcExpiry => TlcErrorCode::IncorrectTlcExpiry,
            ProcessingChannelError::IncorrectFinalTlcExpiry => {
                TlcErrorCode::FinalIncorrectExpiryDelta
            }
            ProcessingChannelError::TlcAmountIsTooLow => TlcErrorCode::AmountBelowMinimum,
            ProcessingChannelError::TlcNumberExceedLimit
            | ProcessingChannelError::TlcAmountExceedLimit
            | ProcessingChannelError::TlcValueInflightExceedLimit => {
                TlcErrorCode::TemporaryChannelFailure
            }
            ProcessingChannelError::WaitingTlcAck => TlcErrorCode::TemporaryChannelFailure,
            ProcessingChannelError::InternalError(_) => TlcErrorCode::TemporaryNodeFailure,
            ProcessingChannelError::InvalidState(_) => match state.state {
                // we can not revert back up `ChannelReady` after `ShuttingDown`
                ChannelState::Closed(_) | ChannelState::ShuttingDown(_) => {
                    TlcErrorCode::PermanentChannelFailure
                }
                ChannelState::ChannelReady => {
                    if !state.local_tlc_info.enabled {
                        // channel is disabled
                        TlcErrorCode::ChannelDisabled
                    } else {
                        // we expect `ChannelReady` will be both OK for tlc forwarding,
                        // so here are the unreachable point in normal workflow,
                        // set `TemporaryNodeFailure` for general temporary failure of the processing node here
                        debug_assert!(false, "unreachable point in normal workflow");
                        TlcErrorCode::TemporaryNodeFailure
                    }
                }
                // otherwise, channel maybe not ready
                _ => TlcErrorCode::TemporaryChannelFailure,
            },
            ProcessingChannelError::RepeatedProcessing(_) => TlcErrorCode::TemporaryChannelFailure,
            ProcessingChannelError::SpawnErr(_)
            | ProcessingChannelError::Musig2RoundFinalizeError(_)
            | ProcessingChannelError::Musig2SigningError(_)
            | ProcessingChannelError::Musig2VerifyError(_)
            | ProcessingChannelError::CapacityError(_) => TlcErrorCode::TemporaryNodeFailure,
            ProcessingChannelError::InvalidParameter(_) => {
                TlcErrorCode::IncorrectOrUnknownPaymentDetails
            }
            ProcessingChannelError::TlcForwardingError(_) => {
                unreachable!("TlcForwardingError should be handled before this point")
            }
            ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(_) => {
                unreachable!("ToBeAcceptedChannelsExceedLimit should be handled before this point")
            }
        };

        let channel_update = if error_code.is_update() {
            state.try_create_channel_update_message()
        } else {
            None
        };
        TlcErr::new_channel_fail(
            error_code,
            state.local_pubkey,
            state.must_get_funding_transaction_outpoint(),
            channel_update,
        )
    }

    async fn handle_commitment_signed_peer_message(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        commitment_signed: CommitmentSigned,
    ) -> ProcessingChannelResult {
        // build commitment tx and verify signature from remote, if passed send ACK for partner
        if let Err(err) = state.verify_commitment_signed_and_send_ack(commitment_signed.clone()) {
            error!(
                "Failed to verify commitment_signed message: {:?}, shutdown channel {} forcefully",
                err,
                state.get_id()
            );
            self.notify_network_actor_shutdown_me(state);
            return Err(err);
        }
        let need_commitment_signed = state.tlc_state.update_for_commitment_signed();

        // flush remove tlc for received tlcs after replying ack for peer
        self.apply_settled_remove_tlcs(state, true).await;

        // when we transfer to shutdown state, we need to build shutdown transaction
        // here `maybe_transfer_to_shutdown` must be called after `apply_settled_remove_tlcs`
        // so the closing transaction is symmetric with peer
        state.maybe_transfer_to_shutdown().await?;

        if need_commitment_signed && !state.tlc_state.waiting_ack {
            self.handle_commitment_signed_command(myself, state).await?;
        }

        Ok(())
    }

    async fn apply_settled_remove_tlcs(&self, state: &mut ChannelActorState, inbound: bool) {
        let previous_balance = state.get_local_balance();
        let pending_tlcs = if inbound {
            state.tlc_state.received_tlcs.tlcs.iter()
        } else {
            state.tlc_state.offered_tlcs.tlcs.iter()
        };
        let settled_tlcs: Vec<_> = pending_tlcs
            .filter(|tlc| {
                tlc.removed_reason.is_some()
                    && matches!(
                        tlc.status,
                        TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed)
                            | TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed)
                    )
                    && !state.tlc_state.applied_remove_tlcs.contains(&tlc.tlc_id)
            })
            .map(|tlc| tlc.tlc_id)
            .collect();

        for tlc_id in settled_tlcs {
            self.apply_remove_tlc_operation(state, tlc_id)
                .await
                .expect("expect remove tlc success");
        }

        if state.get_local_balance() != previous_balance {
            state.update_graph_for_local_channel_change();
            state.update_graph_for_remote_channel_change();
        }
    }

    fn process_add_tlc_error(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        #[allow(unused_variables)] payment_hash: Hash256,
        tlc_id: TLCId,
        error: ProcessingChannelErrorWithSharedSecret,
    ) {
        let tlc_err = match error.source {
            // If we already have TlcErr, we can directly use it to send back to the peer.
            ProcessingChannelError::TlcForwardingError(tlc_err) => tlc_err,
            _ => {
                let error_detail = self.get_tlc_error(state, &error.source);
                #[cfg(debug_assertions)]
                self.network
                    .clone()
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::DebugEvent(DebugEvent::AddTlcFailed(
                            state.get_local_peer_id(),
                            payment_hash,
                            error_detail.clone(),
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                error_detail
            }
        };
        let error_packet = TlcErrPacket::new(
            tlc_err,
            // There's no shared secret stored in the received TLC, use the one found in the peeled onion packet.
            &error.shared_secret,
        );

        self.register_retryable_tlc_remove(
            myself,
            state,
            tlc_id,
            RemoveTlcReason::RemoveTlcFail(error_packet),
        );
    }

    async fn update_tlc_status_on_ack(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
    ) {
        let apply_tlcs: Vec<TlcInfo> = state
            .tlc_state
            .get_committed_received_tlcs()
            .into_iter()
            .filter(|tlc| tlc.removed_reason.is_none())
            .filter(|tlc| !state.tlc_state.applied_add_tlcs.contains(&tlc.tlc_id))
            .collect();

        for add_tlc in apply_tlcs {
            assert!(add_tlc.is_received());
            if let Err(error) = self.apply_add_tlc_operation(myself, state, &add_tlc) {
                self.process_add_tlc_error(
                    myself,
                    state,
                    add_tlc.payment_hash,
                    add_tlc.tlc_id,
                    error,
                );
            }
        }

        // flush outbound tlcs
        self.apply_settled_remove_tlcs(state, false).await;
    }

    async fn try_to_relay_remove_tlc(&self, tlc_info: &TlcInfo, remove_reason: RemoveTlcReason) {
        let (previous_channel_id, previous_tlc) =
            tlc_info.previous_tlc.expect("expect previous tlc");
        debug_assert!(tlc_info.is_offered());
        debug_assert!(previous_tlc.is_received());

        let remove_reason = remove_reason.clone().backward(&tlc_info.shared_secret);

        let _ = self.register_retryable_relay_tlc_remove(
            previous_tlc,
            previous_channel_id,
            remove_reason,
        );
    }

    fn try_to_settle_down_tlc(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        tlc_id: TLCId,
    ) {
        let tlc_info = state.get_received_tlc(tlc_id).expect("expect tlc").clone();

        let Some(preimage) = self.store.get_preimage(&tlc_info.payment_hash) else {
            return;
        };

        let mut remove_reason = RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        });

        let tlc = tlc_info.clone();

        if let Some(invoice) = self.store.get_invoice(&tlc.payment_hash) {
            let status = self.get_invoice_status(&invoice);
            match status {
                CkbInvoiceStatus::Expired => {
                    remove_reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                        TlcErr::new(TlcErrorCode::InvoiceExpired),
                        &tlc.shared_secret,
                    ));
                }
                CkbInvoiceStatus::Cancelled => {
                    remove_reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                        TlcErr::new(TlcErrorCode::InvoiceCancelled),
                        &tlc.shared_secret,
                    ));
                }
                CkbInvoiceStatus::Paid => {
                    // we have already checked invoice status in apply_add_tlc_operation_with_peeled_onion_packet
                    // this maybe happened when process is killed and restart
                    error!("invoice already paid, ignore");
                    return;
                }
                _ if invoice.allow_mpp() => {
                    // add to pending settlement tlc set
                    // the tlc set will be settled by network actor
                    state
                        .pending_notify_mpp_tlcs
                        .push((tlc.payment_hash, tlc.id()));

                    // just return, the tlc set will be settled by network actor
                    return;
                }
                _ => {
                    // single path payment
                    if !is_invoice_fulfilled(&invoice, std::slice::from_ref(&tlc)) {
                        remove_reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                            TlcErr::new(TlcErrorCode::IncorrectOrUnknownPaymentDetails),
                            &tlc.shared_secret,
                        ));
                    }
                }
            }
        }

        // remove tlc
        self.register_retryable_tlc_remove(myself, state, tlc.tlc_id, remove_reason);
    }

    fn apply_add_tlc_operation(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        add_tlc: &TlcInfo,
    ) -> Result<(), ProcessingChannelErrorWithSharedSecret> {
        // If needed, shared secret also get be extracted from the encrypted onion packet:
        // - Extract public key from onion_packet[1..34]
        // - Obtain share secret using DH Key Exchange from the public key
        // and the network private key stored in the network actor state.
        match add_tlc.onion_packet.clone() {
            Some(onion_packet) => {
                let peeled = onion_packet
                    .peel(
                        state.private_key(),
                        Some(add_tlc.payment_hash.as_ref()),
                        &Secp256k1::new(),
                    )
                    .map_err(|err| ProcessingChannelError::PeelingOnionPacketError(err.to_string()))
                    .map_err(ProcessingChannelError::without_shared_secret)?;
                let shared_secret = peeled.shared_secret;
                self.apply_add_tlc_operation_with_peeled_onion_packet(state, add_tlc, peeled)
                    .map_err(move |err| err.with_shared_secret(shared_secret))?;
            }
            None => {
                // The TLC is with a NO_SHARED_SECRET and no onion packet.
                // this may only happen in testing or development environment.
                debug_assert!(add_tlc.onion_packet.is_none());
                #[cfg(not(debug_assertions))]
                {
                    return Err(ProcessingChannelError::PeelingOnionPacketError(
                        "TLC with no onion packet is not supported".to_string(),
                    )
                    .without_shared_secret());
                }
            }
        }

        if let Some(ref udt_type_script) = state.funding_udt_type_script {
            self.subscribers
                .pending_received_tlcs_subscribers
                .send(TlcNotification {
                    tlc: add_tlc.clone().into(),
                    channel_id: state.get_id(),
                    script: udt_type_script.clone(),
                });
        }

        // we don't need to settle down the tlc if it is not the last hop here,
        // some e2e tests are calling AddTlc manually, so we can not use onion packet to
        // check whether it's the last hop here, maybe need to revisit in future.
        self.try_to_settle_down_tlc(myself, state, add_tlc.tlc_id);

        warn!("finished check tlc for peer message: {:?}", &add_tlc.tlc_id);
        Ok(())
    }

    fn apply_add_tlc_operation_with_peeled_onion_packet(
        &self,
        state: &mut ChannelActorState,
        add_tlc: &TlcInfo,
        peeled_onion_packet: PeeledPaymentOnionPacket,
    ) -> ProcessingChannelResult {
        let payment_hash = add_tlc.payment_hash;
        let forward_amount = peeled_onion_packet.current.amount;

        state.tlc_state.applied_add_tlcs.insert(add_tlc.tlc_id);
        if peeled_onion_packet.is_last() {
            if forward_amount != add_tlc.amount {
                return Err(ProcessingChannelError::FinalIncorrectHTLCAmount);
            }

            if add_tlc.expiry < peeled_onion_packet.current.expiry {
                return Err(ProcessingChannelError::IncorrectFinalTlcExpiry);
            }

            let invoice = self.store.get_invoice(&payment_hash);
            if let Some(ref invoice) = invoice {
                let invoice_status = self.get_invoice_status(invoice);
                if !matches!(
                    invoice_status,
                    CkbInvoiceStatus::Open | CkbInvoiceStatus::Received
                ) {
                    return Err(ProcessingChannelError::FinalInvoiceInvalid(invoice_status));
                }
            }

            let Some(tlc) = state.tlc_state.get_mut(&add_tlc.tlc_id) else {
                return Err(ProcessingChannelError::InternalError(
                    "TLC not found in state".to_string(),
                ));
            };

            // extract MPP total payment fields from onion packet
            match (&invoice, peeled_onion_packet.mpp_custom_records()) {
                (Some(invoice), Some(record)) => {
                    if record.total_amount < invoice.amount.unwrap_or_default() {
                        error!(
                            "total amount is less than invoice amount: {:?}",
                            payment_hash
                        );
                        return Err(ProcessingChannelError::FinalIncorrectMPPInfo(
                            "total amount in records is less than invoice amount".to_string(),
                        ));
                    }

                    let payment_secret = invoice.payment_secret();
                    if payment_secret.is_some_and(|s| s != &record.payment_secret) {
                        error!(
                            "payment secret is not equal to invoice payment secret: {:?}",
                            payment_hash
                        );
                        return Err(ProcessingChannelError::FinalIncorrectMPPInfo(
                            "payment secret mismatch".to_string(),
                        ));
                    }

                    tlc.payment_secret = Some(record.payment_secret);
                    tlc.total_amount = Some(record.total_amount);
                }
                (Some(invoice), None) => {
                    if invoice.allow_mpp() {
                        // FIXME: whether we allow MPP without MPP records in onion packet?
                        // currently we allow it pay with enough amount
                        // TODO: add a unit test of using single path payment pay MPP invoice successfully
                        warn!(
                            "invoice allows MPP but no MPP records in onion packet: {:?}",
                            payment_hash
                        );
                    }
                    if !is_invoice_fulfilled(invoice, std::slice::from_ref(tlc)) {
                        error!("invoice is not fulfilled for payment: {:?}", payment_hash);
                        return Err(ProcessingChannelError::FinalIncorrectHTLCAmount);
                    }
                }
                (None, Some(_record)) => {
                    error!("invoice not found for MPP payment: {:?}", payment_hash);
                    return Err(ProcessingChannelError::FinalIncorrectMPPInfo(
                        "invoice not found".to_string(),
                    ));
                }
                _ => {
                    // single path payment with keysend
                }
            }

            // if this is the last hop, store the preimage.
            // though we will RemoveTlcFulfill the TLC in try_to_settle_down_tlc function,
            // here we can do error check early here for better error handling.
            let preimage = peeled_onion_packet
                .current
                .payment_preimage
                .or_else(|| self.store.get_preimage(&add_tlc.payment_hash));

            if let Some(preimage) = preimage {
                let filled_payment_hash: Hash256 = add_tlc.hash_algorithm.hash(preimage).into();
                if add_tlc.payment_hash != filled_payment_hash {
                    error!(
                        "preimage is not matched for payment hash: {:?}",
                        payment_hash
                    );
                    return Err(ProcessingChannelError::FinalIncorrectPreimage);
                }
                // update invoice status to received only all the error checking passed
                if invoice.is_some() {
                    self.store
                        .update_invoice_status(&payment_hash, CkbInvoiceStatus::Received)
                        .expect("update invoice status failed");
                }

                if let Some(custom_records) = peeled_onion_packet.current.custom_records {
                    self.store
                        .insert_payment_custom_records(&payment_hash, custom_records);
                }

                self.store_preimage(payment_hash, preimage);
            } else {
                error!("preimage is not found for payment hash: {:?}", payment_hash);
                return Err(ProcessingChannelError::FinalIncorrectPaymentHash);
            }
        } else {
            if add_tlc.expiry
                < peeled_onion_packet.current.expiry + state.local_tlc_info.tlc_expiry_delta
            {
                return Err(ProcessingChannelError::IncorrectTlcExpiry);
            }

            let received_amount = add_tlc.amount;
            if received_amount < forward_amount {
                return Err(ProcessingChannelError::InvalidParameter(
                    "received_amount is less than forward_amount".to_string(),
                ));
            }

            // Next forwarding channel will get the forward_fee and check if it's enough.
            let forward_fee = received_amount.saturating_sub(forward_amount);

            // if this is not the last hop, forward TLC to next hop
            self.register_and_apply_forward_tlc(
                state,
                add_tlc.payment_hash,
                add_tlc.tlc_id,
                peeled_onion_packet,
                forward_fee,
            );
        }
        Ok(())
    }

    fn store_preimage(&self, payment_hash: Hash256, preimage: Hash256) {
        self.store.insert_preimage(payment_hash, preimage);
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::PreimageCreated(payment_hash, preimage),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    fn handle_add_tlc_peer_message(
        &self,
        state: &mut ChannelActorState,
        add_tlc: AddTlc,
    ) -> ProcessingChannelResult {
        // TODO: here we only check the error which sender didn't follow agreed rules,
        //       if any error happened here we need go to shutdown procedure

        state.check_for_tlc_update(Some(add_tlc.amount), false, false)?;
        let tlc_info = state.create_inbounding_tlc(add_tlc.clone())?;
        state.check_insert_tlc(&tlc_info)?;
        state.tlc_state.add_received_tlc(tlc_info);
        state.increment_next_received_tlc_id();
        Ok(())
    }

    fn handle_remove_tlc_peer_message(
        &self,
        state: &mut ChannelActorState,
        remove_tlc: RemoveTlc,
    ) -> ProcessingChannelResult {
        state.check_for_tlc_update(None, false, false)?;
        // TODO: here if we received a invalid remove tlc, it's maybe a malioucious peer,
        // maybe we need to go through shutdown process for this error
        state
            .check_remove_tlc_with_reason(TLCId::Offered(remove_tlc.tlc_id), &remove_tlc.reason)?;
        let payment_hash = state
            .tlc_state
            .set_offered_tlc_removed(remove_tlc.tlc_id, remove_tlc.reason.clone());
        if let RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }) =
            remove_tlc.reason
        {
            // we need to store the preimage if the TLC is fulfilled
            // incase the peer has already shutdown the channel,
            // so we can send setttlement transaction to get money when necessary
            // the preimage must be valid since we have checked it in check_remove_tlc_with_reason
            self.store_preimage(payment_hash, payment_preimage);
            debug_event!(
                self.network,
                &format!("store payment_preimage for: {:?}", payment_hash)
            );
        }
        Ok(())
    }

    async fn handle_shutdown_peer_message(
        &self,
        state: &mut ChannelActorState,
        shutdown: Shutdown,
    ) -> ProcessingChannelResult {
        debug!("Received Shutdown message from peer: {:?}", shutdown);
        #[cfg(debug_assertions)]
        state.tlc_state.debug();
        let flags = match state.state {
            ChannelState::ChannelReady => {
                if state.tlc_state.all_tlcs().any(|tlc| {
                    matches!(
                        tlc.status,
                        TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced)
                    )
                }) {
                    return Err(ProcessingChannelError::InvalidState(
                        "Unable to process shutdown command peer message, as there are pending inbound tlcs"
                            .to_string(),
                    ));
                }
                ShuttingDownFlags::empty()
            }
            ChannelState::ShuttingDown(flags)
                if flags.contains(ShuttingDownFlags::THEIR_SHUTDOWN_SENT) =>
            {
                return Err(ProcessingChannelError::InvalidParameter(
                    "Received Shutdown message, but we're already in ShuttingDown state"
                        .to_string(),
                ));
            }
            ChannelState::ShuttingDown(flags) => flags,
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "received Shutdown message, but we're not ready for Shutdown, state is currently {:?}",
                    state.state
                )));
            }
        };

        if !state.check_shutdown_fee_valid(shutdown.fee_rate.as_u64()) {
            return Err(ProcessingChannelError::InvalidParameter(
                "Shutdown fee is invalid".to_string(),
            ));
        }

        state.remote_shutdown_info = Some(ShutdownInfo {
            close_script: shutdown.close_script,
            fee_rate: shutdown.fee_rate.as_u64(),
            signature: None,
        });

        let mut flags = flags | ShuttingDownFlags::THEIR_SHUTDOWN_SENT;

        // Only automatically reply shutdown if only their shutdown message is sent.
        // If we are in a state other than only their shutdown is sent,
        // e.g. our shutdown message is also sent, or we are trying to force shutdown,
        // we should not reply.
        let should_we_reply_shutdown = matches!(flags, ShuttingDownFlags::THEIR_SHUTDOWN_SENT);
        if state.check_valid_to_auto_accept_shutdown() && should_we_reply_shutdown {
            let close_script = state.get_local_shutdown_script();
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                        state.get_remote_peer_id(),
                        FiberMessage::shutdown(Shutdown {
                            channel_id: state.get_id(),
                            close_script: close_script.clone(),
                            fee_rate: FeeRate::from_u64(0),
                        }),
                    )),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            state.local_shutdown_info = Some(ShutdownInfo {
                close_script,
                fee_rate: 0,
                signature: None,
            });
            flags |= ShuttingDownFlags::OUR_SHUTDOWN_SENT;
            debug!("Auto accept shutdown ...");
        }

        // TODO: there maybe some tlcs still not settled when shutdown,
        // we need to check if we need to trigger remove tlc for previous channel
        // maybe could be done in cron task from network actor.
        state.update_state(ChannelState::ShuttingDown(flags));
        state.maybe_transfer_to_shutdown().await?;

        Ok(())
    }

    async fn apply_remove_tlc_operation(
        &self,
        state: &mut ChannelActorState,
        tlc_id: TLCId,
    ) -> ProcessingChannelResult {
        let channel_id = state.get_id();
        assert!(!state.tlc_state.applied_remove_tlcs.contains(&tlc_id));
        state.tlc_state.applied_remove_tlcs.insert(tlc_id);

        let (tlc_info, remove_reason) = state.remove_tlc_with_reason(tlc_id)?;

        if matches!(remove_reason, RemoveTlcReason::RemoveTlcFulfill(_)) {
            if self.store.get_invoice(&tlc_info.payment_hash).is_some() {
                self.store
                    .update_invoice_status(&tlc_info.payment_hash, CkbInvoiceStatus::Paid)
                    .expect("update invoice status failed");
            }
            // when a hop is a forwarding hop, we need to keep preimage after relay RemoveTlc finished
            // incase watchtower may need preimage to settledown
            if tlc_info.previous_tlc.is_none() {
                self.remove_preimage(tlc_info.payment_hash);
            }
        }

        if let (
            Some(ref udt_type_script),
            RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
        ) = (state.funding_udt_type_script.clone(), &remove_reason)
        {
            let mut tlc_notify_info: TlcNotifyInfo = tlc_info.clone().into();
            tlc_notify_info.payment_preimage = Some(*payment_preimage);
            self.subscribers
                .settled_tlcs_subscribers
                .send(TlcNotification {
                    tlc: tlc_notify_info,
                    channel_id,
                    script: udt_type_script.clone(),
                });
        }
        if tlc_info.previous_tlc.is_none() {
            // only the original sender of the TLC should send `TlcRemoveReceived` event
            // because only the original sender cares about the TLC event to settle the payment
            if tlc_info.is_offered() {
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::TlcRemoveReceived(
                            tlc_info.payment_hash,
                            tlc_info.attempt_id,
                            remove_reason.clone(),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        } else {
            // relay RemoveTlc to previous channel if needed
            self.try_to_relay_remove_tlc(&tlc_info, remove_reason).await;
        }
        Ok(())
    }

    fn remove_preimage(&self, payment_hash: Hash256) {
        self.store.remove_preimage(&payment_hash);
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::PreimageRemoved(payment_hash),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    pub async fn handle_commitment_signed_command(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
    ) -> ProcessingChannelResult {
        let flags = match state.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABORATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, as collaboration is not completed yet.",
                    &state.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                CommitmentSignedFlags::SigningCommitment(SigningCommitmentFlags::empty())
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, as we have already sent our commitment_signed message.",
                    &state.state
                )));
            }
            ChannelState::SigningCommitment(flags) => {
                CommitmentSignedFlags::SigningCommitment(flags)
            }
            ChannelState::ChannelReady => CommitmentSignedFlags::ChannelReady(),
            ChannelState::ShuttingDown(flags) => {
                warn!(
                    "Received commitment_signed command in ShuttingDown state: {:?}",
                    flags
                );
                if flags.is_ok_for_commitment_operation() {
                    CommitmentSignedFlags::PendingShutdown()
                } else {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Unable to process commitment_signed command in shutdowning state with flags {:?}",
                        &flags
                    )));
                }
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &state.state
                )));
            }
        };
        state.clean_up_failed_tlcs();

        let (funding_tx_partial_signature, commitment_tx_partial_signature) =
            state.build_and_sign_commitment_tx()?;
        let commitment_signed = CommitmentSigned {
            channel_id: state.get_id(),
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
            next_commitment_nonce: state.get_next_commitment_nonce(),
        };

        #[cfg(debug_assertions)]
        debug!(
            "send commitment signed: {:?} at commitment_numbers: {:?}",
            commitment_signed,
            state.get_current_commitment_numbers()
        );

        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    state.get_remote_peer_id(),
                    FiberMessage::commitment_signed(commitment_signed),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT;
                state.update_state(ChannelState::SigningCommitment(flags));
                state.maybe_transfer_to_tx_signatures(flags)?;
            }
            CommitmentSignedFlags::ChannelReady() => {
                state.set_waiting_ack(myself, true);
            }
            CommitmentSignedFlags::PendingShutdown() => {
                state.set_waiting_ack(myself, true);
                state.maybe_transfer_to_shutdown().await?;
            }
        }
        Ok(())
    }

    pub async fn handle_add_tlc_command(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        command: &AddTlcCommand,
    ) -> Result<u64, ProcessingChannelError> {
        state.check_for_tlc_update(Some(command.amount), true, true)?;
        state.check_tlc_expiry(command.expiry)?;
        state.check_tlc_forward_amount(
            command.amount,
            command.previous_tlc.map(|x| x.forwarding_fee),
        )?;
        let tlc = state.create_outbounding_tlc(command);
        state.check_insert_tlc(&tlc)?;
        self.check_add_tlc_consistent(command, state)?;

        let tlc_id = tlc.tlc_id;
        state.tlc_state.add_offered_tlc(tlc);
        state.increment_next_offered_tlc_id();

        let add_tlc = AddTlc {
            channel_id: state.get_id(),
            tlc_id: tlc_id.into(),
            amount: command.amount,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            hash_algorithm: command.hash_algorithm,
            onion_packet: command.onion_packet.clone(),
        };

        // Send tlc update message to peer.
        let msg = FiberMessageWithPeerId::new(
            state.get_remote_peer_id(),
            FiberMessage::add_tlc(add_tlc.clone()),
        );

        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(msg),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        self.handle_commitment_signed_command(myself, state).await?;
        Ok(tlc_id.into())
    }

    pub async fn handle_remove_tlc_command(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        command: RemoveTlcCommand,
    ) -> ProcessingChannelResult {
        state.check_for_tlc_update(None, true, false)?;
        state.check_remove_tlc_with_reason(TLCId::Received(command.id), &command.reason)?;
        let payment_hash = state
            .tlc_state
            .set_received_tlc_removed(command.id, command.reason.clone());
        if let RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }) =
            command.reason
        {
            self.store_preimage(payment_hash, payment_preimage);
        }
        let msg = FiberMessageWithPeerId::new(
            state.get_remote_peer_id(),
            FiberMessage::remove_tlc(RemoveTlc {
                channel_id: state.get_id(),
                tlc_id: command.id,
                reason: command.reason,
            }),
        );
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(msg),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        state.maybe_transfer_to_shutdown().await?;
        self.handle_commitment_signed_command(myself, state).await?;
        Ok(())
    }

    pub async fn handle_shutdown_command(
        &self,
        state: &mut ChannelActorState,
        command: ShutdownCommand,
    ) -> ProcessingChannelResult {
        debug!("Handling shutdown command: {:?}", &command);
        #[cfg(debug_assertions)]
        state.tlc_state.debug();
        if command.force {
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
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Handling force shutdown command invalid state {:?}",
                        &state.state
                    )));
                }
            };

            let transaction = state.get_latest_commitment_transaction().await?;

            self.network
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::ClosingTransactionPending(
                        state.get_id(),
                        self.get_remote_peer_id(),
                        transaction,
                        true,
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

            state.update_state(ChannelState::ShuttingDown(
                ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
            ));
            return Ok(());
        } else {
            let flags = match state.state {
                ChannelState::ChannelReady => {
                    debug!("Handling shutdown command in ChannelReady state");
                    if state.tlc_state.all_tlcs().any(|tlc| {
                        matches!(
                            tlc.status,
                            TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced)
                        )
                    }) {
                        return Err(ProcessingChannelError::InvalidState(
                            "Unable to process shutdown command, as there are pending outbound tlcs"
                                .to_string(),
                        ));
                    }
                    ShuttingDownFlags::empty()
                }
                _ => {
                    debug!("Handling shutdown command in state {:?}", &state.state);
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Trying to send shutdown message while in invalid state {:?}",
                        &state.state
                    )));
                }
            };

            let shutdown_fee_rate = command
                .fee_rate
                .unwrap_or(FeeRate::from_u64(state.commitment_fee_rate));
            let close_script = command
                .close_script
                .clone()
                .unwrap_or(state.get_local_shutdown_script());

            state.check_shutdown_fee_rate(shutdown_fee_rate, &close_script)?;
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                        self.get_remote_peer_id(),
                        FiberMessage::shutdown(Shutdown {
                            channel_id: state.get_id(),
                            close_script: close_script.clone(),
                            fee_rate: shutdown_fee_rate,
                        }),
                    )),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

            state.local_shutdown_info = Some(ShutdownInfo {
                close_script,
                fee_rate: shutdown_fee_rate.as_u64(),
                signature: None,
            });
            state.update_state(ChannelState::ShuttingDown(
                flags | ShuttingDownFlags::OUR_SHUTDOWN_SENT,
            ));

            debug!(
                "Channel state updated to {:?} after processing shutdown command",
                &state.state
            );
            state.maybe_transfer_to_shutdown().await
        }
    }

    pub async fn handle_update_command(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        command: UpdateCommand,
    ) -> ProcessingChannelResult {
        if !state.is_public() {
            return Err(ProcessingChannelError::InvalidState(
                "Only public channel can be updated".to_string(),
            ));
        }

        let UpdateCommand {
            enabled,
            tlc_expiry_delta,
            tlc_minimum_value,
            tlc_fee_proportional_millionths,
        } = command;

        let mut updated = false;

        if let Some(enabled) = enabled {
            updated |= state.update_our_enabled(enabled);
        }

        if let Some(delta) = tlc_expiry_delta {
            check_tlc_delta_with_epochs(delta, state.commitment_delay_epoch)?;
            updated |= state.update_our_tlc_expiry_delta(delta);
        }

        if let Some(value) = tlc_minimum_value {
            updated |= state.update_our_tlc_min_value(value);
        }

        if let Some(fee) = tlc_fee_proportional_millionths {
            updated |= state.update_our_tlc_fee_proportional_millionths(fee);
        }

        if updated {
            state.on_owned_channel_updated(myself, true);
        }

        Ok(())
    }

    pub fn register_retryable_tlc_remove(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        tlc_id: TLCId,
        reason: RemoveTlcReason,
    ) {
        let remove_tlc = RetryableTlcOperation::RemoveTlc(tlc_id, reason);
        self.register_retryable_tlc_operation(myself, state, remove_tlc);
    }

    pub fn register_retryable_tlc_add(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        add_tlc_command: AddTlcCommand,
    ) {
        let add_tlc = RetryableTlcOperation::AddTlc(add_tlc_command);
        self.register_retryable_tlc_operation(myself, state, add_tlc);
    }

    fn register_retryable_tlc_operation(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        operation: RetryableTlcOperation,
    ) {
        state.retryable_tlc_operations.push_back(operation);
        if state.retryable_tlc_operations.len() == 1 {
            // if there are already some retryable tasks in queue, we don't need to trigger again
            // keep the existing schedule event message so that we don't send too many ractor messages
            // which may introduce perform regression
            state.schedule_next_retry_task(myself);
        }
    }

    pub fn register_and_apply_forward_tlc(
        &self,
        state: &mut ChannelActorState,
        payment_hash: Hash256,
        tlc_id: TLCId,
        peeled_onion_packet: PeeledPaymentOnionPacket,
        forward_fee: u128,
    ) {
        match self.network.send_message(NetworkActorMessage::Command(
            NetworkActorCommand::SendPaymentOnionPacket(SendOnionPacketCommand {
                peeled_onion_packet: peeled_onion_packet.clone(),
                previous_tlc: Some(PrevTlcInfo::new(
                    state.get_id(),
                    u64::from(tlc_id),
                    forward_fee,
                )),
                payment_hash,
                // forward tlc always set attempt_id to None
                attempt_id: None,
            }),
        )) {
            Ok(_) => {
                // we successfully sent the forward tlc, we will wait for the result
                // here we just make sure the forward tlc is sent, we don't need to wait for the result
                state.waiting_forward_tlc_tasks.insert(
                    (payment_hash, tlc_id),
                    ForwardTlc(payment_hash, tlc_id, peeled_onion_packet, forward_fee),
                );
            }
            Err(err) => {
                error!("Failed to send forward tlc onion packet command: {:?}", err);
            }
        }
    }

    pub fn register_retryable_relay_tlc_remove(
        &self,
        tlc_id: TLCId,
        channel_id: Hash256,
        reason: RemoveTlcReason,
    ) -> Result<(), ActorProcessingErr> {
        let prev_channel_state = self
            .store
            .get_channel_actor_state(&channel_id)
            .expect("channel state not found");
        let tlc_info = prev_channel_state.tlc_state.get(&tlc_id);
        if tlc_info.is_none_or(|tlc| tlc.removed_reason.is_some()) {
            // the tlc has been removed, we can remove the operation
        } else {
            let channel_status = prev_channel_state.state;
            if matches!(
                channel_status,
                ChannelState::ChannelReady | ChannelState::ShuttingDown(_)
            ) {
                // send relay remove tlc with network actor to previous hop
                // don't wait reply here, if previous hop is not reachable,
                // network actor will add retry task to it's ChannelActorState,
                // if previous hop is in WaitingTlcAck, it will also retry later
                let (send, _recv) = oneshot::channel::<Result<(), ProcessingChannelError>>();
                let port = RpcReplyPort::from(send);
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                            channel_id,
                            command: ChannelCommand::RemoveTlc(
                                RemoveTlcCommand {
                                    id: tlc_id.into(),
                                    reason: reason.clone(),
                                },
                                port,
                            ),
                        }),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        }
        Ok(())
    }

    pub async fn apply_retryable_tlc_operations(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        trigger_next: bool,
    ) {
        loop {
            if state.is_waiting_tlc_ack() {
                break;
            }

            let Some(operation) = state.retryable_tlc_operations.pop_front() else {
                return;
            };

            let success = match operation {
                RetryableTlcOperation::RemoveTlc(tlc_id, reason) => self
                    .handle_remove_tlc_command(
                        myself,
                        state,
                        RemoveTlcCommand {
                            id: u64::from(tlc_id),
                            reason,
                        },
                    )
                    .await
                    .is_ok(),
                RetryableTlcOperation::AddTlc(command) => {
                    let res = self.handle_add_tlc_command(myself, state, &command).await;
                    self.post_add_tlc_command(myself, state, command, &res);
                    res.is_ok()
                }
            };

            if success {
                break;
            }
        }

        if trigger_next {
            state.schedule_next_retry_task(myself);
        }
    }

    fn handle_forward_tlc_result(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        result: ForwardTlcResult,
    ) {
        let key = (result.payment_hash, TLCId::Received(result.tlc_id));
        let Some(ForwardTlc(_, _, onion_packet, _)) = state.waiting_forward_tlc_tasks.remove(&key)
        else {
            return;
        };
        if let Some((channel_err, tlc_err)) = result.error_info {
            match channel_err {
                ProcessingChannelError::WaitingTlcAck => {
                    // peer already buffered the tlc, we already removed the forward tlc record
                    // and just ignore the error here
                }
                _ => {
                    let error = ProcessingChannelError::TlcForwardingError(tlc_err)
                        .with_shared_secret(onion_packet.shared_secret);
                    self.process_add_tlc_error(
                        myself,
                        state,
                        result.payment_hash,
                        TLCId::Received(result.tlc_id),
                        error,
                    );
                }
            }
        }
    }

    // This is the dual of `handle_tx_collaboration_msg`. Any logic error here is likely
    // to present in the other function as well.
    pub fn handle_tx_collaboration_command(
        &self,
        state: &mut ChannelActorState,
        command: TxCollaborationCommand,
    ) -> ProcessingChannelResult {
        debug!("Handling tx collaboration command: {:?}", &command);
        let is_complete_command = matches!(command, TxCollaborationCommand::TxComplete());
        let is_waiting_for_remote = match state.state {
            ChannelState::CollaboratingFundingTx(flags) => {
                flags.contains(CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG)
            }
            _ => false,
        };

        // We first exclude below cases that are invalid for tx collaboration,
        // and then process the commands.
        let flags = match state.state {
            ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT)
                if state.is_acceptor =>
            {
                return Err(ProcessingChannelError::InvalidState(
                    "Acceptor tries to start sending tx collaboration message".to_string(),
                ));
            }
            ChannelState::NegotiatingFunding(_) => {
                debug!("Beginning processing tx collaboration command, and transitioning from {:?} to CollaboratingFundingTx state", state.state);
                state.state =
                    ChannelState::CollaboratingFundingTx(CollaboratingFundingTxFlags::empty());
                CollaboratingFundingTxFlags::empty()
            }
            ChannelState::CollaboratingFundingTx(_)
                if !is_complete_command && is_waiting_for_remote =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Trying to process command {:?} while in {:?} (should only send non-complete message after received response from peer)",
                    &command, state.state
                )));
            }
            ChannelState::CollaboratingFundingTx(flags) => {
                debug!(
                    "Processing tx collaboration command {:?} for state {:?}",
                    &command, &state.state
                );
                flags
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid tx collaboration command {:?} for state {:?}",
                    &command, state.state
                )));
            }
        };

        // TODO: Note that we may deadlock here if send_tx_collaboration_command does successfully send the message,
        // as in that case both us and the remote are waiting for each other to send the message.
        match command {
            TxCollaborationCommand::TxUpdate(tx_update) => {
                let fiber_message = FiberMessage::tx_update(TxUpdate {
                    channel_id: state.get_id(),
                    tx: tx_update.transaction.clone(),
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            state.get_remote_peer_id(),
                            fiber_message,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG,
                ));
                state.funding_tx = Some(tx_update.transaction.clone());
                state.maybe_complete_tx_collaboration(tx_update.transaction)?;
            }
            TxCollaborationCommand::TxComplete() => {
                state.check_tx_complete_preconditions()?;
                let commitment_tx_partial_signature = state.build_init_commitment_tx_signature()?;
                let fiber_message = FiberMessage::tx_complete(TxComplete {
                    channel_id: state.get_id(),
                    next_commitment_nonce: state.get_next_commitment_nonce(),
                    commitment_tx_partial_signature,
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            state.get_remote_peer_id(),
                            fiber_message,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::CollaboratingFundingTx(
                    flags | CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT,
                ));
            }
        }

        Ok(())
    }

    pub async fn handle_command(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        command: ChannelCommand,
    ) -> ProcessingChannelResult {
        match command {
            ChannelCommand::TxCollaborationCommand(tx_collaboration_command) => {
                self.handle_tx_collaboration_command(state, tx_collaboration_command)
            }
            ChannelCommand::FundingTxSigned(tx) => {
                if let ChannelState::AwaitingTxSignatures(flags) = state.state {
                    let flags = flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT;
                    state.funding_tx = Some(tx);
                    state.update_state(ChannelState::AwaitingTxSignatures(flags));
                } else {
                    error!("Invalid state. Expect channel state to be AwaitingTxSignatures, but bot {:?}", state.state);
                }
                Ok(())
            }
            ChannelCommand::CommitmentSigned() => {
                self.handle_commitment_signed_command(myself, state).await
            }
            ChannelCommand::AddTlc(command, reply) => {
                let res = self.handle_add_tlc_command(myself, state, &command).await;

                self.post_add_tlc_command(myself, state, command, &res);
                match res {
                    Ok(tlc_id) => {
                        let _ = reply.send(Ok(AddTlcResponse { tlc_id }));
                        Ok(())
                    }
                    Err(err) => {
                        let tlc_err = self.get_tlc_error(state, &err);
                        let _ = reply.send(Err(tlc_err));
                        Err(err)
                    }
                }
            }
            ChannelCommand::RemoveTlc(command, reply) => {
                match self
                    .handle_remove_tlc_command(myself, state, command.clone())
                    .await
                {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        if matches!(err, ProcessingChannelError::WaitingTlcAck) {
                            self.register_retryable_tlc_remove(
                                myself,
                                state,
                                TLCId::Received(command.id),
                                command.reason,
                            );
                        }
                        let _ = reply.send(Err(err.clone()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::Shutdown(command, reply) => {
                match self.handle_shutdown_command(state, command).await {
                    Ok(_) => {
                        debug!("Shutdown command processed successfully");
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        debug!("Error processing shutdown command: {:?}", &err);
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::Update(command, reply) => {
                match self.handle_update_command(myself, state, command).await {
                    Ok(_) => {
                        debug!("Update command processed successfully");
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        debug!("Error processing update command: {:?}", &err);
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::BroadcastChannelUpdate() => {
                state.broadcast_channel_update(myself);
                Ok(())
            }
            ChannelCommand::NotifyEvent(event) => self.handle_event(myself, state, event).await,
            #[cfg(any(test, feature = "bench"))]
            ChannelCommand::ReloadState(reload_params) => {
                let private_key = state.private_key.clone();
                *state = self
                    .store
                    .get_channel_actor_state(&state.get_id())
                    .expect("load channel state failed");
                state.network = Some(self.network.clone());
                state.private_key = private_key.clone();
                let ReloadParams { notify_changes } = reload_params;
                if notify_changes {
                    state.on_owned_channel_updated(myself, false);
                }
                Ok(())
            }
        }
    }

    pub async fn handle_event(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        event: ChannelEvent,
    ) -> ProcessingChannelResult {
        match event {
            ChannelEvent::FundingTransactionConfirmed(block_hash, tx_index, timestamp) => {
                debug!("Funding transaction confirmed");
                let flags = match state.state {
                    ChannelState::AwaitingChannelReady(flags) => flags,
                    ChannelState::AwaitingTxSignatures(f)
                        if f.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) =>
                    {
                        AwaitingChannelReadyFlags::empty()
                    }
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "Expecting funding transaction confirmed event in state AwaitingChannelReady or after TX_SIGNATURES_SENT, but got state {:?}", &state.state)));
                    }
                };
                state.funding_tx_confirmed_at = Some((block_hash, tx_index, timestamp));
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            state.get_remote_peer_id(),
                            FiberMessage::channel_ready(ChannelReady {
                                channel_id: state.get_id(),
                            }),
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                let flags = flags | AwaitingChannelReadyFlags::OUR_CHANNEL_READY;
                state.update_state(ChannelState::AwaitingChannelReady(flags));
                state.maybe_channel_is_ready(myself);
            }
            ChannelEvent::RunRetryTask => {
                self.apply_retryable_tlc_operations(myself, state, true)
                    .await;
            }
            ChannelEvent::ForwardTlcResult(result) => {
                self.handle_forward_tlc_result(myself, state, result);
            }
            ChannelEvent::Stop(reason) => {
                debug_event!(self.network, "ChannelActorStopped");
                if reason == StopReason::Abandon {
                    state.update_state(ChannelState::Closed(CloseFlags::ABANDONED));
                } else if reason == StopReason::AbortFunding {
                    state.update_state(ChannelState::Closed(CloseFlags::FUNDING_ABORTED));
                    let abort_message = FiberMessageWithPeerId {
                        peer_id: state.get_remote_peer_id(),
                        message: FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::TxAbort(TxAbort {
                                channel_id: state.get_id(),
                                message: "funding aborted".as_bytes().to_vec(),
                            }),
                        ),
                    };
                    state
                        .network()
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(abort_message),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
                myself.stop(Some(format!("ChannelStopped: {:?}", reason)));
            }
            ChannelEvent::ClosingTransactionConfirmed(tx_hash, force, close_by_us) => {
                state
                    .update_close_transaction_confirmed(tx_hash, force, close_by_us)
                    .await?;
                debug_event!(self.network, "ChannelClosed");
                myself.stop(Some("ChannelClosed".to_string()));
            }
            ChannelEvent::CheckActiveChannel => {
                if state.should_disconnect_peer_awaiting_response() && !state.is_closed() {
                    error!(
                        "Channel {} from peer {:?} is inactive for a time, shutting down it forcefully",
                        state.get_id(),
                        state.get_remote_peer_id(),
                    );
                    self.notify_network_actor_shutdown_me(state);
                }
            }
            ChannelEvent::CheckFundingTimeout => {
                if state.can_abort_funding_on_timeout() {
                    info!("Abort funding on timeout for channel {}", state.get_id());
                    myself
                        .send_message(ChannelActorMessage::Event(ChannelEvent::Stop(
                            StopReason::AbortFunding,
                        )))
                        .expect("myself alive");
                }
            }
        }
        Ok(())
    }

    fn post_add_tlc_command(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        command: AddTlcCommand,
        res: &Result<u64, ProcessingChannelError>,
    ) {
        match res {
            Err(ProcessingChannelError::WaitingTlcAck) if command.onion_packet.is_some() => {
                // buffer the tlc add command to retry later
                self.register_retryable_tlc_add(myself, state, command);
            }
            _ => {
                let notify = res
                    .as_ref()
                    .err()
                    .map(|err| (err.clone(), self.get_tlc_error(state, err)));

                // notify the network actor about the add tlc result
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::AddTlcResult(
                            command.payment_hash,
                            command.attempt_id,
                            notify,
                            command.previous_tlc,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        }
    }

    fn get_invoice_status(&self, invoice: &CkbInvoice) -> CkbInvoiceStatus {
        match self
            .store
            .get_invoice_status(invoice.payment_hash())
            .expect("no invoice status found")
        {
            CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
            status => status,
        }
    }

    fn check_add_tlc_consistent(
        &self,
        command: &AddTlcCommand,
        current_state: &ChannelActorState,
    ) -> ProcessingChannelResult {
        if let Some(prev_tlc_info) = command.previous_tlc {
            let prev_state = self
                .store
                .get_channel_actor_state(&prev_tlc_info.prev_channel_id)
                .expect("prev_state error");
            if prev_state.funding_udt_type_script != current_state.funding_udt_type_script {
                return Err(ProcessingChannelError::InvalidParameter(
                    "Previous forwarding channel is with different UDT type".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn notify_network_actor_shutdown_me(&self, state: &ChannelActorState) {
        let (send, _recv) = oneshot::channel();
        let rpc_reply = RpcReplyPort::from(send);
        state
            .network()
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                    channel_id: state.get_id(),
                    command: ChannelCommand::Shutdown(
                        ShutdownCommand {
                            close_script: None,
                            fee_rate: None,
                            force: true,
                        },
                        rpc_reply,
                    ),
                }),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<S> Actor for ChannelActor<S>
where
    S: ChannelActorStateStore + InvoiceStore + PreimageStore + Send + Sync + 'static,
{
    type Msg = ChannelActorMessage;
    type State = ChannelActorState;
    type Arguments = ChannelInitializationParameter;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // startup the event processing
        let mut state = match args.operation {
            ChannelInitializationOperation::AcceptChannel(AcceptChannelParameter {
                funding_amount: local_funding_amount,
                reserved_ckb_amount: local_reserved_ckb_amount,
                shutdown_script: local_shutdown_script,
                tlc_info,
                public_channel_info,
                seed,
                open_channel,
                channel_id_sender,
                max_tlc_number_in_flight,
                max_tlc_value_in_flight,
            }) => {
                let peer_id = self.get_remote_peer_id();
                debug!(
                    "Accepting channel {:?} to peer {:?}",
                    &open_channel, &peer_id
                );

                let counterpart_pubkeys = (&open_channel).into();
                let public = open_channel.is_public();
                let OpenChannel {
                    channel_id,
                    chain_hash,
                    commitment_fee_rate,
                    commitment_delay_epoch,
                    funding_fee_rate,
                    funding_udt_type_script,
                    funding_amount,
                    shutdown_script,
                    reserved_ckb_amount,
                    first_per_commitment_point,
                    second_per_commitment_point,
                    next_commitment_nonce,
                    next_revocation_nonce,
                    max_tlc_value_in_flight: remote_max_tlc_value_in_flight,
                    max_tlc_number_in_flight: remote_max_tlc_number_in_flight,
                    channel_announcement_nonce,
                    ..
                } = &open_channel;

                if *chain_hash != get_chain_hash() {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(format!(
                        "Invalid chain hash {:?}",
                        chain_hash
                    ))));
                }

                // TODO: we may reject the channel opening request here
                // if the peer want to open a public channel, but we don't want to.
                if public && (channel_announcement_nonce.is_none() || public_channel_info.is_none())
                {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(
                        "Public channel should have channel announcement nonce and public channel info".to_string(),
                    )));
                }

                if !public
                    && (channel_announcement_nonce.is_some() || public_channel_info.is_some())
                {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(
                        "Non-public channel should not have channel announcement nonce and public channel info".to_string(),
                    )));
                }

                let mut state = ChannelActorState::new_inbound_channel(
                    *channel_id,
                    public_channel_info,
                    local_funding_amount,
                    local_reserved_ckb_amount,
                    *commitment_fee_rate,
                    *commitment_delay_epoch,
                    *funding_fee_rate,
                    funding_udt_type_script.clone(),
                    &seed,
                    self.get_local_pubkey(),
                    self.get_remote_pubkey(),
                    local_shutdown_script.clone(),
                    shutdown_script.clone(),
                    *funding_amount,
                    *reserved_ckb_amount,
                    counterpart_pubkeys,
                    next_commitment_nonce.clone(),
                    next_revocation_nonce.clone(),
                    channel_announcement_nonce.clone(),
                    *first_per_commitment_point,
                    *second_per_commitment_point,
                    *remote_max_tlc_value_in_flight,
                    *remote_max_tlc_number_in_flight,
                    max_tlc_number_in_flight,
                    max_tlc_value_in_flight,
                    tlc_info,
                    self.network.clone(),
                    args.private_key.clone(),
                );
                state.check_accept_channel_parameters()?;

                let channel_announcement_nonce = if public {
                    Some(state.get_channel_announcement_musig2_pubnonce())
                } else {
                    None
                };

                let commitment_number = 1; // The first commitment number is 1, as 0 is reserved for the initial state.
                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: local_funding_amount,
                    shutdown_script: local_shutdown_script,
                    reserved_ckb_amount: local_reserved_ckb_amount,
                    max_tlc_value_in_flight,
                    max_tlc_number_in_flight,
                    funding_pubkey: state.signer.funding_key.pubkey(),
                    tlc_basepoint: state.signer.tlc_base_key.pubkey(),
                    first_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    channel_announcement_nonce,
                    next_commitment_nonce: state.get_commitment_nonce(),
                    next_revocation_nonce: state.get_init_revocation_nonce(),
                };

                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            peer_id,
                            FiberMessage::accept_channel(accept_channel),
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::NegotiatingFunding(
                    NegotiatingFundingFlags::INIT_SENT,
                ));
                if let Some(sender) = channel_id_sender {
                    sender.send(state.get_id()).expect("Receive not dropped");
                }
                state
            }
            ChannelInitializationOperation::OpenChannel(OpenChannelParameter {
                funding_amount,
                seed,
                tlc_info,
                public_channel_info,
                funding_udt_type_script,
                shutdown_script,
                channel_id_sender,
                commitment_fee_rate,
                commitment_delay_epoch,
                funding_fee_rate,
                max_tlc_number_in_flight,
                max_tlc_value_in_flight,
            }) => {
                let public = public_channel_info.is_some();
                let peer_id = self.get_remote_peer_id();
                info!("Trying to open a channel to {:?}", &peer_id);

                let commitment_fee_rate =
                    commitment_fee_rate.unwrap_or(DEFAULT_COMMITMENT_FEE_RATE);
                let funding_fee_rate = funding_fee_rate.unwrap_or(DEFAULT_FEE_RATE);

                let (to_local_amount, reserved_ckb_amount) = get_funding_and_reserved_amount(
                    funding_amount,
                    &shutdown_script,
                    &funding_udt_type_script,
                )?;

                let mut channel = ChannelActorState::new_outbound_channel(
                    public_channel_info,
                    &seed,
                    self.get_local_pubkey(),
                    self.get_remote_pubkey(),
                    to_local_amount,
                    reserved_ckb_amount,
                    commitment_fee_rate,
                    commitment_delay_epoch
                        .unwrap_or(EpochNumberWithFraction::new(
                            DEFAULT_COMMITMENT_DELAY_EPOCHS,
                            0,
                            1,
                        ))
                        .full_value(),
                    funding_fee_rate,
                    funding_udt_type_script.clone(),
                    shutdown_script.clone(),
                    max_tlc_value_in_flight,
                    max_tlc_number_in_flight,
                    tlc_info,
                    self.network.clone(),
                    args.private_key.clone(),
                );

                check_open_channel_parameters(
                    &channel.funding_udt_type_script,
                    &channel.local_shutdown_script,
                    channel.local_reserved_ckb_amount,
                    channel.funding_fee_rate,
                    channel.commitment_fee_rate,
                    channel.commitment_delay_epoch,
                    channel.local_constraints.max_tlc_number_in_flight,
                )?;

                let channel_flags = if public {
                    ChannelFlags::PUBLIC
                } else {
                    ChannelFlags::empty()
                };
                let channel_announcement_nonce = if public {
                    Some(channel.get_channel_announcement_musig2_pubnonce())
                } else {
                    None
                };
                let commitment_number = 1; // The first commitment number is 1, as 0 is reserved for the initial state.
                let message = FiberMessage::ChannelInitialization(OpenChannel {
                    chain_hash: get_chain_hash(),
                    channel_id: channel.get_id(),
                    funding_udt_type_script,
                    funding_amount: channel.to_local_amount,
                    shutdown_script,
                    reserved_ckb_amount: channel.local_reserved_ckb_amount,
                    funding_fee_rate,
                    commitment_fee_rate,
                    commitment_delay_epoch: channel.commitment_delay_epoch,
                    max_tlc_value_in_flight: channel.local_constraints.max_tlc_value_in_flight,
                    max_tlc_number_in_flight: channel.local_constraints.max_tlc_number_in_flight,
                    channel_flags,
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    funding_pubkey: channel.get_local_channel_public_keys().funding_pubkey,
                    tlc_basepoint: channel.get_local_channel_public_keys().tlc_base_key,
                    next_commitment_nonce: channel.get_commitment_nonce(),
                    next_revocation_nonce: channel.get_init_revocation_nonce(),
                    channel_announcement_nonce,
                });

                debug!(
                    "Created OpenChannel message to {:?}: {:?}",
                    &peer_id, &message
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message,
                        }),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                // TODO: note that we can't actually guarantee that this OpenChannel message is sent here.
                // It is even possible that the peer_id is bogus, and we can't send a message to it.
                // We need some book-keeping service to remove all the OUR_INIT_SENT channels.
                channel.update_state(ChannelState::NegotiatingFunding(
                    NegotiatingFundingFlags::OUR_INIT_SENT,
                ));
                debug!(
                    "Channel to peer {:?} with id {:?} created",
                    &peer_id,
                    &channel.get_id()
                );

                channel_id_sender
                    .send(channel.get_id())
                    .expect("Receive not dropped");
                channel
            }
            ChannelInitializationOperation::ReestablishChannel(channel_id) => {
                let mut channel = self
                    .store
                    .get_channel_actor_state(&channel_id)
                    .expect("channel should exist");
                channel.reestablishing = true;
                channel.network = Some(self.network.clone());
                channel.private_key = Some(args.private_key.clone());

                let reestablish_channel = ReestablishChannel {
                    channel_id,
                    local_commitment_number: channel.get_local_commitment_number(),
                    remote_commitment_number: channel.get_remote_commitment_number(),
                };

                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            self.get_remote_peer_id(),
                            FiberMessage::reestablish_channel(reestablish_channel),
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                channel
            }
        };

        state.ephemeral_config = args.ephemeral_config;
        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        trace!(
            "Channel actor processing message: peer: {:?} id: {:?}, state: {:?}, message: {:?}",
            state.get_local_peer_id(),
            &state.get_id(),
            &state.state,
            message,
        );

        match message {
            ChannelActorMessage::PeerMessage(message) => {
                if let Err(error) = self
                    .handle_peer_message(&myself, state, message.clone())
                    .await
                {
                    error!(
                        "{:?} Error while processing channel message: {:?} with message: {:?}",
                        state.get_local_peer_id(),
                        error,
                        message
                    );
                    debug_event!(&self.network, &format!("{:?}", error));
                }
            }
            ChannelActorMessage::Event(e) => {
                if let Err(err) = self.handle_event(&myself, state, e).await {
                    error!("Error while processing channel event: {:?}", err);
                }
            }
            ChannelActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(&myself, state, command).await {
                    if !matches!(err, ProcessingChannelError::WaitingTlcAck) {
                        error!(
                            "{:?} Error while processing channel command: {:?}",
                            state.get_local_peer_id(),
                            err,
                        );
                    }
                }
            }
        }

        // take the pending settlement tlc set
        let pending_notify_mpp_tcls = std::mem::take(&mut state.pending_notify_mpp_tlcs);

        self.store.insert_channel_actor_state(state.clone());

        // try to settle down tlc set
        for (payment_hash, tlc_id) in pending_notify_mpp_tcls {
            let channel_id = state.get_id();
            // hold the tlc
            self.store.insert_payment_hold_tlc(
                payment_hash,
                HoldTlc {
                    channel_id,
                    tlc_id,
                    hold_expire_at: now_timestamp_as_millis_u64() + DEFAULT_HOLD_TLC_TIMEOUT,
                },
            );

            // set timeout for hold tlc
            self.network
                .send_after(Duration::from_millis(DEFAULT_HOLD_TLC_TIMEOUT), move || {
                    NetworkActorMessage::new_command(NetworkActorCommand::TimeoutHoldTlc(
                        payment_hash,
                        channel_id,
                        tlc_id,
                    ))
                });

            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SettleMPPTlcSet(payment_hash),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }

        Ok(())
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // handle funding timeout
        if state.can_abort_funding_on_timeout() {
            let event_factory = || ChannelActorMessage::Event(ChannelEvent::CheckFundingTimeout);

            match Duration::from_secs(DEFAULT_FUNDING_TIMEOUT_SECONDS)
                .checked_sub(state.created_at.elapsed().unwrap_or_default())
            {
                Some(timeout) => {
                    // timeout in future
                    myself.send_after(timeout, event_factory);
                }
                None => {
                    // already timeout
                    myself.send_message(event_factory()).expect("myself alive");
                }
            }
        }

        // handle funding timeout
        if state.can_abort_funding_on_timeout() {
            let event_factory = || ChannelActorMessage::Event(ChannelEvent::CheckFundingTimeout);

            match Duration::from_secs(state.ephemeral_config.funding_timeout_seconds)
                .checked_sub(state.created_at.elapsed().unwrap_or_default())
            {
                Some(timeout) => {
                    // timeout in future
                    myself.send_after(timeout, event_factory);
                }
                None => {
                    // already timeout
                    myself.send_message(event_factory()).expect("myself alive");
                }
            }
        }

        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // ignore send message failure here, because network actor may already be stopped.
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            let _ = self.network.send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::OwnedChannelUpdateEvent(
                    super::graph::OwnedChannelUpdateEvent::Down(outpoint),
                ),
            ));
        }
        let stop_reason = match state.state {
            ChannelState::Closed(flags) => match flags {
                CloseFlags::ABANDONED => StopReason::Abandon,
                CloseFlags::FUNDING_ABORTED => StopReason::AbortFunding,
                _ => StopReason::Closed,
            },
            _ => StopReason::PeerDisConnected,
        };
        debug!(
            "ChannelActor stopped: {:?} with reason: {:?}",
            state.get_id(),
            stop_reason
        );
        let _ = self.network.send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelActorStopped(state.get_id(), stop_reason),
        ));
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentNumbers {
    local: u64,
    remote: u64,
}

impl Default for CommitmentNumbers {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitmentNumbers {
    pub fn new() -> Self {
        Self {
            local: INITIAL_COMMITMENT_NUMBER,
            remote: INITIAL_COMMITMENT_NUMBER,
        }
    }

    pub fn get_local(&self) -> u64 {
        self.local
    }

    pub fn get_remote(&self) -> u64 {
        self.remote
    }

    pub fn increment_local(&mut self) {
        self.local += 1;
    }

    pub fn increment_remote(&mut self) {
        self.remote += 1;
    }
}

/// The id of a tlc, it can be either offered or received.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
pub enum TLCId {
    /// Offered tlc id
    Offered(u64),
    /// Received tlc id
    Received(u64),
}

impl From<TLCId> for u64 {
    fn from(id: TLCId) -> u64 {
        match id {
            TLCId::Offered(id) => id,
            TLCId::Received(id) => id,
        }
    }
}

impl TLCId {
    pub fn is_offered(&self) -> bool {
        matches!(self, TLCId::Offered(_))
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    pub fn flip(&self) -> Self {
        match self {
            TLCId::Offered(id) => TLCId::Received(*id),
            TLCId::Received(id) => TLCId::Offered(*id),
        }
    }

    pub fn flip_mut(&mut self) {
        *self = self.flip();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum OutboundTlcStatus {
    // Offered tlc created and sent to remote party
    LocalAnnounced,
    // Received ACK from remote party for this offered tlc
    Committed,
    // Remote party removed this tlc
    RemoteRemoved,
    // We received another RemoveTlc message from peer when we are waiting for the ack of the last one.
    // So we need another ACK to confirm the removal.
    RemoveWaitPrevAck,
    // We have sent commitment signed to peer and waiting ACK for confirming this RemoveTlc
    RemoveWaitAck,
    // We have received the ACK for the RemoveTlc, it's safe to remove this tlc
    RemoveAckConfirmed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum InboundTlcStatus {
    // Received tlc from remote party, but not committed yet
    RemoteAnnounced,
    // We received another AddTlc peer message when we are waiting for the ack of the last one.
    // So we need another ACK to confirm the addition.
    AnnounceWaitPrevAck,
    // We have sent commitment signed to peer and waiting ACK for confirming this AddTlc
    AnnounceWaitAck,
    // We have received ACK from peer and Committed this tlc
    Committed,
    // We have removed this tlc, but haven't received ACK from peer
    LocalRemoved,
    // We have received the ACK for the RemoveTlc, it's safe to remove this tlc
    RemoveAckConfirmed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum TlcStatus {
    Outbound(OutboundTlcStatus),
    Inbound(InboundTlcStatus),
}

impl TlcStatus {
    pub fn as_outbound_status(&self) -> OutboundTlcStatus {
        match self {
            TlcStatus::Outbound(status) => status.clone(),
            _ => {
                unreachable!("unexpected status")
            }
        }
    }

    pub fn as_inbound_status(&self) -> InboundTlcStatus {
        match self {
            TlcStatus::Inbound(status) => status.clone(),
            _ => {
                unreachable!("unexpected status ")
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct TlcInfo {
    pub channel_id: Hash256,
    pub status: TlcStatus,
    pub tlc_id: TLCId,
    pub amount: u128,
    pub payment_hash: Hash256,
    /// bolt04 total amount of the payment, must exist if payment secret is set
    pub total_amount: Option<u128>,
    /// bolt04 payment secret, only exists for last hop in multi-path payment
    pub payment_secret: Option<Hash256>,
    /// The attempt id associate with the tlc, only on outbound tlc
    /// only exists for first hop in multi-path payment
    pub attempt_id: Option<u64>,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    // the onion packet for multi-hop payment
    pub onion_packet: Option<PaymentOnionPacket>,
    /// Shared secret used in forwarding.
    ///
    /// Save it to backward errors. Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
    pub created_at: CommitmentNumbers,
    pub removed_reason: Option<RemoveTlcReason>,

    /// Note: `previous_tlc` is used to track the tlc chain for a multi-tlc payment,
    ///       we need to know previous when removing tlc backwardly.
    ///
    /// Node A ---------> Node B ------------> Node C ----------> Node D
    ///  tlc_1 <---> (tlc_1) (tlc_2) <---> (tlc_2) (tlc_3) <----> tlc_3
    ///                ^^^^                 ^^^^
    ///
    pub previous_tlc: Option<(Hash256, TLCId)>,
    pub removed_confirmed_at: Option<u64>,
}

// When we are forwarding a TLC, we need to know the previous TLC information.
// This struct keeps the information of the previous TLC.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct PrevTlcInfo {
    pub(crate) prev_channel_id: Hash256,
    // The TLC is always a received TLC because we are forwarding it.
    pub(crate) prev_tlc_id: u64,
    pub(crate) forwarding_fee: u128,
}

impl PrevTlcInfo {
    pub fn new(prev_channel_id: Hash256, prev_tlc_id: u64, forwarding_fee: u128) -> Self {
        Self {
            prev_channel_id,
            prev_tlc_id,
            forwarding_fee,
        }
    }
}

impl Debug for TlcInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlcInfo")
            .field("tlc_id", &self.tlc_id)
            .field("status", &self.status)
            .field("channel_id", &self.channel_id)
            .field("amount", &self.amount)
            .field("total_amount", &self.total_amount)
            .field("removed_reason", &self.removed_reason)
            .field("payment_hash", &self.payment_hash)
            .field("removed_confirmed_at", &self.removed_confirmed_at)
            .finish()
    }
}

impl TlcInfo {
    pub fn log(&self) -> String {
        format!(
            "id: {:?} status: {:?} amount: {:?} removed: {:?} hash: {:?} ",
            &self.tlc_id, self.status, self.amount, self.removed_reason, self.payment_hash,
        )
    }

    pub fn id(&self) -> u64 {
        self.tlc_id.into()
    }

    pub fn is_offered(&self) -> bool {
        self.tlc_id.is_offered()
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    pub fn get_commitment_numbers(&self) -> CommitmentNumbers {
        self.created_at
    }

    pub fn flip_mut(&mut self) {
        self.tlc_id.flip_mut();
    }

    pub fn outbound_status(&self) -> OutboundTlcStatus {
        self.status.as_outbound_status()
    }

    pub fn inbound_status(&self) -> InboundTlcStatus {
        self.status.as_inbound_status()
    }

    pub fn is_fail_remove_confirmed(&self) -> bool {
        matches!(self.removed_reason, Some(RemoveTlcReason::RemoveTlcFail(_)))
            && matches!(
                self.status,
                TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed)
                    | TlcStatus::Outbound(OutboundTlcStatus::RemoveWaitAck)
                    | TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed)
            )
    }

    fn get_hash(&self) -> ShortHash {
        self.payment_hash.as_ref()[..20]
            .try_into()
            .expect("short hash from payment hash")
    }

    /// Get the value for the field `htlc_type` in commitment lock witness.
    /// - Lowest 1 bit: 0 if the tlc is offered by the remote party, 1 otherwise.
    /// - High 7 bits:
    ///     - 0: ckb hash
    ///     - 1: sha256
    pub fn get_htlc_type(&self) -> u8 {
        let offered_flag = if self.is_offered() { 0u8 } else { 1u8 };
        ((self.hash_algorithm as u8) << 1) + offered_flag
    }
}

impl From<TlcInfo> for TlcNotifyInfo {
    fn from(tlc: TlcInfo) -> Self {
        TlcNotifyInfo {
            tlc_id: tlc.tlc_id,
            amount: tlc.amount,
            payment_hash: tlc.payment_hash,
            payment_preimage: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Debug)]
pub enum RetryableTlcOperation {
    RemoveTlc(TLCId, RemoveTlcReason),
    AddTlc(AddTlcCommand),
}

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct RelayRemoveTlc(Hash256, TLCId, RemoveTlcReason);

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ForwardTlc(Hash256, TLCId, PeeledPaymentOnionPacket, u128);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct PendingTlcs {
    pub tlcs: Vec<TlcInfo>,
    pub next_tlc_id: u64,
}

impl PendingTlcs {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut TlcInfo> {
        self.tlcs.iter_mut()
    }

    pub fn get_next_id(&self) -> u64 {
        self.next_tlc_id
    }

    pub fn increment_next_id(&mut self) {
        self.next_tlc_id += 1;
    }

    pub fn add_tlc(&mut self, tlc: TlcInfo) {
        self.tlcs.push(tlc);
    }

    pub fn get_committed_tlcs(&self) -> Vec<TlcInfo> {
        self.tlcs
            .iter()
            .filter(|tlc| {
                if tlc.is_offered() {
                    matches!(tlc.outbound_status(), OutboundTlcStatus::Committed)
                } else {
                    matches!(tlc.inbound_status(), InboundTlcStatus::Committed)
                }
            })
            .cloned()
            .collect()
    }

    pub fn get_oldest_failed_tlcs(&self) -> Vec<TLCId> {
        let mut failed_tlcs = self
            .tlcs
            .iter()
            .filter_map(|tlc| {
                if tlc.is_fail_remove_confirmed() {
                    Some((tlc.tlc_id, tlc.removed_confirmed_at.unwrap_or(u64::MAX)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if failed_tlcs.len() > 1 {
            failed_tlcs.sort_by_key(|a| a.1);
            failed_tlcs
                .iter()
                .take(failed_tlcs.len() - 1)
                .map(|(tlc_id, _)| *tlc_id)
                .collect()
        } else {
            return Vec::new();
        }
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct TlcState {
    pub offered_tlcs: PendingTlcs,
    pub received_tlcs: PendingTlcs,
    pub applied_add_tlcs: HashSet<TLCId>,
    pub applied_remove_tlcs: HashSet<TLCId>,
    pub waiting_ack: bool,
}

impl TlcState {
    pub fn info(&self) -> String {
        format!(
            "offer_tlcs: {:?} received_tlcs: {:?} applied_add_tlcs: {:?} applied_remove_tlcs: {:?}",
            self.offered_tlcs.tlcs.len(),
            self.received_tlcs.tlcs.len(),
            self.applied_add_tlcs.len(),
            self.applied_remove_tlcs.len(),
        )
    }
    #[cfg(any(debug_assertions, feature = "bench"))]
    pub fn debug(&self) {
        let format_tlc_list = |tlcs: &[TlcInfo]| -> String {
            if tlcs.is_empty() {
                "    <none>".to_string()
            } else {
                tlcs.iter()
                    .map(|tlc| format!("    {}", tlc.log()))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        let offered_str = format_tlc_list(&self.offered_tlcs.tlcs);
        let received_str = format_tlc_list(&self.received_tlcs.tlcs);

        if offered_str.contains("<none>") && received_str.contains("<none>") {
            info!("TlcState: <none>");
        } else {
            info!(
                "TlcState:\n  Offered:\n{}\n  Received:\n{}",
                offered_str, received_str
            );
        }
    }

    pub fn get_mut(&mut self, tlc_id: &TLCId) -> Option<&mut TlcInfo> {
        self.offered_tlcs
            .tlcs
            .iter_mut()
            .find(|tlc| tlc.tlc_id == *tlc_id)
            .or_else(|| {
                self.received_tlcs
                    .tlcs
                    .iter_mut()
                    .find(|tlc| tlc.tlc_id == *tlc_id)
            })
    }

    pub fn get(&self, tlc_id: &TLCId) -> Option<&TlcInfo> {
        if tlc_id.is_offered() {
            self.offered_tlcs
                .tlcs
                .iter()
                .find(|tlc| tlc.tlc_id == *tlc_id)
        } else {
            self.received_tlcs
                .tlcs
                .iter()
                .find(|tlc| tlc.tlc_id == *tlc_id)
        }
    }

    pub fn get_committed_received_tlcs(&self) -> Vec<TlcInfo> {
        self.received_tlcs.get_committed_tlcs()
    }

    pub fn get_next_offering(&self) -> u64 {
        self.offered_tlcs.get_next_id()
    }

    pub fn get_next_received(&self) -> u64 {
        self.received_tlcs.get_next_id()
    }

    pub fn increment_offering(&mut self) {
        self.offered_tlcs.increment_next_id();
    }

    pub fn increment_received(&mut self) {
        self.received_tlcs.increment_next_id();
    }

    pub fn set_waiting_ack(&mut self, waiting_ack: bool) {
        self.waiting_ack = waiting_ack;
    }

    pub fn all_tlcs(&self) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.offered_tlcs
            .tlcs
            .iter()
            .chain(self.received_tlcs.tlcs.iter())
    }

    pub fn all_committed_tlcs(&self) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.offered_tlcs
            .tlcs
            .iter()
            .chain(self.received_tlcs.tlcs.iter())
            .filter(|tlc| {
                if tlc.is_offered() {
                    matches!(tlc.outbound_status(), OutboundTlcStatus::Committed)
                } else {
                    matches!(tlc.inbound_status(), InboundTlcStatus::Committed)
                }
            })
    }

    pub fn apply_remove_tlc(&mut self, tlc_id: TLCId) {
        self.applied_add_tlcs.remove(&tlc_id);
        self.applied_remove_tlcs.remove(&tlc_id);
        if tlc_id.is_offered() {
            self.offered_tlcs.tlcs.retain(|tlc| tlc.tlc_id != tlc_id);
        } else {
            self.received_tlcs.tlcs.retain(|tlc| tlc.tlc_id != tlc_id);
        }
    }
    pub fn add_offered_tlc(&mut self, tlc: TlcInfo) {
        self.offered_tlcs.add_tlc(tlc);
    }

    pub fn add_received_tlc(&mut self, tlc: TlcInfo) {
        self.received_tlcs.add_tlc(tlc);
    }

    pub fn set_received_tlc_removed(&mut self, tlc_id: u64, reason: RemoveTlcReason) -> Hash256 {
        let tlc = self.get_mut(&TLCId::Received(tlc_id)).expect("get tlc");
        assert_eq!(tlc.inbound_status(), InboundTlcStatus::Committed);
        tlc.removed_reason = Some(reason);
        tlc.status = TlcStatus::Inbound(InboundTlcStatus::LocalRemoved);
        tlc.payment_hash
    }

    pub fn set_offered_tlc_removed(&mut self, tlc_id: u64, reason: RemoveTlcReason) -> Hash256 {
        let tlc = self.get_mut(&TLCId::Offered(tlc_id)).expect("get tlc");
        assert_eq!(tlc.outbound_status(), OutboundTlcStatus::Committed);
        tlc.removed_reason = Some(reason);
        tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoteRemoved);
        tlc.payment_hash
    }

    pub fn commitment_signed_tlcs(&self, for_remote: bool) -> impl Iterator<Item = &TlcInfo> + '_ {
        self.offered_tlcs
            .tlcs
            .iter()
            .filter(move |tlc| match tlc.outbound_status() {
                OutboundTlcStatus::LocalAnnounced => for_remote,
                OutboundTlcStatus::Committed => true,
                OutboundTlcStatus::RemoteRemoved => for_remote,
                OutboundTlcStatus::RemoveWaitPrevAck => for_remote,
                OutboundTlcStatus::RemoveWaitAck => false,
                OutboundTlcStatus::RemoveAckConfirmed => false,
            })
            .chain(
                self.received_tlcs
                    .tlcs
                    .iter()
                    .filter(move |tlc| match tlc.inbound_status() {
                        InboundTlcStatus::RemoteAnnounced => !for_remote,
                        InboundTlcStatus::AnnounceWaitPrevAck => !for_remote,
                        InboundTlcStatus::AnnounceWaitAck => true,
                        InboundTlcStatus::Committed => true,
                        InboundTlcStatus::LocalRemoved => !for_remote,
                        InboundTlcStatus::RemoveAckConfirmed => false,
                    }),
            )
    }

    pub fn update_for_commitment_signed(&mut self) -> bool {
        for tlc in self.offered_tlcs.tlcs.iter_mut() {
            if tlc.outbound_status() == OutboundTlcStatus::RemoteRemoved {
                let status = if self.waiting_ack {
                    OutboundTlcStatus::RemoveWaitPrevAck
                } else {
                    OutboundTlcStatus::RemoveWaitAck
                };
                tlc.status = TlcStatus::Outbound(status);
            }
        }
        for tlc in self.received_tlcs.tlcs.iter_mut() {
            if tlc.inbound_status() == InboundTlcStatus::RemoteAnnounced {
                let status = if self.waiting_ack {
                    InboundTlcStatus::AnnounceWaitPrevAck
                } else {
                    InboundTlcStatus::AnnounceWaitAck
                };
                tlc.status = TlcStatus::Inbound(status)
            }
        }
        self.need_another_commitment_signed()
    }

    pub fn update_for_revoke_and_ack(&mut self, commitment_number: CommitmentNumbers) {
        for tlc in self.offered_tlcs.tlcs.iter_mut() {
            match tlc.outbound_status() {
                OutboundTlcStatus::LocalAnnounced => {
                    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::Committed);
                }
                OutboundTlcStatus::RemoveWaitPrevAck => {
                    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveWaitAck);
                }
                OutboundTlcStatus::RemoveWaitAck => {
                    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed);
                    tlc.removed_confirmed_at = Some(commitment_number.get_local());
                }
                _ => {}
            }
        }

        for tlc in self.received_tlcs.tlcs.iter_mut() {
            match tlc.inbound_status() {
                InboundTlcStatus::AnnounceWaitPrevAck => {
                    tlc.status = TlcStatus::Inbound(InboundTlcStatus::AnnounceWaitAck);
                }
                InboundTlcStatus::AnnounceWaitAck => {
                    tlc.status = TlcStatus::Inbound(InboundTlcStatus::Committed);
                }
                InboundTlcStatus::LocalRemoved => {
                    tlc.status = TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed);
                    tlc.removed_confirmed_at = Some(commitment_number.get_remote());
                }
                _ => {}
            }
        }
    }

    pub fn need_another_commitment_signed(&self) -> bool {
        self.offered_tlcs.tlcs.iter().any(|tlc| {
            let status = tlc.outbound_status();
            matches!(
                status,
                OutboundTlcStatus::LocalAnnounced
                    | OutboundTlcStatus::RemoteRemoved
                    | OutboundTlcStatus::RemoveWaitPrevAck
                    | OutboundTlcStatus::RemoveWaitAck
            )
        }) || self.received_tlcs.tlcs.iter().any(|tlc| {
            let status = tlc.inbound_status();
            matches!(
                status,
                InboundTlcStatus::RemoteAnnounced
                    | InboundTlcStatus::AnnounceWaitPrevAck
                    | InboundTlcStatus::AnnounceWaitAck
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct ChannelConstraints {
    // The maximum value can be in pending
    pub max_tlc_value_in_flight: u128,
    // The maximum number of tlcs that we can accept.
    pub max_tlc_number_in_flight: u64,
}

impl ChannelConstraints {
    pub fn new(max_tlc_value_in_flight: u128, max_tlc_number_in_flight: u64) -> Self {
        Self {
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
        }
    }
}

/// Data needed to revoke an outdated commitment transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RevocationData {
    /// The commitment transaction version number that was revoked
    pub commitment_number: u64,
    /// The x-only aggregated public key used in the multisig for this commitment transaction
    pub x_only_aggregated_pubkey: [u8; 32],
    /// The aggregated signature from both parties that authorizes the revocation
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    /// The output cell from the revoked commitment transaction
    #[serde_as(as = "EntityHex")]
    pub output: CellOutput,
    /// The associated data for the output cell (e.g., UDT amount for token transfers)
    #[serde_as(as = "EntityHex")]
    pub output_data: Bytes,
}

/// Data needed to authorize and execute a settlement transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementData {
    /// The x-only aggregated public key used in the multi-signature for the settlement transaction
    pub x_only_aggregated_pubkey: [u8; 32],
    /// The aggregated signature from both parties that authorizes the settlement transaction
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    /// The output cell for the local party (this node's owner) in the settlement transaction
    #[serde_as(as = "EntityHex")]
    pub to_local_output: CellOutput,
    /// The associated data for the local output cell (e.g., UDT amount for token transfers)
    #[serde_as(as = "EntityHex")]
    pub to_local_output_data: Bytes,
    /// The output cell for the remote party (channel partner) in the settlement transaction
    #[serde_as(as = "EntityHex")]
    pub to_remote_output: CellOutput,
    /// The associated data for the remote output cell (e.g., UDT amount for token transfers)
    #[serde_as(as = "EntityHex")]
    pub to_remote_output_data: Bytes,
    /// The list of Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementTlc {
    /// The ID of the TLC (either offered or received)
    pub tlc_id: TLCId,
    /// The hash algorithm used for the TLC
    pub hash_algorithm: HashAlgorithm,
    /// The amount of CKB/UDT involved in the TLC
    pub payment_amount: u128,
    /// The hash of the payment preimage
    pub payment_hash: Hash256,
    /// The expiry time for the TLC in milliseconds
    pub expiry: u64,
    /// The local party's private key used to sign the TLC
    pub local_key: Privkey,
    /// The remote party's public key used to verify the TLC
    pub remote_key: Pubkey,
}

impl SettlementTlc {
    pub fn to_witness(&self, for_remote: bool) -> Vec<u8> {
        let mut vec = Vec::new();
        let offered_flag = if self.tlc_id.is_offered() { 0u8 } else { 1u8 };
        vec.push(((self.hash_algorithm as u8) << 1) + offered_flag);
        vec.extend_from_slice(&self.payment_amount.to_le_bytes());
        vec.extend_from_slice(&self.payment_hash.as_ref()[0..20]);
        if for_remote {
            vec.extend_from_slice(blake160(&self.remote_key.serialize()).as_ref());
            vec.extend_from_slice(blake160(&self.local_key.pubkey().serialize()).as_ref());
        } else {
            vec.extend_from_slice(blake160(&self.local_key.pubkey().serialize()).as_ref());
            vec.extend_from_slice(blake160(&self.remote_key.serialize()).as_ref());
        }

        let since = Since::new(SinceType::Timestamp, self.expiry / 1000, false);
        vec.extend_from_slice(&since.value().to_le_bytes());
        vec
    }
}

type ScheduledChannelUpdateHandle =
    Option<Arc<JoinHandle<Result<(), MessagingErr<ChannelActorMessage>>>>>;

#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct ChannelActorState {
    pub state: ChannelState,
    // The data below are only relevant if the channel is public.
    pub public_channel_info: Option<PublicChannelInfo>,

    pub local_tlc_info: ChannelTlcInfo,
    pub remote_tlc_info: Option<ChannelTlcInfo>,

    // The local public key used to establish p2p network connection.
    pub local_pubkey: Pubkey,
    // The remote public key used to establish p2p network connection.
    pub remote_pubkey: Pubkey,

    pub id: Hash256,
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_tx: Option<Transaction>,

    pub funding_tx_confirmed_at: Option<(H256, u32, u64)>,

    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,

    // Is this channel initially inbound?
    // An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,

    // TODO: consider transaction fee while building the commitment transaction.
    // The invariant here is that the sum of `to_local_amount` and `to_remote_amount`
    // should be equal to the total amount of the channel.
    // The changes of both `to_local_amount` and `to_remote_amount`
    // will always happen after a revoke_and_ack message is sent/received.
    // This means that while calculating the amounts for commitment transactions,
    // processing add_tlc command and messages, we need to take into account that
    // the amounts are not decremented/incremented yet.

    // The amount of CKB/UDT that we own in the channel.
    // This value will only change after we have resolved a tlc.
    pub to_local_amount: u128,
    // The amount of CKB/UDT that the remote owns in the channel.
    // This value will only change after we have resolved a tlc.
    pub to_remote_amount: u128,

    // these two amounts used to keep the minimal ckb amount for the two parties
    // TLC operations will not affect these two amounts, only used to keep the commitment transactions
    // to be valid, so that any party can close the channel at any time.
    // Note: the values are different for the UDT scenario
    pub local_reserved_ckb_amount: u64,
    pub remote_reserved_ckb_amount: u64,

    // The commitment fee rate is used to calculate the fee for the commitment transactions.
    // The side who want to submit the commitment transaction will pay fee
    pub commitment_fee_rate: u64,

    // The delay time for the commitment transaction, this value is set by the initiator of the channel.
    // It must be a relative EpochNumberWithFraction in u64 format.
    pub commitment_delay_epoch: u64,

    // The fee rate used for funding transaction, the initiator may set it as `funding_fee_rate` option,
    // if it's not set, DEFAULT_FEE_RATE will be used as default value, two sides will use the same fee rate
    pub funding_fee_rate: u64,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    // Cached channel public keys for easier of access.
    pub local_channel_public_keys: ChannelBasePublicKeys,

    // Commitment numbers that are used to derive keys.
    // This value is guaranteed to be 0 when channel is just created.
    pub commitment_numbers: CommitmentNumbers,

    pub local_constraints: ChannelConstraints,
    pub remote_constraints: ChannelConstraints,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).

    // all the TLC related information
    pub tlc_state: TlcState,

    // the retryable tlc operations that are waiting to be processed.
    pub retryable_tlc_operations: VecDeque<RetryableTlcOperation>,
    pub waiting_forward_tlc_tasks: HashMap<(Hash256, TLCId), ForwardTlc>,

    // The remote and local lock script for close channel, they are setup during the channel establishment.
    #[serde_as(as = "Option<EntityHex>")]
    pub remote_shutdown_script: Option<Script>,
    #[serde_as(as = "EntityHex")]
    pub local_shutdown_script: Script,

    // Basically the latest remote nonce sent by the peer with the CommitmentSigned message,
    // but we will only update this field after we have sent a RevokeAndAck to the peer.
    // With above guarantee, we can be sure the results of the sender obtaining its latest local nonce
    // and the receiver obtaining its latest remote nonce are the same.
    pub last_committed_remote_nonce: Option<CommitmentNonce>,

    pub remote_revocation_nonce_for_verify: Option<RevocationNonce>,
    pub remote_revocation_nonce_for_send: Option<RevocationNonce>,
    pub remote_revocation_nonce_for_next: Option<RevocationNonce>,

    // The latest commitment transaction we're holding,
    // it can be broadcasted to blockchain by us to force close the channel.
    #[serde_as(as = "Option<EntityHex>")]
    pub latest_commitment_transaction: Option<Transaction>,

    // All the commitment point that are sent from the counterparty.
    // We need to save all these points to derive the keys for the commitment transactions.
    // The length of this vector is at most the maximum number of flighting tlcs.
    pub remote_commitment_points: Vec<(u64, Pubkey)>,
    pub remote_channel_public_keys: Option<ChannelBasePublicKeys>,

    // The shutdown info for both local and remote, they are setup by the shutdown command or message.
    pub local_shutdown_info: Option<ShutdownInfo>,
    pub remote_shutdown_info: Option<ShutdownInfo>,

    // Transaction hash of the shutdown transaction
    // The shutdown transaction can be COOPERATIVE or UNCOOPERATIVE
    pub shutdown_transaction_hash: Option<H256>,

    // A flag to indicate whether the channel is reestablishing,
    // we won't process any messages until the channel is reestablished.
    pub reestablishing: bool,
    pub last_revoke_ack_msg: Option<RevokeAndAck>,

    pub created_at: SystemTime,

    // the time stamp we last sent an message to the peer, used to check if the peer is still alive
    // we will disconnect the peer if we haven't sent any message to the peer for a long time
    // currently we only have set commitment_signed as the heartbeat message,
    #[serde(skip)]
    pub waiting_peer_response: Option<u64>,

    #[serde(skip)]
    pub network: Option<ActorRef<NetworkActorMessage>>,

    // The handle for scheduled channel update broadcasting.
    // We will use this handle to cancel the scheduled task when the channel is closed,
    // create a new handle when we broadcast a new channel update message.
    // The arc here is only used to implement the clone trait for the ChannelActorState.
    #[serde(skip)]
    pub scheduled_channel_update_handle: ScheduledChannelUpdateHandle,

    // The TLC set ready to be settled
    #[serde(skip)]
    pub pending_notify_mpp_tlcs: Vec<(Hash256, u64)>,

    #[serde(skip)]
    pub ephemeral_config: ChannelEphemeralConfig,

    // signing key
    #[serde(skip)]
    pub private_key: Option<Privkey>,
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Debug)]
pub struct ShutdownInfo {
    #[serde_as(as = "EntityHex")]
    pub close_script: Script,
    pub fee_rate: u64,
    pub signature: Option<PartialSignature>,
}

// This struct holds the TLC information for the channel participants.
// We can update this information through the channel update message.
#[serde_as]
#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelTlcInfo {
    // The timestamp when the following information is updated.
    pub timestamp: u64,

    // Whether this channel is enabled for TLC forwarding or not.
    pub enabled: bool,

    // The fee rate for TLC transfers. We only have these values set when
    // this is a public channel. Both sides may set this value differently.
    // This is a fee that is paid by the sender of the TLC.
    // The detailed calculation for the fee of forwarding tlcs is
    // `fee = round_above(tlc_fee_proportional_millionths * tlc_value / 1,000,000)`.
    pub tlc_fee_proportional_millionths: u128,

    // The expiry delta timestamp, in milliseconds, for the TLC.
    pub tlc_expiry_delta: u64,

    /// The minimal TLC value we can receive in relay TLC
    pub tlc_minimum_value: u128,
}

impl ChannelTlcInfo {
    pub fn new(
        tlc_min_value: u128,
        tlc_expiry_delta: u64,
        tlc_fee_proportional_millionths: u128,
    ) -> Self {
        Self {
            tlc_minimum_value: tlc_min_value,
            tlc_expiry_delta,
            tlc_fee_proportional_millionths,
            enabled: true,
            timestamp: now_timestamp_as_millis_u64(),
        }
    }
}

// This struct holds the channel information that are only relevant when the channel
// is public. The information includes signatures to the channel announcement message,
// our config for the channel that will be published to the network (via ChannelUpdate).
// For ChannelUpdate config, only information on our side are saved here because we have no
// control to the config on the counterparty side. And they will publish
// the config to the network via another ChannelUpdate message.
#[serde_as]
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct PublicChannelInfo {
    // Channel announcement signatures, may be empty for private channel.
    pub local_channel_announcement_signature: Option<(EcdsaSignature, PartialSignature)>,
    pub remote_channel_announcement_signature: Option<(EcdsaSignature, PartialSignature)>,

    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub remote_channel_announcement_nonce: Option<PubNonce>,

    pub channel_announcement: Option<ChannelAnnouncement>,
    pub channel_update: Option<ChannelUpdate>,
}

impl PublicChannelInfo {
    pub fn new() -> Self {
        Default::default()
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ClosedChannel {}

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum StopReason {
    Abandon,
    AbortFunding,
    Closed,
    PeerDisConnected,
}

#[derive(Debug)]
pub enum ChannelEvent {
    Stop(StopReason),
    FundingTransactionConfirmed(H256, u32, u64),
    // (tx_hash, force, close_by_us)
    ClosingTransactionConfirmed(H256, bool, bool),
    ForwardTlcResult(ForwardTlcResult),
    RunRetryTask,
    CheckActiveChannel,
    CheckFundingTimeout,
}

pub type ProcessingChannelResult = Result<(), ProcessingChannelError>;

#[derive(Error, Debug, Clone)]
pub enum ProcessingChannelError {
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("Repeated processing message: {0}")]
    RepeatedProcessing(String),
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Capacity error: {0}")]
    CapacityError(#[from] CapacityError),
    #[error("Failed to spawn actor: {0}")]
    SpawnErr(String),
    #[error("Musig2 RoundFinalizeError: {0}")]
    Musig2RoundFinalizeError(#[from] RoundFinalizeError),
    #[error("Musig2 VerifyError: {0}")]
    Musig2VerifyError(#[from] VerifyError),
    #[error("Musig2 SigningError: {0}")]
    Musig2SigningError(#[from] SigningError),
    #[error("Unable to handle TLC command in waiting TLC ACK state")]
    WaitingTlcAck,
    #[error("Failed to peel onion packet: {0}")]
    PeelingOnionPacketError(String),
    #[error("Forwarding node has tampered with the intended HTLC values or origin node has an obsolete cltv_expiry_delta")]
    IncorrectTlcExpiry,
    #[error("Upstream node set CLTV to less than the CLTV set by the sender")]
    IncorrectFinalTlcExpiry,
    #[error("The amount in the HTLC is not expected")]
    FinalIncorrectHTLCAmount,
    #[error("The payment_hash is not expected for final hop")]
    FinalIncorrectPaymentHash,
    #[error("The payment_hash and preimage does not match for final hop")]
    FinalIncorrectPreimage,
    #[error("The payment does not contain expected MPP info: {0}")]
    FinalIncorrectMPPInfo(String),
    #[error("The tlc forward fee is tow low")]
    TlcForwardFeeIsTooLow,
    #[error("The invoice status is invalid")]
    FinalInvoiceInvalid(CkbInvoiceStatus),
    #[error("The tlc number exceed limit of this channel")]
    TlcNumberExceedLimit,
    #[error("The tlc flight value exceed limit of this channel")]
    TlcValueInflightExceedLimit,
    #[error("The tlc amount below minimal")]
    TlcAmountIsTooLow,
    #[error("The tlc amount exceed maximal")]
    TlcAmountExceedLimit,
    #[error("The tlc expiry soon")]
    TlcExpirySoon,
    #[error("The tlc expiry too far")]
    TlcExpiryTooFar,
    #[error("Tlc forwarding error")]
    TlcForwardingError(TlcErr),
    #[error("Total number or bytes of to-be-accepted channels exceed the limit: {0}")]
    ToBeAcceptedChannelsExceedLimit(String),
}

/// ProcessingChannelError which brings the shared secret used in forwarding onion packet.
/// The shared secret is required to obfuscate the error message.
#[derive(Error, Debug)]
#[error("{source}")]
pub struct ProcessingChannelErrorWithSharedSecret {
    pub source: ProcessingChannelError,
    /// Shared secret used in forwarding.
    ///
    /// Save it to backward errors. Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
}

impl ProcessingChannelError {
    pub fn with_shared_secret(
        self,
        shared_secret: [u8; 32],
    ) -> ProcessingChannelErrorWithSharedSecret {
        ProcessingChannelErrorWithSharedSecret {
            source: self,
            shared_secret,
        }
    }

    pub fn without_shared_secret(self) -> ProcessingChannelErrorWithSharedSecret {
        self.with_shared_secret(NO_SHARED_SECRET)
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelFlags: u8 {
        const PUBLIC = 1;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct NegotiatingFundingFlags: u32 {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = NegotiatingFundingFlags::OUR_INIT_SENT.bits() | NegotiatingFundingFlags::THEIR_INIT_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CollaboratingFundingTxFlags: u32 {
        const AWAITING_REMOTE_TX_COLLABORATION_MSG = 1;
        const PREPARING_LOCAL_TX_COLLABORATION_MSG = 1 << 1;
        const OUR_TX_COMPLETE_SENT = 1 << 2;
        const THEIR_TX_COMPLETE_SENT = 1 << 3;
        const COLLABORATION_COMPLETED = CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT.bits() | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct SigningCommitmentFlags: u32 {
        const OUR_COMMITMENT_SIGNED_SENT = 1;
        const THEIR_COMMITMENT_SIGNED_SENT = 1 << 1;
        const COMMITMENT_SIGNED_SENT = SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT.bits() | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct AwaitingTxSignaturesFlags: u32 {
        const OUR_TX_SIGNATURES_SENT = 1;
        const THEIR_TX_SIGNATURES_SENT = 1 << 1;
        const TX_SIGNATURES_SENT = AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT.bits() | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct AwaitingChannelReadyFlags: u32 {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = AwaitingChannelReadyFlags::OUR_CHANNEL_READY.bits() | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ShuttingDownFlags: u32 {
        /// Indicates that we have sent a `shutdown` message.
        const OUR_SHUTDOWN_SENT = 1;
        /// Indicates that they have sent a `shutdown` message.
        const THEIR_SHUTDOWN_SENT = 1 << 1;
        /// Indicates that both we and they have sent `shutdown` messages,
        /// but some HTLCs are still pending to be resolved.
        const AWAITING_PENDING_TLCS = ShuttingDownFlags::OUR_SHUTDOWN_SENT.bits() | ShuttingDownFlags::THEIR_SHUTDOWN_SENT.bits();
        /// Indicates all pending HTLCs are resolved, and this channel will be dropped.
        const DROPPING_PENDING = 1 << 2;
        /// Indicates we have submitted a commitment transaction, waiting for confirmation
        const WAITING_COMMITMENT_CONFIRMATION = 1 << 3;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CloseFlags: u32 {
        /// Indicates that channel is closed cooperatively.
        const COOPERATIVE = 1;
        /// Indicates that channel is closed uncooperatively, initiated by local forcibly.
        const UNCOOPERATIVE_LOCAL = 1 << 1;
        /// Indicates that channel is abandoned.
        const ABANDONED = 1 << 2;
        /// Channel is closed because of aborted funding.
        const FUNDING_ABORTED = 1 << 3;
        /// Indicates that channel is closed uncooperatively, initiated by remote forcibly.
        const UNCOOPERATIVE_REMOTE = 1 << 4;
    }
}

impl ShuttingDownFlags {
    fn is_ok_for_commitment_operation(&self) -> bool {
        !self.contains(ShuttingDownFlags::DROPPING_PENDING)
            && !self.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
    }
}

// Depending on the state of the channel, we may process the commitment_signed command differently.
// Below are all the channel state flags variants that we may encounter
// in normal commitment_signed processing flow.
#[derive(Debug)]
enum CommitmentSignedFlags {
    SigningCommitment(SigningCommitmentFlags),
    PendingShutdown(),
    ChannelReady(),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(CollaboratingFundingTxFlags),
    /// We have collaborated over the funding and are now waiting for CommitmentSigned messages.
    SigningCommitment(SigningCommitmentFlags),
    /// We've received and sent `commitment_signed` and are now waiting for both
    /// party to collaborate on creating a valid funding transaction.
    AwaitingTxSignatures(AwaitingTxSignaturesFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady,
    /// We've successfully negotiated a `closing_signed` dance, the channel is now in the process of being shutdown.
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed(CloseFlags),
}

impl ChannelState {
    fn is_closed(&self) -> bool {
        matches!(
            self,
            ChannelState::Closed(_)
                | ChannelState::ShuttingDown(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
        )
    }

    /// Can only abort funding when the channel is funding and our signatures have not sent yet.
    fn can_abort_funding(&self) -> bool {
        match self {
            ChannelState::NegotiatingFunding(_)
            | ChannelState::CollaboratingFundingTx(_)
            | ChannelState::SigningCommitment(_) => true,
            ChannelState::AwaitingTxSignatures(flags)
                if !flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT) =>
            {
                true
            }
            _ => false,
        }
    }
}

fn new_channel_id_from_seed(seed: &[u8]) -> Hash256 {
    blake2b_256(seed).into()
}

fn derive_channel_id_from_tlc_keys(tlc_basepoint1: &Pubkey, tlc_basepoint2: &Pubkey) -> Hash256 {
    let mut preimage = [tlc_basepoint1.0.serialize(), tlc_basepoint2.0.serialize()];
    preimage.sort();
    new_channel_id_from_seed(&preimage.concat())
}

fn derive_temp_channel_id_from_tlc_key(tlc_basepoint: &Pubkey) -> Hash256 {
    let preimage = [tlc_basepoint.0.serialize(), [0; 33]].concat();
    new_channel_id_from_seed(&preimage)
}

pub fn get_commitment_secret(commitment_seed: &[u8; 32], commitment_number: u64) -> [u8; 32] {
    // Note that here, we hold the same assumption to bolts for commitment number,
    // i.e. this number should be in the range [0, 2^48).
    let mut res: [u8; 32] = *commitment_seed;
    for i in 0..48 {
        let bitpos = 47 - i;
        if commitment_number & (1 << bitpos) == (1 << bitpos) {
            res[bitpos / 8] ^= 1 << (bitpos & 7);
            res = blake2b_256(res);
        }
    }
    res
}

pub fn get_commitment_point(commitment_seed: &[u8; 32], commitment_number: u64) -> Pubkey {
    Privkey::from(&get_commitment_secret(commitment_seed, commitment_number)).pubkey()
}

pub(crate) fn get_funding_and_reserved_amount(
    total_amount: u128,
    shutdown_script: &Script,
    udt_type_script: &Option<Script>,
) -> Result<(u128, u64), ProcessingChannelError> {
    let reserved_capacity = reserved_capacity(shutdown_script, udt_type_script)?.as_u64();
    if udt_type_script.is_none() {
        if total_amount < reserved_capacity as u128 {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "The funding amount ({}) should be greater than or equal to {}",
                total_amount, reserved_capacity
            )));
        }
        if total_amount >= u64::MAX as u128 {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "The funding amount ({}) should be less than {}",
                total_amount,
                u64::MAX
            )));
        }
        Ok((total_amount - reserved_capacity as u128, reserved_capacity))
    } else {
        Ok((total_amount, reserved_capacity))
    }
}

pub(crate) fn reserved_capacity(
    shutdown_script: &Script,
    udt_type_script: &Option<Script>,
) -> Result<Capacity, CapacityError> {
    occupied_capacity(shutdown_script, udt_type_script)?
        .safe_add(Capacity::shannons(DEFAULT_MIN_SHUTDOWN_FEE))
}

pub(crate) fn occupied_capacity(
    shutdown_script: &Script,
    udt_type_script: &Option<Script>,
) -> Result<Capacity, CapacityError> {
    let cell_output = CellOutput::new_builder()
        .lock(shutdown_script.clone())
        .type_(udt_type_script.clone().pack())
        .build();

    if udt_type_script.is_some() {
        // 16 bytes for udt data
        cell_output.occupied_capacity(Capacity::bytes(16)?)
    } else {
        cell_output.occupied_capacity(Capacity::bytes(0)?)
    }
}

// Constructors for the channel actor state.
impl ChannelActorState {
    pub fn network(&self) -> ActorRef<NetworkActorMessage> {
        self.network
            .as_ref()
            .expect("ChannelActorState should have network actor reference")
            .clone()
    }

    fn private_key(&self) -> &Privkey {
        self.private_key
            .as_ref()
            .expect("ChannelActorState should have signing key")
    }

    pub fn is_public(&self) -> bool {
        self.public_channel_info.is_some()
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, ChannelState::ChannelReady)
    }

    pub fn is_tlc_forwarding_enabled(&self) -> bool {
        self.local_tlc_info.enabled
    }

    pub fn set_waiting_peer_response(&mut self) {
        self.waiting_peer_response = Some(now_timestamp_as_millis_u64());
    }

    pub fn clear_waiting_peer_response(&mut self) {
        self.waiting_peer_response = None;
    }

    pub fn should_disconnect_peer_awaiting_response(&self) -> bool {
        // this check only needed when other peer already shutdown force and we don't know it
        // if we are already got in ShuttingDown, means we already in normal shutdown process
        if matches!(self.state, ChannelState::ShuttingDown(_)) {
            return false;
        }
        if let Some(timestamp) = self.waiting_peer_response {
            // depends on the system's clock source, not all system clocks are monotonic,
            // using saturating_sub to avoid potential underflow
            let elapsed = now_timestamp_as_millis_u64().saturating_sub(timestamp);
            elapsed > PEER_CHANNEL_RESPONSE_TIMEOUT && !self.reestablishing
        } else {
            false
        }
    }

    pub fn set_waiting_ack(&mut self, myself: &ActorRef<ChannelActorMessage>, waiting_ack: bool) {
        self.tlc_state.set_waiting_ack(waiting_ack);
        if waiting_ack {
            self.set_waiting_peer_response();
            myself.send_after(Duration::from_millis(PEER_CHANNEL_RESPONSE_TIMEOUT), || {
                ChannelActorMessage::Event(ChannelEvent::CheckActiveChannel)
            });
        } else {
            self.clear_waiting_peer_response();
        }
    }

    pub fn try_create_channel_messages(&mut self) -> Option<(ChannelAnnouncement, ChannelUpdate)> {
        let channel_announcement = self.try_create_channel_announcement_message()?;
        let channel_update = self.try_create_channel_update_message()?;
        Some((channel_announcement, channel_update))
    }

    pub fn try_create_channel_announcement_message(&mut self) -> Option<ChannelAnnouncement> {
        if !self.is_public() {
            debug!("Ignoring non-public channel announcement");
            return None;
        }

        let channel_outpoint = self.get_funding_transaction_outpoint()?;
        let mut channel_announcement = match self
            .public_channel_info
            .as_ref()
            .and_then(|state| state.channel_announcement.clone())
        {
            // Skipping creating new signed channel announcement if it exists
            Some(x) if x.is_signed() => return Some(x),
            // We have created a channel announcement, but it's not signed by the other
            // party yet. We should try to complete the signatures next.
            Some(x) => x,
            // We have not created a channel announcement yet.
            None => {
                let capacity = self.get_liquid_capacity();
                let (node1_id, node2_id) = if self.local_is_node1() {
                    (self.local_pubkey, self.remote_pubkey)
                } else {
                    (self.remote_pubkey, self.local_pubkey)
                };

                ChannelAnnouncement::new_unsigned(
                    &node1_id,
                    &node2_id,
                    channel_outpoint,
                    &self.get_funding_lock_script_xonly_key(),
                    capacity,
                    self.funding_udt_type_script.clone(),
                )
            }
        };

        let local_nonce = self
            .get_channel_announcement_musig2_secnonce()
            .public_nonce();
        let remote_nonce = self.get_remote_channel_announcement_nonce()?;
        let agg_nonce =
            AggNonce::sum(self.order_things_for_musig2(local_nonce, remote_nonce.clone()));

        let key_agg_ctx = self.get_deterministic_musig2_agg_context();

        let message = channel_announcement.message_to_sign();

        let (local_node_signature, local_partial_signature) =
            self.get_or_create_local_channel_announcement_signature(remote_nonce.clone(), message);

        let (remote_node_signature, remote_partial_signature) =
            self.get_remote_channel_announcement_signature()?;

        if self.local_is_node1() {
            channel_announcement.node1_signature = Some(local_node_signature);
            channel_announcement.node2_signature = Some(remote_node_signature);
        } else {
            channel_announcement.node1_signature = Some(remote_node_signature);
            channel_announcement.node2_signature = Some(local_node_signature);
        }

        let partial_signatures =
            self.order_things_for_musig2(local_partial_signature, remote_partial_signature);

        if let Ok(signature) =
            aggregate_partial_signatures(&key_agg_ctx, &agg_nonce, partial_signatures, message)
        {
            channel_announcement.ckb_signature = Some(signature);
            self.public_channel_state_mut().channel_announcement =
                Some(channel_announcement.clone());
            Some(channel_announcement)
        } else {
            // TODO: we should ban remote peer if we fail to aggregate the signature since the error is caused by the wrong nonce.
            warn!(
                "Failed to aggregate channel announcement signature for channel {:?}",
                self.get_id()
            );
            None
        }
    }

    fn do_generate_channel_update(
        &mut self,
        // The function that would change the channel update parameters.
        f: impl FnOnce(&mut ChannelUpdate),
    ) -> ChannelUpdate {
        assert!(self.is_public());
        let mut channel_update = self
            .get_unsigned_channel_update_message()
            .expect("public channel can generate channel update message");
        f(&mut channel_update);
        debug!(
            "Generated channel update message for channel {:?}: {:?}",
            &self.get_id(),
            &channel_update
        );
        let node_signature =
            sign_network_message(self.private_key(), channel_update.message_to_sign());

        channel_update.signature = Some(node_signature);
        self.public_channel_state_mut().channel_update = Some(channel_update.clone());
        channel_update
    }

    fn generate_channel_update(&mut self) -> ChannelUpdate {
        self.do_generate_channel_update(|_update| {})
    }

    fn create_update_tlc_info_message(&mut self) -> UpdateTlcInfo {
        self.local_tlc_info.timestamp = now_timestamp_as_millis_u64();
        UpdateTlcInfo {
            channel_id: self.get_id(),
            timestamp: self.local_tlc_info.timestamp,
            channel_flags: self.get_channel_update_channel_flags(),
            tlc_minimum_value: self.local_tlc_info.tlc_minimum_value,
            tlc_fee_proportional_millionths: self.local_tlc_info.tlc_fee_proportional_millionths,
            tlc_expiry_delta: self.local_tlc_info.tlc_expiry_delta,
        }
    }

    fn generate_disabled_channel_update(&mut self) -> ChannelUpdate {
        self.do_generate_channel_update(|update| {
            update.channel_flags |= ChannelUpdateChannelFlags::DISABLED;
        })
    }

    // Notify the network, network graph and channel counterparty about the channel update,
    // and update the handle for scheduled channel update broadcasting.
    // We do this on channel ready, channel reestablishment, user channel parameters updates.
    // Some of the events require us to send an OwnedChannelUpdateEvent::Up to the network actor,
    // (e.g. channel ready and channel reestablishment) and some require us to send a
    // OwnedChannelUpdateEvent::Updated (e.g. user channel parameters update) to the network actor.
    // update_only is used to distinguish between the two cases.
    fn on_owned_channel_updated(
        &mut self,
        myself: &ActorRef<ChannelActorMessage>,
        update_only: bool,
    ) {
        if update_only {
            self.update_graph_for_local_channel_change();
        } else {
            self.update_graph_for_local_channel_ready();
        }
        // Cancel the scheduled channel update broadcasting task if it exists.
        if let Some(handle) = self.scheduled_channel_update_handle.take() {
            handle.abort();
        }
        self.broadcast_channel_update(myself);
        self.send_update_tlc_info_message();
    }

    fn broadcast_channel_update(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        if self.is_public() {
            let channel_update = self.generate_channel_update();
            self.network()
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::BroadcastMessages(vec![
                        BroadcastMessageWithTimestamp::ChannelUpdate(channel_update),
                    ]),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

            // We need to periodically broadcast the public channel update message to the network,
            // so that the network can know the channel is still alive. We are currently using
            // BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION / 2 as interval to broadcast the channel update message.
            // This allows us to have send at least one channel update message in the interval of BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION.
            // Note that even though we use send_after to send a message after the timeout, this will
            // actually send the message periodically, we will send another message while the previous message
            // is received.
            let handle = myself.send_after(
                SOFT_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION / 2,
                || ChannelActorMessage::Command(ChannelCommand::BroadcastChannelUpdate()),
            );
            self.scheduled_channel_update_handle = Some(Arc::new(handle));
        }
    }

    fn update_graph_for_remote_channel_change(&mut self) {
        if let Some(channel_update_info) = self.get_remote_channel_update_info() {
            if let Some(channel_outpoint) = self.get_funding_transaction_outpoint() {
                let peer_id = self.get_remote_pubkey();
                self.network()
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::OwnedChannelUpdateEvent(
                            super::graph::OwnedChannelUpdateEvent::Updated(
                                channel_outpoint,
                                peer_id,
                                channel_update_info,
                            ),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        }
    }

    fn update_graph_for_local_channel_ready(&mut self) {
        if let Ok(channel_info) = (&*self).try_into() {
            self.network()
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::OwnedChannelUpdateEvent(
                        super::graph::OwnedChannelUpdateEvent::Up(channel_info),
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
    }

    fn update_graph_for_local_channel_change(&mut self) {
        let Some(channel_outpoint) = self.get_funding_transaction_outpoint() else {
            return;
        };
        let peer_id = self.get_local_pubkey();
        let channel_update_info = self.get_local_channel_update_info();
        self.network()
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::OwnedChannelUpdateEvent(
                    super::graph::OwnedChannelUpdateEvent::Updated(
                        channel_outpoint,
                        peer_id,
                        channel_update_info,
                    ),
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    fn send_update_tlc_info_message(&mut self) {
        let update_tlc_info = self.create_update_tlc_info_message();
        self.network()
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId {
                    peer_id: self.get_remote_peer_id(),
                    message: FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::UpdateTlcInfo(update_tlc_info),
                    ),
                }),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    fn try_create_channel_update_message(&mut self) -> Option<ChannelUpdate> {
        if !self.is_public() {
            debug!("Ignoring non-public channel update");
            return None;
        }

        if let Some(x) = self
            .public_channel_info
            .as_ref()
            .and_then(|state| state.channel_update.clone())
        {
            return Some(x);
        };

        Some(self.generate_channel_update())
    }

    fn get_channel_update_channel_flags(&self) -> ChannelUpdateChannelFlags {
        if self.is_tlc_forwarding_enabled() {
            ChannelUpdateChannelFlags::empty()
        } else {
            ChannelUpdateChannelFlags::DISABLED
        }
    }

    fn schedule_next_retry_task(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        if !self.retryable_tlc_operations.is_empty() {
            myself.send_after(RETRYABLE_TLC_OPS_INTERVAL, || {
                ChannelActorMessage::Event(ChannelEvent::RunRetryTask)
            });
        }
        debug!(
            "schedule_next_retry_task retryable tlc ops: {:?} \
            waiting_forward: {:?} tlc_state: {:?}",
            self.retryable_tlc_operations.len(),
            self.waiting_forward_tlc_tasks.len(),
            self.tlc_state.info()
        );
    }

    pub fn get_unsigned_channel_update_message(&self) -> Option<ChannelUpdate> {
        let message_flags = if self.local_is_node1() {
            ChannelUpdateMessageFlags::UPDATE_OF_NODE1
        } else {
            ChannelUpdateMessageFlags::UPDATE_OF_NODE2
        };

        self.is_public().then_some(ChannelUpdate::new_unsigned(
            self.must_get_funding_transaction_outpoint(),
            now_timestamp_as_millis_u64(),
            message_flags,
            self.get_channel_update_channel_flags(),
            self.local_tlc_info.tlc_expiry_delta,
            self.local_tlc_info.tlc_minimum_value,
            self.local_tlc_info.tlc_fee_proportional_millionths,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_inbound_channel(
        temp_channel_id: Hash256,
        public_channel_info: Option<PublicChannelInfo>,
        local_value: u128,
        local_reserved_ckb_amount: u64,
        commitment_fee_rate: u64,
        commitment_delay_epoch: u64,
        funding_fee_rate: u64,
        funding_udt_type_script: Option<Script>,
        seed: &[u8],
        local_pubkey: Pubkey,
        remote_pubkey: Pubkey,
        local_shutdown_script: Script,
        remote_shutdown_script: Script,
        remote_value: u128,
        remote_reserved_ckb_amount: u64,
        remote_pubkeys: ChannelBasePublicKeys,
        remote_nonce: CommitmentNonce,
        remote_revocation_nonce: RevocationNonce,
        remote_channel_announcement_nonce: Option<PubNonce>,
        first_commitment_point: Pubkey,
        second_commitment_point: Pubkey,
        remote_max_tlc_value_in_flight: u128,
        remote_max_tlc_number_in_flight: u64,
        local_max_tlc_number_in_flight: u64,
        local_max_tlc_value_in_flight: u128,
        local_tlc_info: ChannelTlcInfo,
        network: ActorRef<NetworkActorMessage>,
        private_key: Privkey,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_base_pubkeys = signer.get_base_public_keys();

        let channel_id = derive_channel_id_from_tlc_keys(
            &local_base_pubkeys.tlc_base_key,
            &remote_pubkeys.tlc_base_key,
        );

        debug!(
            "Generated channel id ({:?}) for temporary channel {:?}",
            &channel_id, &temp_channel_id,
        );

        let mut state = Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
            public_channel_info,
            local_tlc_info,
            remote_tlc_info: None,
            local_pubkey,
            remote_pubkey,
            funding_tx: None,
            funding_tx_confirmed_at: None,
            is_acceptor: true,
            funding_udt_type_script,
            to_local_amount: local_value,
            to_remote_amount: remote_value,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            id: channel_id,
            tlc_state: Default::default(),
            retryable_tlc_operations: Default::default(),
            waiting_forward_tlc_tasks: Default::default(),
            local_shutdown_script,
            local_channel_public_keys: local_base_pubkeys,
            signer,
            remote_channel_public_keys: Some(remote_pubkeys),
            commitment_numbers: Default::default(),
            remote_shutdown_script: Some(remote_shutdown_script),
            last_committed_remote_nonce: Some(remote_nonce),
            remote_revocation_nonce_for_send: Some(remote_revocation_nonce.clone()),
            remote_revocation_nonce_for_verify: Some(remote_revocation_nonce.clone()),
            remote_revocation_nonce_for_next: None,
            remote_commitment_points: vec![
                (1, first_commitment_point),
                (2, second_commitment_point),
            ],
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount,
            local_constraints: ChannelConstraints::new(
                local_max_tlc_value_in_flight,
                local_max_tlc_number_in_flight,
            ),
            remote_constraints: ChannelConstraints::new(
                remote_max_tlc_value_in_flight,
                remote_max_tlc_number_in_flight,
            ),
            latest_commitment_transaction: None,
            reestablishing: false,
            last_revoke_ack_msg: None,
            created_at: SystemTime::now(),
            waiting_peer_response: None,
            network: Some(network),
            scheduled_channel_update_handle: None,
            pending_notify_mpp_tlcs: vec![],
            ephemeral_config: Default::default(),
            private_key: Some(private_key),
        };
        if let Some(nonce) = remote_channel_announcement_nonce {
            state.update_remote_channel_announcement_nonce(&nonce);
        }
        state
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_outbound_channel(
        public_channel_info: Option<PublicChannelInfo>,
        seed: &[u8],
        local_pubkey: Pubkey,
        remote_pubkey: Pubkey,
        to_local_amount: u128,
        local_reserved_ckb_amount: u64,
        commitment_fee_rate: u64,
        commitment_delay_epoch: u64,
        funding_fee_rate: u64,
        funding_udt_type_script: Option<Script>,
        shutdown_script: Script,
        local_max_tlc_value_in_flight: u128,
        local_max_tlc_number_in_flight: u64,
        local_tlc_info: ChannelTlcInfo,
        network: ActorRef<NetworkActorMessage>,
        private_key: Privkey,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_pubkeys = signer.get_base_public_keys();
        let temp_channel_id = derive_temp_channel_id_from_tlc_key(&local_pubkeys.tlc_base_key);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            public_channel_info,
            local_tlc_info,
            remote_tlc_info: None,
            local_pubkey,
            remote_pubkey,
            funding_tx: None,
            funding_tx_confirmed_at: None,
            funding_udt_type_script,
            is_acceptor: false,
            to_local_amount,
            to_remote_amount: 0,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            id: temp_channel_id,
            tlc_state: Default::default(),
            retryable_tlc_operations: Default::default(),
            waiting_forward_tlc_tasks: Default::default(),
            signer,
            local_channel_public_keys: local_pubkeys,
            local_constraints: ChannelConstraints::new(
                local_max_tlc_value_in_flight,
                local_max_tlc_number_in_flight,
            ),
            // these values will update after accept channel peer message handled
            remote_constraints: ChannelConstraints::default(),
            remote_channel_public_keys: None,
            last_committed_remote_nonce: None,
            remote_revocation_nonce_for_send: None,
            remote_revocation_nonce_for_verify: None,
            remote_revocation_nonce_for_next: None,
            commitment_numbers: Default::default(),
            remote_commitment_points: vec![],
            local_shutdown_script: shutdown_script,
            remote_shutdown_script: None,
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount: 0,
            latest_commitment_transaction: None,
            reestablishing: false,
            last_revoke_ack_msg: None,
            created_at: SystemTime::now(),
            waiting_peer_response: None,
            network: Some(network),
            scheduled_channel_update_handle: None,
            pending_notify_mpp_tlcs: vec![],
            ephemeral_config: Default::default(),
            private_key: Some(private_key),
        }
    }

    fn check_accept_channel_parameters(&self) -> ProcessingChannelResult {
        if self.remote_constraints.max_tlc_number_in_flight > MAX_TLC_NUMBER_IN_FLIGHT {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Remote max TLC number in flight {} is greater than the system maximal value {}",
                self.remote_constraints.max_tlc_number_in_flight, MAX_TLC_NUMBER_IN_FLIGHT
            )));
        }

        let udt_type_script = &self.funding_udt_type_script;

        if udt_type_script.is_some() {
            if self.to_local_amount > u128::MAX - self.to_remote_amount {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "The total UDT funding amount should be less than {}",
                    u128::MAX
                )));
            }
        } else {
            let total_ckb_amount = self.get_liquid_capacity();
            let max_ckb_amount = u64::MAX as u128 - self.get_total_reserved_ckb_amount() as u128;
            if total_ckb_amount > max_ckb_amount {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "The total funding amount ({}) should be less than {}",
                    total_ckb_amount, max_ckb_amount
                )));
            }
        }

        // reserved_ckb_amount
        let occupied_capacity =
            occupied_capacity(&self.get_remote_shutdown_script(), udt_type_script)?.as_u64();
        if self.remote_reserved_ckb_amount < occupied_capacity {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Reserved CKB amount {} is less than {}",
                self.remote_reserved_ckb_amount, occupied_capacity,
            )));
        }

        // commitment_fee_rate
        let commitment_fee = calculate_commitment_tx_fee(self.commitment_fee_rate, udt_type_script);
        let reserved_fee = self.remote_reserved_ckb_amount - occupied_capacity;
        if commitment_fee * 2 > reserved_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee {} which calculated by commitment fee rate {} is larger than half of reserved fee {}",
                commitment_fee, self.commitment_fee_rate, reserved_fee
            )));
        }

        Ok(())
    }

    fn check_shutdown_fee_rate(
        &self,
        fee_rate: FeeRate,
        close_script: &Script,
    ) -> ProcessingChannelResult {
        if fee_rate.as_u64() < self.commitment_fee_rate {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Fee rate {} is less than commitment fee rate {}",
                fee_rate, self.commitment_fee_rate
            )));
        }

        let fee = calculate_shutdown_tx_fee(
            fee_rate.as_u64(),
            &self.funding_udt_type_script,
            (self.get_remote_shutdown_script(), close_script.clone()),
        );

        let occupied_capacity =
            occupied_capacity(close_script, &self.funding_udt_type_script)?.as_u64();
        let available_max_fee = if self.funding_udt_type_script.is_none() {
            (self.to_local_amount as u64 + self.local_reserved_ckb_amount)
                .saturating_sub(occupied_capacity)
        } else {
            self.local_reserved_ckb_amount
                .saturating_sub(occupied_capacity)
        };

        if fee > available_max_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Local balance is not enough to pay the fee, expect fee {} <= available_max_fee {}",
                fee, available_max_fee
            )));
        }
        Ok(())
    }

    pub fn get_local_balance(&self) -> u128 {
        self.to_local_amount
    }

    pub fn get_remote_balance(&self) -> u128 {
        self.to_remote_amount
    }

    pub fn get_offered_tlc_balance(&self) -> u128 {
        self.get_all_offer_tlcs()
            .filter(|tlc| !tlc.is_fail_remove_confirmed())
            .map(|tlc| tlc.amount)
            .sum::<u128>()
    }

    pub fn get_received_tlc_balance(&self) -> u128 {
        self.get_all_received_tlcs()
            .filter(|tlc| !tlc.is_fail_remove_confirmed())
            .map(|tlc| tlc.amount)
            .sum::<u128>()
    }

    pub fn get_created_at_in_millis(&self) -> u64 {
        self.created_at
            .duration_since(UNIX_EPOCH)
            .expect("Duration since unix epoch")
            .as_millis() as u64
    }

    pub fn is_closed(&self) -> bool {
        self.state.is_closed()
    }

    pub(crate) fn update_state(&mut self, new_state: ChannelState) {
        debug!(
            "Updating channel state from {:?} to {:?}",
            &self.state, &new_state
        );
        self.state = new_state;
    }

    pub(crate) fn local_is_node1(&self) -> bool {
        self.local_pubkey < self.remote_pubkey
    }

    fn get_or_create_local_channel_announcement_signature(
        &mut self,
        remote_nonce: PubNonce,
        message: [u8; 32],
    ) -> (EcdsaSignature, PartialSignature) {
        if let Some(local_channel_announcement_signature) = self
            .public_channel_info
            .as_ref()
            .and_then(|channel_info| channel_info.local_channel_announcement_signature.clone())
        {
            return local_channel_announcement_signature;
        }

        let local_secnonce = self.get_channel_announcement_musig2_secnonce();
        let local_nonce = local_secnonce.public_nonce();
        let agg_nonce = AggNonce::sum(self.order_things_for_musig2(local_nonce, remote_nonce));
        let key_agg_ctx = self.get_deterministic_musig2_agg_context();
        let channel_id = self.get_id();
        let peer_id = self.get_remote_peer_id();
        let channel_outpoint = self.must_get_funding_transaction_outpoint();

        let partial_signature: PartialSignature = sign_partial(
            &key_agg_ctx,
            &self.signer.funding_key,
            local_secnonce,
            &agg_nonce,
            message,
        )
        .expect("Partial sign channel announcement");

        let node_signature = sign_network_message(self.private_key(), message);
        self.network()
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    peer_id,
                    FiberMessage::announcement_signatures(AnnouncementSignatures {
                        channel_id,
                        channel_outpoint,
                        partial_signature,
                        node_signature: node_signature.clone(),
                    }),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        let result = (node_signature, partial_signature);
        self.public_channel_state_mut()
            .local_channel_announcement_signature = Some(result.clone());
        result
    }

    fn public_channel_state_mut(&mut self) -> &mut PublicChannelInfo {
        self.public_channel_info
            .as_mut()
            .expect("public channel info exists")
    }

    fn get_remote_channel_announcement_nonce(&self) -> Option<PubNonce> {
        self.public_channel_info
            .as_ref()
            .and_then(|state| state.remote_channel_announcement_nonce.clone())
    }

    fn update_remote_channel_announcement_nonce(&mut self, nonce: &PubNonce) {
        assert!(self.is_public());
        self.public_channel_state_mut()
            .remote_channel_announcement_nonce = Some(nonce.clone());
    }

    fn get_remote_channel_announcement_signature(
        &self,
    ) -> Option<(EcdsaSignature, PartialSignature)> {
        self.public_channel_info
            .as_ref()
            .and_then(|state| state.remote_channel_announcement_signature.clone())
    }

    fn update_remote_channel_announcement_signature(
        &mut self,
        ecdsa_signature: EcdsaSignature,
        partial_signatures: PartialSignature,
    ) {
        assert!(self.is_public());
        self.public_channel_info
            .as_mut()
            .expect("public channel info exists")
            .remote_channel_announcement_signature = Some((ecdsa_signature, partial_signatures));
    }

    fn update_our_tlc_fee_proportional_millionths(&mut self, fee: u128) -> bool {
        if self.local_tlc_info.tlc_fee_proportional_millionths == fee {
            return false;
        }
        self.local_tlc_info.tlc_fee_proportional_millionths = fee;
        true
    }

    fn update_our_tlc_min_value(&mut self, value: u128) -> bool {
        if self.local_tlc_info.tlc_minimum_value == value {
            return false;
        }
        self.local_tlc_info.tlc_minimum_value = value;
        true
    }

    fn update_our_enabled(&mut self, enabled: bool) -> bool {
        if self.local_tlc_info.enabled == enabled {
            return false;
        }
        self.local_tlc_info.enabled = enabled;
        true
    }

    fn update_our_tlc_expiry_delta(&mut self, value: u64) -> bool {
        if self.local_tlc_info.tlc_expiry_delta == value {
            return false;
        }
        self.local_tlc_info.tlc_expiry_delta = value;
        true
    }

    fn get_total_reserved_ckb_amount(&self) -> u64 {
        self.local_reserved_ckb_amount + self.remote_reserved_ckb_amount
    }

    fn get_total_ckb_amount(&self) -> u64 {
        self.to_local_amount as u64
            + self.to_remote_amount as u64
            + self.get_total_reserved_ckb_amount()
    }

    // Get the total liquid capacity of the channel, which will exclude the reserved ckb amount.
    // This is the capacity used for gossiping channel information.
    pub(crate) fn get_liquid_capacity(&self) -> u128 {
        self.to_local_amount + self.to_remote_amount
    }

    // Send RevokeAndAck message to the counterparty, and update the
    // channel state accordingly.
    fn send_revoke_and_ack_message(&mut self) -> ProcessingChannelResult {
        // special case for reestablishing channel state
        if self.remote_revocation_nonce_for_send.is_none() {
            match self.last_revoke_ack_msg {
                Some(ref revoke_and_ack) => {
                    self.network()
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                                self.get_remote_peer_id(),
                                FiberMessage::revoke_and_ack(revoke_and_ack.clone()),
                            )),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    return Ok(());
                }
                None => {
                    let error = "Error reestablishing channel state, RevokeAndAck message not sent, but remote_revocation_nonce_for_send is None";
                    warn!(error);
                    return Err(ProcessingChannelError::InvalidState(error.to_string()));
                }
            }
        }

        let sign_ctx = match self.get_revoke_sign_context(false) {
            Some(ctx) => ctx,
            None => {
                let error =
                    "get_revoke_sign_context returned None, cannot send RevokeAndAck message";
                warn!(error);
                return Err(ProcessingChannelError::InvalidState(error.to_string()));
            }
        };
        let x_only_aggregated_pubkey = sign_ctx.common_ctx.x_only_aggregated_pubkey();

        let revocation_partial_signature = {
            let commitment_tx_fee = calculate_commitment_tx_fee(
                self.commitment_fee_rate,
                &self.funding_udt_type_script,
            );
            let lock_script = self.get_remote_shutdown_script();
            let (output, output_data) = if let Some(udt_type_script) = &self.funding_udt_type_script
            {
                let capacity = self.get_total_reserved_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script)
                    .type_(Some(udt_type_script.clone()).pack())
                    .capacity(capacity.pack())
                    .build();

                let output_data = self.get_liquid_capacity().to_le_bytes().pack();
                (output, output_data)
            } else {
                let capacity = self.get_total_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script.clone())
                    .capacity(capacity.pack())
                    .build();
                let output_data = Bytes::default();
                (output, output_data)
            };

            let commitment_number = self.get_remote_commitment_number() - 1;
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                commitment_number.to_be_bytes().as_slice(),
            ]
            .concat();

            let message = blake2b_256(
                [
                    output.as_slice(),
                    output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );
            let our_signature = sign_ctx.sign(message.as_slice()).expect("valid signature");
            our_signature
        };

        let commitment_tx_partial_signature = {
            let (
                [to_local_output, to_remote_output],
                [to_local_output_data, to_remote_output_data],
            ) = self.build_settlement_transaction_outputs(false);
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                self.get_remote_commitment_number().to_be_bytes().as_slice(),
            ]
            .concat();

            let message = blake2b_256(
                [
                    to_local_output.as_slice(),
                    to_local_output_data.as_slice(),
                    to_remote_output.as_slice(),
                    to_remote_output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );
            let sign_ctx = match self.get_ack_sign_context(false) {
                Some(ctx) => ctx,
                None => {
                    let error =
                        "get_ack_sign_context returned None, cannot send RevokeAndAck message";
                    warn!(error);
                    return Err(ProcessingChannelError::InvalidState(error.to_string()));
                }
            };
            sign_ctx.sign(message.as_slice())?
        };

        let next_revocation_nonce = self.get_next_revocation_nonce();
        // Note that we must update channel state here to update commitment number,
        // so that next step will obtain the correct commitment point.
        self.increment_remote_commitment_number();
        let point = self.get_current_local_commitment_point();
        self.last_revoke_ack_msg = Some(RevokeAndAck {
            channel_id: self.get_id(),
            revocation_partial_signature,
            commitment_tx_partial_signature,
            next_revocation_nonce,
            next_per_commitment_point: point,
        });

        // update the remote_revocation_nonce_for_send and remote_revocation_nonce_for_verify for next round if needed
        if self.remote_revocation_nonce_for_verify.is_none() {
            self.remote_revocation_nonce_for_send = self.remote_revocation_nonce_for_next.clone();
            self.remote_revocation_nonce_for_verify = self.remote_revocation_nonce_for_next.clone();
        } else {
            self.remote_revocation_nonce_for_send = None;
        }

        self.network()
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    self.get_remote_peer_id(),
                    FiberMessage::revoke_and_ack(
                        self.last_revoke_ack_msg.as_ref().unwrap().clone(),
                    ),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        Ok(())
    }

    pub fn get_id(&self) -> Hash256 {
        self.id
    }

    pub fn get_local_pubkey(&self) -> Pubkey {
        self.local_pubkey
    }

    pub fn get_local_peer_id(&self) -> PeerId {
        self.local_pubkey.tentacle_peer_id()
    }

    pub fn get_local_channel_update_info(&self) -> ChannelUpdateInfo {
        let balance = self.get_local_balance();
        let mut info = ChannelUpdateInfo::from(&self.local_tlc_info);
        info.outbound_liquidity = Some(balance);
        info
    }

    pub fn get_remote_pubkey(&self) -> Pubkey {
        self.remote_pubkey
    }

    pub fn get_remote_peer_id(&self) -> PeerId {
        self.remote_pubkey.tentacle_peer_id()
    }

    pub fn get_remote_channel_update_info(&self) -> Option<ChannelUpdateInfo> {
        let balance = self.get_remote_balance();
        self.remote_tlc_info.as_ref().map(|tlc_info| {
            let mut info = ChannelUpdateInfo::from(tlc_info);
            info.outbound_liquidity = Some(balance);
            info
        })
    }

    pub fn get_next_commitment_nonce(&self) -> CommitmentNonce {
        let commitment_number = self.get_next_commitment_number(true);
        let [funding, commitment] = self.signer.derive_musig2_nonce(
            commitment_number,
            [Musig2Context::Funding, Musig2Context::Commitment],
        );
        CommitmentNonce {
            funding: funding.public_nonce(),
            commitment: commitment.public_nonce(),
        }
    }

    pub fn get_next_revocation_nonce(&self) -> RevocationNonce {
        let commitment_number = self.get_next_commitment_number(false);
        let [revoke, ack] = self.signer.derive_musig2_nonce(
            commitment_number,
            [Musig2Context::Revoke, Musig2Context::Ack],
        );
        RevocationNonce {
            revoke: revoke.public_nonce(),
            ack: ack.public_nonce(),
        }
    }

    fn get_last_committed_remote_nonce(&self) -> CommitmentNonce {
        self.last_committed_remote_nonce
            .as_ref()
            .expect("always have peer's last_committed_remote_nonce in normal channel operations")
            .clone()
    }

    fn commit_remote_nonce(&mut self, nonce: CommitmentNonce) {
        self.last_committed_remote_nonce = Some(nonce);
    }

    pub fn get_current_commitment_numbers(&self) -> CommitmentNumbers {
        self.commitment_numbers
    }

    pub fn get_local_commitment_number(&self) -> u64 {
        self.commitment_numbers.get_local()
    }

    pub fn get_remote_commitment_number(&self) -> u64 {
        self.commitment_numbers.get_remote()
    }

    pub fn increment_local_commitment_number(&mut self) {
        self.commitment_numbers.increment_local();
    }

    pub fn increment_remote_commitment_number(&mut self) {
        self.commitment_numbers.increment_remote();
    }

    pub fn get_current_commitment_number(&self, for_remote: bool) -> u64 {
        if for_remote {
            self.get_local_commitment_number()
        } else {
            self.get_remote_commitment_number()
        }
    }

    pub fn get_next_commitment_number(&self, for_remote: bool) -> u64 {
        self.get_current_commitment_number(for_remote) + 1
    }

    pub fn get_next_offering_tlc_id(&self) -> TLCId {
        TLCId::Offered(self.tlc_state.get_next_offering())
    }

    pub fn get_next_received_tlc_id(&self) -> TLCId {
        TLCId::Received(self.tlc_state.get_next_received())
    }

    pub fn increment_next_offered_tlc_id(&mut self) {
        self.tlc_state.increment_offering();
    }

    pub fn increment_next_received_tlc_id(&mut self) {
        self.tlc_state.increment_received();
    }

    pub fn get_offered_tlc(&self, tlc_id: TLCId) -> Option<&TlcInfo> {
        self.tlc_state.get(&tlc_id)
    }

    pub fn get_received_tlc(&self, tlc_id: TLCId) -> Option<&TlcInfo> {
        self.tlc_state.get(&tlc_id)
    }

    pub fn check_insert_tlc(&mut self, tlc: &TlcInfo) -> ProcessingChannelResult {
        let next_tlc_id = if tlc.is_offered() {
            self.get_next_offering_tlc_id()
        } else {
            self.get_next_received_tlc_id()
        };
        if tlc.tlc_id != next_tlc_id {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Received tlc id {:?} is not the expected next id {:?}",
                tlc.tlc_id, next_tlc_id
            )));
        }
        Ok(())
    }

    // Remove a tlc with a reason. If the tlc is removed, then the channel
    // balance will be updated accordingly. Otherwise, it is guaranteed that
    // the channel state is not updated.
    pub fn remove_tlc_with_reason(
        &mut self,
        tlc_id: TLCId,
    ) -> Result<(TlcInfo, RemoveTlcReason), ProcessingChannelError> {
        let current = self.tlc_state.get_mut(&tlc_id).expect("TLC exists").clone();
        let reason = current
            .removed_reason
            .clone()
            .expect("expect removed_reason exist");

        assert!(matches!(
            current.status,
            TlcStatus::Inbound(InboundTlcStatus::RemoveAckConfirmed)
                | TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed)
        ));

        if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = &reason {
            let filled_payment_hash: Hash256 =
                current.hash_algorithm.hash(fulfill.payment_preimage).into();
            if current.payment_hash != filled_payment_hash {
                return Err(ProcessingChannelError::FinalIncorrectPreimage);
            }

            // update balance according to the tlc,
            // we already checked the amount is valid in handle_add_tlc_command and handle_add_tlc_peer_message
            // here we double confirm everything is correct with `checked_*` methods
            let (mut to_local_amount, mut to_remote_amount) =
                (self.to_local_amount, self.to_remote_amount);
            if current.is_offered() {
                to_local_amount = to_local_amount.checked_sub(current.amount).ok_or(
                    ProcessingChannelError::InternalError(format!(
                        "Cannot remove tlc {:?} with amount {} from local balance {}",
                        tlc_id, current.amount, to_local_amount
                    )),
                )?;
                to_remote_amount = to_remote_amount.checked_add(current.amount).ok_or(
                    ProcessingChannelError::InternalError(format!(
                        "Cannot remove tlc {:?} with amount {} from remote balance {}",
                        tlc_id, current.amount, to_remote_amount
                    )),
                )?;
            } else {
                to_local_amount = to_local_amount.checked_add(current.amount).ok_or(
                    ProcessingChannelError::InternalError(format!(
                        "Cannot remove tlc {:?} with amount {} from local balance {}",
                        tlc_id, current.amount, to_local_amount
                    )),
                )?;
                to_remote_amount = to_remote_amount.checked_sub(current.amount).ok_or(
                    ProcessingChannelError::InternalError(format!(
                        "Cannot remove tlc {:?} with amount {} from remote balance {}",
                        tlc_id, current.amount, to_remote_amount
                    )),
                )?;
            }

            self.to_local_amount = to_local_amount;
            self.to_remote_amount = to_remote_amount;

            debug!("Updated local balance to {} and remote balance to {} by removing tlc {:?} with reason {:?}",
                            to_local_amount, to_remote_amount, tlc_id, reason);
            self.tlc_state.apply_remove_tlc(tlc_id);
        }
        debug!(
            "Removed tlc payment_hash {:?} with reason {:?}",
            current.payment_hash, reason
        );

        Ok((current.clone(), reason))
    }

    pub fn clean_up_failed_tlcs(&mut self) {
        // Remove the oldest failed tlcs from the channel state turns out to be very tricky
        // Because the different parties may have different views on the failed tlcs,
        // so we need to be very careful here.

        // The basic idea is to remove the oldest failed tlcs that are confirmed by both parties.
        // And we need to calculate the oldest failed tlcs independently from two directions,
        // Because we may have tlc operations from both directions at the same time, order matters.
        // see #475 for more details.
        let failed_offered_tlcs = self.tlc_state.offered_tlcs.get_oldest_failed_tlcs();
        let failed_received_tlcs = self.tlc_state.received_tlcs.get_oldest_failed_tlcs();

        for tlc_id in failed_offered_tlcs
            .iter()
            .chain(failed_received_tlcs.iter())
        {
            debug_assert!(self.tlc_state.applied_remove_tlcs.contains(tlc_id));
            self.tlc_state.apply_remove_tlc(*tlc_id);
        }
    }

    pub fn get_local_channel_public_keys(&self) -> &ChannelBasePublicKeys {
        &self.local_channel_public_keys
    }

    pub fn get_remote_channel_public_keys(&self) -> &ChannelBasePublicKeys {
        self.remote_channel_public_keys
            .as_ref()
            .expect("remote channel public keys exist")
    }

    pub fn must_get_funding_transaction(&self) -> &Transaction {
        self.funding_tx
            .as_ref()
            .expect("Funding transaction is present")
    }

    pub fn get_funding_transaction_outpoint(&self) -> Option<OutPoint> {
        self.funding_tx.as_ref().map(|tx| {
            // By convention, the funding tx output for the channel is the first output.
            OutPoint::new(tx.calc_tx_hash(), 0)
        })
    }

    pub fn must_get_funding_transaction_outpoint(&self) -> OutPoint {
        self.get_funding_transaction_outpoint()
            .expect("Funding transaction outpoint is present")
    }

    pub fn must_get_funding_transaction_timestamp(&self) -> u64 {
        self.funding_tx_confirmed_at
            .as_ref()
            .expect("Funding transaction confirmed at present")
            .2
    }

    pub fn get_local_shutdown_script(&self) -> Script {
        self.local_shutdown_script.clone()
    }

    pub fn get_remote_shutdown_script(&self) -> Script {
        self.remote_shutdown_script
            .as_ref()
            .expect("remote_shutdown_script should be set in current state")
            .clone()
    }

    fn get_local_commitment_point(&self, commitment_number: u64) -> Pubkey {
        self.signer.get_commitment_point(commitment_number)
    }

    /// Get the counterparty commitment point for the given commitment number.
    fn get_remote_commitment_point(&self, commitment_number: u64) -> Pubkey {
        self.remote_commitment_points
            .iter()
            .find_map(|(number, point)| {
                if *number == commitment_number {
                    Some(*point)
                } else {
                    None
                }
            })
            .expect("remote commitment point should exist")
    }

    fn get_current_local_commitment_point(&self) -> Pubkey {
        self.get_local_commitment_point(self.get_remote_commitment_number())
    }

    pub fn get_funding_lock_script_xonly_key(&self) -> XOnlyPublicKey {
        let pubkey: secp256k1::PublicKey = self
            .get_deterministic_musig2_agg_context()
            .aggregated_pubkey();
        pubkey.into()
    }

    pub fn get_funding_lock_script_xonly(&self) -> [u8; 32] {
        self.get_deterministic_musig2_agg_context()
            .aggregated_pubkey::<Point>()
            .serialize_xonly()
    }

    pub fn get_funding_lock_script(&self) -> Script {
        let aggregated_pubkey = self.get_funding_lock_script_xonly();
        let pubkey_hash = blake2b_256(aggregated_pubkey);
        get_script_by_contract(Contract::FundingLock, &pubkey_hash[0..20])
    }

    pub fn get_funding_request(&self) -> FundingRequest {
        FundingRequest {
            script: self.get_funding_lock_script(),
            udt_type_script: self.funding_udt_type_script.clone(),
            local_amount: self.to_local_amount,
            funding_fee_rate: self.funding_fee_rate,
            remote_amount: self.to_remote_amount,
            local_reserved_ckb_amount: self.local_reserved_ckb_amount,
            remote_reserved_ckb_amount: self.remote_reserved_ckb_amount,
        }
    }

    pub fn get_deterministic_musig2_agg_context(&self) -> KeyAggContext {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        let keys = self.order_things_for_musig2(local_pubkey, remote_pubkey);
        KeyAggContext::new(keys).expect("Valid pubkeys")
    }

    pub fn get_channel_announcement_musig2_secnonce(&self) -> SecNonce {
        let seckey = blake2b_hash_with_salt(
            self.signer.musig2_base_nonce.as_ref(),
            b"channel_announcement".as_slice(),
        );
        SecNonce::build(seckey).build()
    }

    pub fn get_channel_announcement_musig2_pubnonce(&self) -> PubNonce {
        self.get_channel_announcement_musig2_secnonce()
            .public_nonce()
    }

    fn get_init_revocation_nonce(&self) -> RevocationNonce {
        let [revoke, ack] = self
            .signer
            .derive_musig2_nonce(2, [Musig2Context::Revoke, Musig2Context::Ack]);
        RevocationNonce {
            revoke: revoke.public_nonce(),
            ack: ack.public_nonce(),
        }
    }

    fn get_commitment_nonce(&self) -> CommitmentNonce {
        let [funding, commitment] = self.signer.derive_musig2_nonce(
            self.get_local_commitment_number(),
            [Musig2Context::Funding, Musig2Context::Commitment],
        );
        CommitmentNonce {
            funding: funding.public_nonce(),
            commitment: commitment.public_nonce(),
        }
    }

    fn get_revocation_nonce(&self, for_remote: bool) -> RevocationNonce {
        let commitment_number = if for_remote {
            self.get_local_commitment_number()
        } else {
            self.get_remote_commitment_number()
        };
        let [revoke, ack] = self.signer.derive_musig2_nonce(
            commitment_number,
            [Musig2Context::Revoke, Musig2Context::Ack],
        );
        RevocationNonce {
            revoke: revoke.public_nonce(),
            ack: ack.public_nonce(),
        }
    }

    fn get_local_nonce_funding(&self) -> PubNonce {
        self.signer
            .derive_musig2_nonce(self.get_local_commitment_number(), [Musig2Context::Funding])[0]
            .public_nonce()
    }

    pub fn get_deterministic_musig2_agg_pubnonce(
        &self,
        local_nonce: PubNonce,
        remote_nonce: PubNonce,
    ) -> AggNonce {
        let nonces = self.order_things_for_musig2(local_nonce, remote_nonce);
        AggNonce::sum(nonces)
    }

    fn get_active_received_tlcs(&self, for_remote: bool) -> Vec<TlcInfo> {
        self.tlc_state
            .commitment_signed_tlcs(for_remote)
            .filter(|tlc| tlc.is_received())
            .cloned()
            .collect()
    }

    fn get_active_offered_tlcs(&self, for_remote: bool) -> Vec<TlcInfo> {
        self.tlc_state
            .commitment_signed_tlcs(for_remote)
            .filter(|tlc| tlc.is_offered())
            .cloned()
            .collect()
    }

    pub fn get_all_received_tlcs(&self) -> impl Iterator<Item = &TlcInfo> {
        self.tlc_state.all_tlcs().filter(|tlc| tlc.is_received())
    }

    pub fn get_all_offer_tlcs(&self) -> impl Iterator<Item = &TlcInfo> {
        self.tlc_state.all_tlcs().filter(|tlc| tlc.is_offered())
    }

    // Get the pubkeys for the tlc. Tlc pubkeys are the pubkeys held by each party
    // while this tlc was created (pubkeys are derived from the commitment number
    // when this tlc was created). The pubkeys returned here are sorted.
    // The offerer who offered this tlc will have the first pubkey, and the receiver
    // will have the second pubkey.
    // This tlc must have valid local_committed_at and remote_committed_at fields.
    pub fn get_tlc_pubkeys(&self, tlc: &TlcInfo) -> (Pubkey, Pubkey) {
        let CommitmentNumbers {
            local: local_commitment_number,
            remote: remote_commitment_number,
        } = tlc.get_commitment_numbers();
        let local_pubkey = derive_tlc_pubkey(
            &self.get_local_channel_public_keys().tlc_base_key,
            &self.get_local_commitment_point(remote_commitment_number),
        );
        let remote_pubkey = derive_tlc_pubkey(
            &self.get_remote_channel_public_keys().tlc_base_key,
            &self.get_remote_commitment_point(local_commitment_number),
        );
        (local_pubkey, remote_pubkey)
    }

    fn get_tlc_keys(&self, tlc: &TlcInfo) -> (Privkey, Pubkey) {
        let CommitmentNumbers {
            local: local_commitment_number,
            remote: remote_commitment_number,
        } = tlc.get_commitment_numbers();

        (
            self.signer.derive_tlc_key(remote_commitment_number),
            derive_tlc_pubkey(
                &self.get_remote_channel_public_keys().tlc_base_key,
                &self.get_remote_commitment_point(local_commitment_number),
            ),
        )
    }

    fn get_active_tlcs(&self, for_remote: bool) -> Vec<TlcInfo> {
        // Build a sorted array of TLC so that both party can generate the same commitment transaction.
        let (mut received_tlcs, mut offered_tlcs) = (
            self.get_active_received_tlcs(for_remote),
            self.get_active_offered_tlcs(for_remote),
        );
        let (mut a, mut b) = if for_remote {
            (received_tlcs, offered_tlcs)
        } else {
            for tlc in received_tlcs.iter_mut().chain(offered_tlcs.iter_mut()) {
                // Need to flip these fields for the counterparty.
                tlc.flip_mut();
            }
            (offered_tlcs, received_tlcs)
        };
        a.sort_by(|x, y| u64::from(x.tlc_id).cmp(&u64::from(y.tlc_id)));
        b.sort_by(|x, y| u64::from(x.tlc_id).cmp(&u64::from(y.tlc_id)));
        [a, b].concat()
    }

    fn get_active_tlcs_for_commitment(&self, for_remote: bool) -> Vec<u8> {
        let tlcs = self.get_active_tlcs(for_remote);
        if tlcs.is_empty() {
            Vec::new()
        } else {
            let mut result = vec![tlcs.len() as u8];
            for tlc in tlcs {
                let (local_key, remote_key) = self.get_tlc_pubkeys(&tlc);
                result.extend_from_slice(&tlc.get_htlc_type().to_le_bytes());
                result.extend_from_slice(&tlc.amount.to_le_bytes());
                result.extend_from_slice(&tlc.get_hash());
                if for_remote {
                    result.extend_from_slice(blake160(&remote_key.serialize()).as_ref());
                    result.extend_from_slice(blake160(&local_key.serialize()).as_ref());
                } else {
                    result.extend_from_slice(blake160(&local_key.serialize()).as_ref());
                    result.extend_from_slice(blake160(&remote_key.serialize()).as_ref());
                }
                result.extend_from_slice(
                    &Since::new(SinceType::Timestamp, tlc.expiry / 1000, false)
                        .value()
                        .to_le_bytes(),
                );
            }
            result
        }
    }

    fn get_active_tlcs_for_settlement(&self, for_remote: bool) -> Vec<SettlementTlc> {
        let tlcs = self.get_active_tlcs(for_remote);
        tlcs.into_iter()
            .map(|tlc| {
                let (local_key, remote_key) = self.get_tlc_keys(&tlc);
                SettlementTlc {
                    tlc_id: tlc.tlc_id,
                    hash_algorithm: tlc.hash_algorithm,
                    payment_amount: tlc.amount,
                    payment_hash: tlc.payment_hash,
                    expiry: tlc.expiry,
                    local_key,
                    remote_key,
                }
            })
            .collect()
    }

    pub fn any_tlc_pending(&self) -> bool {
        self.tlc_state
            .all_tlcs()
            .any(|tlc| tlc.removed_confirmed_at.is_none())
    }

    pub fn get_local_funding_pubkey(&self) -> &Pubkey {
        &self.get_local_channel_public_keys().funding_pubkey
    }

    pub fn get_remote_funding_pubkey(&self) -> &Pubkey {
        &self.get_remote_channel_public_keys().funding_pubkey
    }

    fn check_shutdown_fee_valid(&self, remote_fee_rate: u64) -> bool {
        let remote_shutdown_fee = calculate_shutdown_tx_fee(
            remote_fee_rate,
            &self.funding_udt_type_script,
            (
                self.get_remote_shutdown_script(),
                self.get_local_shutdown_script(),
            ),
        );
        let occupied_capacity = match occupied_capacity(
            &self.get_remote_shutdown_script(),
            &self.funding_udt_type_script,
        ) {
            Ok(capacity) => capacity.as_u64(),
            Err(_) => return false,
        };
        let remote_available_max_fee = if self.funding_udt_type_script.is_none() {
            (self.to_remote_amount as u64 + self.remote_reserved_ckb_amount)
                .saturating_sub(occupied_capacity)
        } else {
            self.remote_reserved_ckb_amount
                .saturating_sub(occupied_capacity)
        };
        return remote_shutdown_fee <= remote_available_max_fee;
    }

    fn check_valid_to_auto_accept_shutdown(&self) -> bool {
        self.remote_shutdown_info
            .as_ref()
            .is_some_and(|i| i.fee_rate >= self.commitment_fee_rate)
    }

    fn check_tlc_expiry(&self, expiry: u64) -> ProcessingChannelResult {
        let current_time = now_timestamp_as_millis_u64();
        if expiry <= current_time + MIN_TLC_EXPIRY_DELTA {
            error!(
                "TLC expiry {} is too soon, current time: {}, MIN_TLC_EXPIRY_DELTA: {}",
                expiry, current_time, MIN_TLC_EXPIRY_DELTA
            );
            return Err(ProcessingChannelError::TlcExpirySoon);
        }
        let delay_epoch = EpochNumberWithFraction::from_full_value(self.commitment_delay_epoch);
        let epoch_delay_milliseconds =
            (delay_epoch.number() as f64 * MILLI_SECONDS_PER_EPOCH as f64 * 2.0 / 3.0) as u64;
        let pending_tlc_count = self
            .tlc_state
            .all_tlcs()
            .filter(|tlc| tlc.removed_confirmed_at.is_none())
            .count() as u64;
        debug!(
            "here debug pending_tlc_count: {} => delay: {}",
            pending_tlc_count,
            epoch_delay_milliseconds * (pending_tlc_count + 1)
        );
        let expect_expiry = current_time + epoch_delay_milliseconds * (pending_tlc_count + 1);
        if expiry < expect_expiry {
            error!(
                "TLC expiry {} is too soon, current time + epoch delay: {}",
                expiry, expect_expiry
            );
            return Err(ProcessingChannelError::TlcExpirySoon);
        }

        if expiry >= current_time + MAX_PAYMENT_TLC_EXPIRY_LIMIT {
            debug!(
                "TLC expiry {} is too far in the future, current time: {}",
                expiry, current_time
            );
            return Err(ProcessingChannelError::TlcExpiryTooFar);
        }

        Ok(())
    }

    fn check_tlc_forward_amount(
        &self,
        forward_amount: u128,
        forward_fee: Option<u128>,
    ) -> ProcessingChannelResult {
        if self.local_tlc_info.tlc_minimum_value != 0
            && forward_amount < self.local_tlc_info.tlc_minimum_value
        {
            return Err(ProcessingChannelError::TlcAmountIsTooLow);
        }
        let forward_fee = match forward_fee {
            Some(fee) => fee,
            None => {
                // We are not forwarding the tlc, so no need to check the fee.
                return Ok(());
            }
        };
        let fee_rate = self.local_tlc_info.tlc_fee_proportional_millionths;
        match calculate_tlc_forward_fee(forward_amount, fee_rate) {
            Ok(expected_fee) if forward_fee >= expected_fee => Ok(()),
            Ok(fee) => {
                error!(
                    "too low forward_fee: {}, expected_fee: {}",
                    forward_fee, fee
                );
                Err(ProcessingChannelError::TlcForwardFeeIsTooLow)
            }
            Err(e) => {
                error!("calculate_tlc_forward_fee error: {:?}", e);
                Err(ProcessingChannelError::TlcForwardFeeIsTooLow)
            }
        }
    }

    // Check whether the reason is valid for removing the tlc.
    fn check_remove_tlc_with_reason(
        &self,
        tlc_id: TLCId,
        reason: &RemoveTlcReason,
    ) -> ProcessingChannelResult {
        if let Some(tlc) = self.tlc_state.get(&tlc_id) {
            if tlc.removed_reason.is_some() {
                return Err(ProcessingChannelError::RepeatedProcessing(
                    "TLC is already removed".to_string(),
                ));
            }
            if (tlc.is_offered() && tlc.outbound_status() != OutboundTlcStatus::Committed)
                || (tlc.is_received() && tlc.inbound_status() != InboundTlcStatus::Committed)
            {
                return Err(ProcessingChannelError::InvalidState(
                    "TLC is not in Committed status".to_string(),
                ));
            }
            if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = reason {
                let filled_payment_hash: Hash256 =
                    tlc.hash_algorithm.hash(fulfill.payment_preimage).into();
                if tlc.payment_hash != filled_payment_hash {
                    // actually this branch should never be reached in normal case
                    // `FinalIncorrectPreimage` will be returned in `apply_add_tlc_operation_with_peeled_onion_packet`
                    // when the preimage is incorrect
                    return Err(ProcessingChannelError::FinalIncorrectPreimage);
                }
            }
            Ok(())
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Trying to remove non-existing tlc with id {:?}",
                tlc_id
            )));
        }
    }

    fn check_for_tlc_update(
        &self,
        add_tlc_amount: Option<u128>,
        is_tlc_command_message: bool,
        is_sent: bool,
    ) -> ProcessingChannelResult {
        match self.state {
            ChannelState::ChannelReady => {}
            ChannelState::ShuttingDown(flags)
                if add_tlc_amount.is_none()
                    || (!is_sent && flags == ShuttingDownFlags::OUR_SHUTDOWN_SENT) =>
            {
                // when we've sent out shutting down command,
                // we can only remove tlc or process add_tlc peer message
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid state {:?} for {} tlc",
                    self.state,
                    if add_tlc_amount.is_some() {
                        "adding"
                    } else {
                        "removing"
                    }
                )))
            }
        }

        // don't check whether channel is public,
        // since private channel could also forward tlc
        if let Some(add_amount) = add_tlc_amount {
            if is_tlc_command_message && !self.local_tlc_info.enabled {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "TLC forwarding is not enabled for channel {}",
                    self.get_id()
                )));
            }
            self.check_tlc_limits(add_amount, is_sent)?;
        }

        if is_tlc_command_message && (self.is_waiting_tlc_ack() || self.reestablishing) {
            return Err(ProcessingChannelError::WaitingTlcAck);
        }

        Ok(())
    }

    fn is_waiting_tlc_ack(&self) -> bool {
        self.tlc_state.waiting_ack
            || (self.remote_revocation_nonce_for_send.is_none()
                || self.remote_revocation_nonce_for_verify.is_none())
    }

    fn check_tlc_limits(&self, add_amount: u128, is_sent: bool) -> ProcessingChannelResult {
        if add_amount == 0 {
            return Err(ProcessingChannelError::TlcAmountIsTooLow);
        }
        if is_sent {
            // local peer can not sent more tlc amount than they have
            let pending_sent_amount = self.get_offered_tlc_balance();
            if add_amount > self.to_local_amount.saturating_sub(pending_sent_amount) {
                return Err(ProcessingChannelError::TlcAmountExceedLimit);
            }

            let active_offered_tls_number = self.get_all_offer_tlcs().count() as u64 + 1;
            if active_offered_tls_number > self.local_constraints.max_tlc_number_in_flight {
                return Err(ProcessingChannelError::TlcNumberExceedLimit);
            }

            let active_offered_amount = self
                .get_all_offer_tlcs()
                .fold(0_u128, |sum, tlc| sum + tlc.amount)
                + add_amount;
            if active_offered_amount > self.local_constraints.max_tlc_value_in_flight {
                return Err(ProcessingChannelError::TlcValueInflightExceedLimit);
            }
        } else {
            // remote peer can not sent more tlc amount than they have
            let pending_recv_amount = self.get_received_tlc_balance();
            if add_amount > self.to_remote_amount.saturating_sub(pending_recv_amount) {
                return Err(ProcessingChannelError::TlcAmountExceedLimit);
            }

            let active_received_tls_number = self.get_all_received_tlcs().count() as u64 + 1;
            if active_received_tls_number > self.remote_constraints.max_tlc_number_in_flight {
                return Err(ProcessingChannelError::TlcNumberExceedLimit);
            }

            let active_received_amount = self
                .get_all_received_tlcs()
                .fold(0_u128, |sum, tlc| sum + tlc.amount)
                + add_amount;
            if active_received_amount > self.remote_constraints.max_tlc_value_in_flight {
                return Err(ProcessingChannelError::TlcValueInflightExceedLimit);
            }
        }

        Ok(())
    }

    fn create_outbounding_tlc(&self, command: &AddTlcCommand) -> TlcInfo {
        let tlc_id = self.get_next_offering_tlc_id();
        assert!(
            self.get_offered_tlc(tlc_id).is_none(),
            "Must not have the same id in pending offered tlcs"
        );

        TlcInfo {
            channel_id: self.get_id(),
            status: TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
            tlc_id,
            attempt_id: command.attempt_id,
            amount: command.amount,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            hash_algorithm: command.hash_algorithm,
            created_at: self.get_current_commitment_numbers(),
            removed_reason: None,
            onion_packet: command.onion_packet.clone(),
            shared_secret: command.shared_secret,
            previous_tlc: command.previous_tlc.map(|prev_tlc| {
                (
                    prev_tlc.prev_channel_id,
                    TLCId::Received(prev_tlc.prev_tlc_id),
                )
            }),
            removed_confirmed_at: None,
            total_amount: None,
            payment_secret: None,
        }
    }

    fn create_inbounding_tlc(&self, message: AddTlc) -> Result<TlcInfo, ProcessingChannelError> {
        let tlc_info = TlcInfo {
            tlc_id: TLCId::Received(message.tlc_id),
            status: TlcStatus::Inbound(InboundTlcStatus::RemoteAnnounced),
            channel_id: self.get_id(),
            amount: message.amount,
            payment_hash: message.payment_hash,
            attempt_id: None,
            expiry: message.expiry,
            hash_algorithm: message.hash_algorithm,
            // will be set when apply AddTlc operations after the signature is checked
            onion_packet: message.onion_packet,
            // No need to save shared secret for inbound TLC.
            shared_secret: NO_SHARED_SECRET,
            created_at: self.get_current_commitment_numbers(),
            removed_reason: None,
            previous_tlc: None,
            removed_confirmed_at: None,
            total_amount: None,
            payment_secret: None,
        };
        Ok(tlc_info)
    }

    fn aggregate_partial_signatures_to_consume_funding_cell(
        &self,
        common_ctx: &Musig2CommonContext,
        our_partial_signature: PartialSignature,
        their_partial_signature: PartialSignature,
        tx: &TransactionView,
    ) -> Result<TransactionView, ProcessingChannelError> {
        let signature = common_ctx.aggregate_partial_signatures_for_msg(
            our_partial_signature,
            their_partial_signature,
            &compute_tx_message(tx),
        )?;

        let witness =
            create_witness_for_funding_cell(self.get_funding_lock_script_xonly(), signature);
        Ok(tx
            .as_advanced_builder()
            .set_witnesses(vec![witness.pack()])
            .build())
    }

    fn complete_partially_signed_tx(
        &self,
        psct: &PartiallySignedCommitmentTransaction,
    ) -> Result<(TransactionView, SettlementData), ProcessingChannelError> {
        let completed_commitment_tx = {
            let sign_ctx = self.get_funding_sign_context();
            let signature = sign_ctx.sign_and_aggregate(
                &compute_tx_message(&psct.commitment_tx),
                psct.funding_tx_partial_signature,
            )?;
            let witness =
                create_witness_for_funding_cell(self.get_funding_lock_script_xonly(), signature);
            psct.commitment_tx
                .as_advanced_builder()
                .set_witnesses(vec![witness.pack()])
                .build()
        };

        let settlement_data = {
            let sign_ctx = self.get_commitment_sign_context(false);
            let x_only_aggregated_pubkey = sign_ctx.common_ctx.x_only_aggregated_pubkey();

            let settlement_tx = &psct.settlement_tx;
            let commitment_tx = &psct.commitment_tx;
            let to_local_output = settlement_tx
                .outputs()
                .get(0)
                .expect("get output 0 of settlement tx");
            let to_local_output_data = settlement_tx
                .outputs_data()
                .get(0)
                .expect("get output 0 data of settlement tx");
            let to_remote_output = settlement_tx
                .outputs()
                .get(1)
                .expect("get output 1 of settlement tx");
            let to_remote_output_data = settlement_tx
                .outputs_data()
                .get(1)
                .expect("get output 1 data of settlement tx");
            let args = commitment_tx
                .outputs()
                .get(0)
                .expect("get output 0 of commitment tx")
                .lock()
                .args()
                .raw_data();
            let message = blake2b_256(
                [
                    to_local_output.as_slice(),
                    to_local_output_data.as_slice(),
                    to_remote_output.as_slice(),
                    to_remote_output_data.as_slice(),
                    &args[0..36],
                ]
                .concat(),
            );
            let aggregated_signature = sign_ctx
                .sign_and_aggregate(message.as_slice(), psct.commitment_tx_partial_signature)?;

            SettlementData {
                x_only_aggregated_pubkey,
                aggregated_signature,
                to_local_output,
                to_local_output_data,
                to_remote_output,
                to_remote_output_data,
                tlcs: self.get_active_tlcs_for_settlement(false),
            }
        };

        Ok((completed_commitment_tx, settlement_data))
    }

    async fn maybe_transfer_to_shutdown(&mut self) -> ProcessingChannelResult {
        // This function will also be called when we resolve all pending tlcs.
        // If we are not in the ShuttingDown state, we should not do anything.
        let flags = match self.state {
            ChannelState::ShuttingDown(flags) => flags,
            _ => {
                return Ok(());
            }
        };

        #[cfg(debug_assertions)]
        self.tlc_state.debug();
        if !flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) || self.any_tlc_pending() {
            debug!("Will not shutdown the channel because we require all tlcs resolved");
            return Ok(());
        }

        debug!("All pending tlcs are resolved, transitioning to Shutdown state");
        self.update_state(ChannelState::ShuttingDown(
            flags | ShuttingDownFlags::DROPPING_PENDING,
        ));
        self.clear_waiting_peer_response();

        if self.local_shutdown_info.is_some() && self.remote_shutdown_info.is_some() {
            let shutdown_tx = self.build_shutdown_tx().await?;
            let sign_ctx = self.get_funding_sign_context();

            let local_shutdown_info = self
                .local_shutdown_info
                .as_mut()
                .expect("local shutdown info exists");
            let remote_shutdown_info = self
                .remote_shutdown_info
                .as_ref()
                .expect("remote shutdown info exists");
            let shutdown_scripts = (
                local_shutdown_info.close_script.clone(),
                remote_shutdown_info.close_script.clone(),
            );
            let local_shutdown_signature = match local_shutdown_info.signature {
                Some(signature) => signature,
                None => {
                    let signature = sign_ctx.sign(&compute_tx_message(&shutdown_tx))?;
                    local_shutdown_info.signature = Some(signature);

                    self.network()
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                                self.get_remote_peer_id(),
                                FiberMessage::closing_signed(ClosingSigned {
                                    partial_signature: signature,
                                    channel_id: self.get_id(),
                                }),
                            )),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    signature
                }
            };

            if let Some(remote_shutdown_signature) = remote_shutdown_info.signature {
                let tx: TransactionView = self
                    .aggregate_partial_signatures_to_consume_funding_cell(
                        &sign_ctx.common_ctx,
                        local_shutdown_signature,
                        remote_shutdown_signature,
                        &shutdown_tx,
                    )?;
                assert_eq!(
                    tx.data().serialized_size_in_block(),
                    shutdown_tx_size(&self.funding_udt_type_script, shutdown_scripts)
                );

                self.update_state(ChannelState::ShuttingDown(
                    ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
                ));

                self.network()
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ClosingTransactionPending(
                            self.get_id(),
                            self.get_remote_peer_id(),
                            tx,
                            false,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            } else {
                debug!("We have sent our shutdown signature, waiting for counterparty's signature");
            }
        } else {
            debug!("Not ready to shutdown the channel, waiting for both parties to send the Shutdown message");
        }

        Ok(())
    }

    fn handle_accept_channel_message(
        &mut self,
        accept_channel: AcceptChannel,
    ) -> ProcessingChannelResult {
        if self.state != ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT) {
            return Err(ProcessingChannelError::InvalidState(format!(
                "accepting a channel while in state {:?}, expecting NegotiatingFundingFlags::OUR_INIT_SENT",
                self.state
            )));
        }

        self.update_state(ChannelState::NegotiatingFunding(
            NegotiatingFundingFlags::INIT_SENT,
        ));

        self.to_remote_amount = accept_channel.funding_amount;
        self.remote_reserved_ckb_amount = accept_channel.reserved_ckb_amount;

        self.commit_remote_nonce(accept_channel.next_commitment_nonce.clone());
        self.remote_revocation_nonce_for_send = Some(accept_channel.next_revocation_nonce.clone());
        self.remote_revocation_nonce_for_verify =
            Some(accept_channel.next_revocation_nonce.clone());
        let remote_pubkeys = (&accept_channel).into();
        self.remote_channel_public_keys = Some(remote_pubkeys);
        self.remote_commitment_points = vec![
            (1, accept_channel.first_per_commitment_point),
            (2, accept_channel.second_per_commitment_point),
        ];
        self.remote_shutdown_script = Some(accept_channel.shutdown_script.clone());

        self.remote_constraints = ChannelConstraints::new(
            accept_channel.max_tlc_value_in_flight,
            accept_channel.max_tlc_number_in_flight,
        );

        self.check_accept_channel_parameters()?;

        match accept_channel.channel_announcement_nonce {
            Some(ref nonce) if self.is_public() => {
                debug!("Updating remote channel announcement nonce: {:?}", nonce);
                self.update_remote_channel_announcement_nonce(nonce);
            }
            None if !self.is_public() => {}
            _ => {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Must/Mustn't send announcement nonce if channel is public/private, nonce {:?}, channel is public: {}",
                    &accept_channel.channel_announcement_nonce, self.is_public()
                )));
            }
        }
        debug!(
            "Successfully processed AcceptChannel message {:?}",
            &accept_channel
        );
        Ok(())
    }

    // This is the dual of `handle_tx_collaboration_command`. Any logic error here is likely
    // to present in the other function as well.
    async fn handle_tx_collaboration_msg(
        &mut self,
        myself: &ActorRef<ChannelActorMessage>,
        msg: TxCollaborationMsg,
    ) -> ProcessingChannelResult {
        debug!("Processing tx collaboration message: {:?}", &msg);
        let network = self.network();
        let is_complete_message = matches!(msg, TxCollaborationMsg::TxComplete(_));
        let is_waiting_for_remote = match self.state {
            ChannelState::CollaboratingFundingTx(flags) => {
                flags.contains(CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG)
            }
            _ => false,
        };
        let flags = match self.state {
            // Starting transaction collaboration
            ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT)
                if !self.is_acceptor =>
            {
                return Err(ProcessingChannelError::InvalidState(
                    "Initiator received a tx collaboration message".to_string(),
                ));
            }
            ChannelState::NegotiatingFunding(_) => {
                debug!("Started negotiating funding tx collaboration, and transitioning from {:?} to CollaboratingFundingTx state", self.state);
                self.state =
                    ChannelState::CollaboratingFundingTx(CollaboratingFundingTxFlags::empty());
                CollaboratingFundingTxFlags::empty()
            }
            ChannelState::CollaboratingFundingTx(_)
                if !is_complete_message && !is_waiting_for_remote =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Trying to process message {:?} while in {:?} (should only receive non-complete message after sent response from peer)",
                    &msg, self.state
                )));
            }
            ChannelState::CollaboratingFundingTx(flags) => {
                if flags.contains(CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT) {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Received a tx collaboration message {:?}, but we are already in the state {:?} where the remote has sent a complete message",
                        &msg, &self.state
                    )));
                }
                debug!(
                    "Processing tx collaboration message {:?} for state {:?}",
                    &msg, &self.state
                );
                flags
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid tx collaboration message {:?} for state {:?}",
                    &msg, &self.state
                )));
            }
        };
        match msg {
            TxCollaborationMsg::TxUpdate(msg) => {
                if let Err(err) = call!(network, |tx| NetworkActorMessage::Command(
                    NetworkActorCommand::VerifyFundingTx {
                        local_tx: self.funding_tx.clone().unwrap_or_default(),
                        remote_tx: msg.tx.clone(),
                        funding_cell_lock_script: self.get_funding_lock_script(),
                        reply: tx
                    }
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE)
                {
                    error!(
                        "fails to verify the TxUpdate message from the peer: {}",
                        err
                    );
                    if !err.is_temporary() {
                        myself
                            .send_message(ChannelActorMessage::Event(ChannelEvent::Stop(
                                StopReason::AbortFunding,
                            )))
                            .expect("myself alive");
                    }
                    return Ok(());
                }

                self.funding_tx = Some(msg.tx.clone());
                if self.is_tx_final(&msg.tx)? {
                    self.maybe_complete_tx_collaboration(msg.tx)?;
                } else {
                    network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::UpdateChannelFunding(
                                self.get_id(),
                                msg.tx,
                                self.get_funding_request(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    self.update_state(ChannelState::CollaboratingFundingTx(
                        CollaboratingFundingTxFlags::PREPARING_LOCAL_TX_COLLABORATION_MSG,
                    ));
                }
            }
            TxCollaborationMsg::TxComplete(tx_complete) => {
                self.check_tx_complete_preconditions()?;
                let settlement_data = self.check_init_commitment_tx_signature(
                    tx_complete.commitment_tx_partial_signature,
                )?;
                network
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::RemoteTxComplete(
                            self.get_remote_peer_id(),
                            self.get_id(),
                            self.get_funding_lock_script(),
                            settlement_data,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                let flags = flags | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT;
                self.commit_remote_nonce(tx_complete.next_commitment_nonce);
                self.increment_remote_commitment_number();
                self.update_state(ChannelState::CollaboratingFundingTx(flags));
            }
        }
        Ok(())
    }

    fn verify_commitment_signed_and_send_ack(
        &mut self,
        commitment_signed: CommitmentSigned,
    ) -> ProcessingChannelResult {
        let flags = match self.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABORATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to verify commitment_signed message in state {:?}, as collaboration is not completed yet.",
                    &self.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                CommitmentSignedFlags::SigningCommitment(SigningCommitmentFlags::empty())
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to verify commitment_signed message in state {:?}, as we have already received our commitment_signed message.",
                    &self.state
                )));
            }
            ChannelState::SigningCommitment(flags) => {
                CommitmentSignedFlags::SigningCommitment(flags)
            }
            ChannelState::ChannelReady => CommitmentSignedFlags::ChannelReady(),
            ChannelState::ShuttingDown(flags) => {
                if flags.is_ok_for_commitment_operation() {
                    debug!(
                        "Verify commitment_signed message while shutdown is pending, current state {:?}",
                        &self.state
                    );
                    CommitmentSignedFlags::PendingShutdown()
                } else {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Unable to verify commitment_signed message in shutdowning state with flags {:?}",
                        &flags
                    )));
                }
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to verify commitment signed message in state {:?}",
                    &self.state
                )));
            }
        };

        self.clean_up_failed_tlcs();

        #[cfg(debug_assertions)]
        {
            debug!(
                "verify commitment_signed: {:?} at commitment_numbers: {:?}",
                commitment_signed,
                self.get_current_commitment_numbers()
            );
        }

        let (commitment_tx, settlement_data) = self.verify_and_complete_tx(
            commitment_signed.funding_tx_partial_signature,
            commitment_signed.commitment_tx_partial_signature,
        )?;

        // Notify outside observers.
        self.network()
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::RemoteCommitmentSigned(
                    self.get_remote_peer_id(),
                    self.get_id(),
                    commitment_tx.clone(),
                    settlement_data,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT;
                self.update_state(ChannelState::SigningCommitment(flags));
                self.maybe_transfer_to_tx_signatures(flags)?;
            }
            CommitmentSignedFlags::ChannelReady() | CommitmentSignedFlags::PendingShutdown() => {
                self.send_revoke_and_ack_message()?;
            }
        }
        self.commit_remote_nonce(commitment_signed.next_commitment_nonce);
        self.latest_commitment_transaction = Some(commitment_tx.data());
        Ok(())
    }

    fn maybe_transfer_to_tx_signatures(
        &mut self,
        flags: SigningCommitmentFlags,
    ) -> ProcessingChannelResult {
        if flags.contains(SigningCommitmentFlags::COMMITMENT_SIGNED_SENT) {
            debug!("Commitment signed message sent by both sides, transitioning to AwaitingTxSignatures state");
            self.update_state(ChannelState::AwaitingTxSignatures(
                AwaitingTxSignaturesFlags::empty(),
            ));
            if self.should_local_send_tx_signatures_first() {
                debug!("It is our turn to send tx_signatures, so we will do it now.");
                self.handle_tx_signatures(None)?;
            }
        }
        Ok(())
    }

    // TODO: currently witnesses in the tx_signatures molecule message are a list of bytes.
    // It is unclear how can we compose two partial sets witnesses into a complete
    // set of witnesses.
    fn handle_tx_signatures(
        &mut self,
        // If partial_witnesses is given, then it is the counterparty that send a message
        // to us, and we must combine them to make a full list of witnesses.
        // Otherwise, we are the one who is to start send the tx_signatures.
        // We can just create a partial set of witnesses, and sent them to the peer.
        partial_witnesses: Option<Vec<Vec<u8>>>,
    ) -> ProcessingChannelResult {
        let flags = match self.state {
            ChannelState::AwaitingTxSignatures(flags)
                if flags.contains(AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT)
                    && partial_witnesses.is_some() =>
            {
                return Err(ProcessingChannelError::RepeatedProcessing(format!(
                    "tx_signatures partial witnesses {:?}",
                    partial_witnesses
                )));
            }
            ChannelState::AwaitingTxSignatures(flags)
                if flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT)
                    && partial_witnesses.is_none() =>
            {
                return Err(ProcessingChannelError::RepeatedProcessing(
                    "We have already sent our tx_signatures".to_string(),
                ));
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::COMMITMENT_SIGNED_SENT) =>
            {
                AwaitingTxSignaturesFlags::empty()
            }
            ChannelState::AwaitingTxSignatures(flags) => flags,
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to build and sign funding tx in state {:?}",
                    &self.state
                )));
            }
        };

        let funding_tx = self
            .funding_tx
            .clone()
            .ok_or(ProcessingChannelError::InvalidState(
                "Funding transaction is not present".to_string(),
            ))?;

        if partial_witnesses.is_some() {
            self.update_state(ChannelState::AwaitingTxSignatures(
                flags | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT,
            ));
        }

        self.network()
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SignFundingTx(
                    self.get_remote_peer_id(),
                    self.get_id(),
                    funding_tx,
                    partial_witnesses,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        Ok(())
    }

    fn maybe_public_channel_is_ready(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        debug!("Trying to create channel announcement message for public channel");
        if let Some((channel_announcement, channel_update)) = self.try_create_channel_messages() {
            debug!(
                "Channel announcement/update message for {:?} created, public channel is ready",
                self.get_id(),
            );
            self.on_new_channel_ready(myself);

            debug!(
                "Broadcasting channel announcement {:?} and channel update {:?}",
                &channel_announcement, &channel_update
            );
            self.network()
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::BroadcastMessages(vec![
                        BroadcastMessageWithTimestamp::ChannelAnnouncement(
                            // here channel funding tx must be exists, otherwise the above
                            // `try_create_channel_messages` will return None
                            self.must_get_funding_transaction_timestamp(),
                            channel_announcement,
                        ),
                        BroadcastMessageWithTimestamp::ChannelUpdate(channel_update),
                    ]),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
    }

    fn maybe_channel_is_ready(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        match self.state {
            ChannelState::AwaitingChannelReady(flags) => {
                if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                    if !self.is_public() {
                        self.on_new_channel_ready(myself);
                    } else {
                        self.maybe_public_channel_is_ready(myself);
                    }
                }
            }
            _ => {
                panic!(
                    "Invalid state {:?} for maybe_on_channel_ready (expected AwaitingChannelReady)",
                    &self.state
                );
            }
        }
    }

    fn on_new_channel_ready(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        self.update_state(ChannelState::ChannelReady);
        self.increment_local_commitment_number();
        self.increment_remote_commitment_number();
        let peer_id = self.get_remote_peer_id();
        self.on_owned_channel_updated(myself, false);
        self.network()
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::ChannelReady(
                    self.get_id(),
                    peer_id.clone(),
                    self.must_get_funding_transaction_outpoint(),
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    fn on_reestablished_channel_ready(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        let Some(outpoint) = self.get_funding_transaction_outpoint() else {
            return;
        };

        self.reestablishing = false;

        // If the channel is already ready, we should notify the network actor.
        // so that we update the network.outpoint_channel_map
        let channel_id = self.get_id();
        let peer_id = self.get_remote_peer_id();
        self.network()
            .send_after(WAITING_REESTABLISH_FINISH_TIMEOUT, move || {
                NetworkActorMessage::new_event(NetworkActorEvent::ChannelReady(
                    channel_id, peer_id, outpoint,
                ))
            });
        self.on_owned_channel_updated(myself, false);
    }

    fn resume_funding(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        match self.state {
            ChannelState::AwaitingTxSignatures(mut flags) => {
                let channel_id = self.get_id();
                let funding_tx = self.must_get_funding_transaction().clone();
                let funding_cell_outpoint = self.must_get_funding_transaction_outpoint();
                let witnesses: Vec<Vec<u8>> = funding_tx
                    .witnesses()
                    .into_iter()
                    .map(|x| x.unpack())
                    .collect();

                if !flags.is_empty() {
                    // In the old code, signatures are not saved. Reset the state and resign the
                    // funding tx if signatures does not match the flags.
                    let placeholder_witness = packed::WitnessArgs::new_builder()
                        .lock(Some(molecule::bytes::Bytes::from(vec![0u8; 170])).pack())
                        .build()
                        .as_bytes()
                        .to_vec();
                    if witnesses.iter().any(|w| w == &placeholder_witness) {
                        flags = AwaitingTxSignaturesFlags::empty();
                        self.update_state(ChannelState::AwaitingTxSignatures(flags));
                    }
                }

                if flags.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) {
                    // Both signed
                    self.network()
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionPending(
                                funding_tx,
                                funding_cell_outpoint,
                                self.get_id(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    let peer_id = self.get_remote_peer_id();
                    let peer_message = FiberMessageWithPeerId {
                        peer_id,
                        message: FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::TxSignatures(TxSignatures {
                                channel_id,
                                witnesses,
                            }),
                        ),
                    };
                    self.network()
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(peer_message),
                        ))
                        .expect("network actor alive");
                } else if flags.contains(AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT)
                    || self.should_local_send_tx_signatures_first()
                {
                    // It's our turn to sign and send the signatures to the peer. If we have signed, resign it to reuse the code.
                    self.network()
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SignFundingTx(
                                self.get_remote_peer_id(),
                                self.get_id(),
                                funding_tx,
                                if self.should_local_send_tx_signatures_first() {
                                    None
                                } else {
                                    Some(witnesses)
                                },
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
            ChannelState::AwaitingChannelReady(flags) => {
                // It's turn to send the funding tx to chain and waiting for confirmations
                if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                    self.maybe_channel_is_ready(myself);
                } else {
                    if flags.contains(AwaitingChannelReadyFlags::OUR_CHANNEL_READY) {
                        // If we are ready, resend the ChannelReady message
                        self.network()
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                                    self.get_remote_peer_id(),
                                    FiberMessage::channel_ready(ChannelReady {
                                        channel_id: self.get_id(),
                                    }),
                                )),
                            ))
                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    }

                    // Trace the funding tx again
                    self.network()
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionPending(
                                self.must_get_funding_transaction().clone(),
                                self.must_get_funding_transaction_outpoint(),
                                self.get_id(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
            _ => {
                unreachable!("Expect resume funding when the channel state is in AwaitingTxSignatures | AwaitingChannelReady, but got {:?}", self.state);
            }
        }
    }

    fn append_remote_commitment_point(&mut self, commitment_point: Pubkey) {
        self.remote_commitment_points
            .push((self.get_local_commitment_number(), commitment_point));

        // shrink the remote commitment points list
        // TODO: use all_tlcs as filter instead of select the minimal commitment number
        let len = self.remote_commitment_points.len();
        if len > (self.local_constraints.max_tlc_number_in_flight + 1) as usize {
            let min_remote_commitment = self
                .tlc_state
                .all_tlcs()
                .map(|x| x.created_at.remote.min(x.created_at.local))
                .min()
                .unwrap_or_default();
            self.remote_commitment_points
                .retain(|(num, _)| *num >= min_remote_commitment);
        }
    }

    fn handle_revoke_and_ack_peer_message(
        &mut self,
        myself: &ActorRef<ChannelActorMessage>,
        revoke_and_ack: RevokeAndAck,
    ) -> ProcessingChannelResult {
        if !self.tlc_state.waiting_ack {
            return Err(ProcessingChannelError::InvalidState(
                "unexpected RevokeAndAck message".to_string(),
            ));
        }
        let RevokeAndAck {
            channel_id: _,
            revocation_partial_signature,
            commitment_tx_partial_signature,
            next_per_commitment_point,
            next_revocation_nonce,
        } = revoke_and_ack;

        let sign_ctx = match self.get_revoke_sign_context(true) {
            Some(ctx) => ctx,
            None => {
                let error =
                    "RevokeAndAck message received, but get_revoke_sign_context returned None";
                warn!(error);
                return Err(ProcessingChannelError::InvalidState(error.to_string()));
            }
        };
        let x_only_aggregated_pubkey = sign_ctx.common_ctx.x_only_aggregated_pubkey();

        let revocation_data = {
            let commitment_tx_fee = calculate_commitment_tx_fee(
                self.commitment_fee_rate,
                &self.funding_udt_type_script,
            );
            let lock_script = self.get_local_shutdown_script();
            let (output, output_data) = if let Some(udt_type_script) = &self.funding_udt_type_script
            {
                let capacity = self.get_total_reserved_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script.clone())
                    .type_(Some(udt_type_script.clone()).pack())
                    .capacity(capacity.pack())
                    .build();

                let output_data = self.get_liquid_capacity().to_le_bytes().pack();
                (output, output_data)
            } else {
                let capacity = self.get_total_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script.clone())
                    .capacity(capacity.pack())
                    .build();
                let output_data = Bytes::default();
                (output, output_data)
            };

            let commitment_number = self.get_local_commitment_number() - 1;
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                commitment_number.to_be_bytes().as_slice(),
            ]
            .concat();

            let message = blake2b_256(
                [
                    output.as_slice(),
                    output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );
            let aggregated_signature =
                sign_ctx.sign_and_aggregate(message.as_slice(), revocation_partial_signature)?;
            RevocationData {
                commitment_number,
                x_only_aggregated_pubkey,
                aggregated_signature,
                output,
                output_data,
            }
        };

        let settlement_data = {
            let (
                [to_local_output, to_remote_output],
                [to_local_output_data, to_remote_output_data],
            ) = self.build_settlement_transaction_outputs(true);
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                self.get_local_commitment_number().to_be_bytes().as_slice(),
            ]
            .concat();
            let message = blake2b_256(
                [
                    to_local_output.as_slice(),
                    to_local_output_data.as_slice(),
                    to_remote_output.as_slice(),
                    to_remote_output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );

            let sign_ctx = match self.get_ack_sign_context(true) {
                Some(ctx) => ctx,
                None => {
                    let error =
                        "RevokeAndAck message received, but get_ack_sign_context returned None";
                    warn!(error);
                    return Err(ProcessingChannelError::InvalidState(error.to_string()));
                }
            };
            let aggregated_signature =
                sign_ctx.sign_and_aggregate(message.as_slice(), commitment_tx_partial_signature)?;
            SettlementData {
                x_only_aggregated_pubkey,
                aggregated_signature,
                to_local_output,
                to_local_output_data,
                to_remote_output,
                to_remote_output_data,
                tlcs: self.get_active_tlcs_for_settlement(true),
            }
        };

        self.increment_local_commitment_number();
        self.append_remote_commitment_point(next_per_commitment_point);
        self.tlc_state
            .update_for_revoke_and_ack(self.commitment_numbers);
        self.set_waiting_ack(myself, false);

        // update the remote_revocation_nonce_for_send and remote_revocation_nonce_for_verify for next round if needed
        self.remote_revocation_nonce_for_next = Some(next_revocation_nonce);
        if self.remote_revocation_nonce_for_send.is_none() {
            self.remote_revocation_nonce_for_send = self.remote_revocation_nonce_for_next.clone();
            self.remote_revocation_nonce_for_verify = self.remote_revocation_nonce_for_next.clone();
        } else {
            self.remote_revocation_nonce_for_verify = None;
        }

        self.network()
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::RevokeAndAckReceived(
                    self.get_remote_peer_id(),
                    self.get_id(),
                    revocation_data,
                    settlement_data,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        Ok(())
    }

    fn handle_reestablish_channel_message(
        &mut self,
        myself: &ActorRef<ChannelActorMessage>,
        reestablish_channel: &ReestablishChannel,
    ) -> ProcessingChannelResult {
        debug!(
            "peer: {:?} Handling reestablish channel message: {:?}, our commitment_numbers {:?} in channel state {:?}",
            self.get_local_peer_id(),
            reestablish_channel, self.commitment_numbers, self.state
        );
        let network = self.network();
        self.notify_funding_tx(&network);
        match self.state {
            ChannelState::NegotiatingFunding(_flags) => {
                // TODO: in current implementation, we don't store the channel when we are in NegotiatingFunding state.
                // This is an unreachable state for reestablish channel message. we may need to handle this case in the future.
            }
            ChannelState::AwaitingTxSignatures(_) | ChannelState::AwaitingChannelReady(_) => {
                self.on_reestablished_channel_ready(myself);
                self.resume_funding(myself);
            }
            ChannelState::ChannelReady => {
                self.clear_waiting_peer_response();

                let my_local_commitment_number = self.get_local_commitment_number();
                let my_remote_commitment_number = self.get_remote_commitment_number();
                let my_waiting_ack = self.tlc_state.waiting_ack;
                let peer_local_commitment_number = reestablish_channel.local_commitment_number;
                let peer_remote_commitment_number = reestablish_channel.remote_commitment_number;

                warn!(
                    "peer: {:?} \
                    local_commitment_number ({:?}, {:?}) \
                    peer_commitment_number ({:?} {:?}) \
                    waiting_ack: {:?}",
                    self.get_local_peer_id(),
                    my_local_commitment_number,
                    my_remote_commitment_number,
                    peer_local_commitment_number,
                    peer_remote_commitment_number,
                    my_waiting_ack,
                );

                if peer_local_commitment_number.abs_diff(my_remote_commitment_number) > 1
                    || peer_remote_commitment_number.abs_diff(my_local_commitment_number) > 1
                {
                    return Err(ProcessingChannelError::InvalidParameter(
                        "reestablish channel message with invalid commitment numbers".to_string(),
                    ));
                }

                if my_local_commitment_number == peer_remote_commitment_number
                    && my_remote_commitment_number == peer_local_commitment_number
                {
                    // commitments are the same, sync up the tlcs
                    self.set_waiting_ack(myself, false);
                    self.resend_tlcs_on_reestablish(true)?;
                } else if my_remote_commitment_number == peer_local_commitment_number + 1 {
                    // peer need ACK, I need to send my revoke_and_ack message
                    // don't clear my waiting_ack flag here, since if i'm waiting for peer ack,
                    // peer will resend commitment_signed message
                    self.send_revoke_and_ack_message()?;
                    if my_waiting_ack && my_local_commitment_number == peer_remote_commitment_number
                    {
                        self.set_waiting_ack(myself, false);
                        self.resend_tlcs_on_reestablish(true)?;
                    }
                } else if my_waiting_ack
                    && my_local_commitment_number == peer_remote_commitment_number
                {
                    // I need to resend my commitment_signed message, don't clear my WaitingTlcAck flag
                    self.resend_tlcs_on_reestablish(true)?;
                } else {
                    // ignore, waiting for remote peer to resend revoke_and_ack
                }

                self.on_reestablished_channel_ready(myself);
                debug_event!(network, "Reestablished channel in ChannelReady");
            }
            _ => {
                // TODO: @quake we need to handle other states.
                warn!(
                    "Unhandled reestablish channel message in state {:?}",
                    &self.state
                );
            }
        }
        Ok(())
    }

    fn resend_tlcs_on_reestablish(&self, send_commitment_signed: bool) -> ProcessingChannelResult {
        let network = self.network();
        let mut need_commitment_signed = false;
        for info in self.tlc_state.all_tlcs() {
            if info.is_offered()
                && matches!(info.outbound_status(), OutboundTlcStatus::LocalAnnounced)
            {
                // resend AddTlc message
                network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            self.get_remote_peer_id(),
                            FiberMessage::add_tlc(AddTlc {
                                channel_id: self.get_id(),
                                tlc_id: info.tlc_id.into(),
                                amount: info.amount,
                                payment_hash: info.payment_hash,
                                expiry: info.expiry,
                                hash_algorithm: info.hash_algorithm,
                                onion_packet: info.onion_packet.clone(),
                            }),
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                debug_event!(network, "resend add tlc");
                need_commitment_signed = true;
            } else if let Some(remove_reason) = &info.removed_reason {
                if info.is_received()
                    && matches!(info.inbound_status(), InboundTlcStatus::LocalRemoved)
                {
                    // resend RemoveTlc message
                    network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                                self.get_remote_peer_id(),
                                FiberMessage::remove_tlc(RemoveTlc {
                                    channel_id: self.get_id(),
                                    tlc_id: info.tlc_id.into(),
                                    reason: remove_reason.clone(),
                                }),
                            )),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    debug_event!(network, "resend remove tlc");
                    need_commitment_signed = true;
                }
            }
        }

        if send_commitment_signed
            && (need_commitment_signed || self.tlc_state.need_another_commitment_signed())
        {
            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                        channel_id: self.get_id(),
                        command: ChannelCommand::CommitmentSigned(),
                    }),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
        Ok(())
    }

    fn is_tx_final(&self, tx: &Transaction) -> Result<bool, ProcessingChannelError> {
        let tx = tx.clone().into_view();

        let first_output = tx
            .outputs()
            .get(0)
            .ok_or(ProcessingChannelError::InvalidParameter(
                "Funding transaction should have at least one output".to_string(),
            ))?;

        if first_output.lock() != self.get_funding_lock_script() {
            return Err(ProcessingChannelError::InvalidState(
                "Invalid funding transaction lock script".to_string(),
            ));
        }

        let current_capacity: u64 = first_output.capacity().unpack();

        // make sure both parties have paid the reserved ckb amount
        if current_capacity <= self.local_reserved_ckb_amount
            || current_capacity <= self.remote_reserved_ckb_amount
        {
            return Ok(false);
        }

        if self.funding_udt_type_script.is_some() {
            let (_output, data) =
                tx.output_with_data(0)
                    .ok_or(ProcessingChannelError::InvalidParameter(
                        "Funding transaction should have at least one output".to_string(),
                    ))?;
            assert!(data.as_ref().len() >= 16);
            let mut amount_bytes = [0u8; 16];
            amount_bytes.copy_from_slice(&data.as_ref()[0..16]);
            let udt_amount = u128::from_le_bytes(amount_bytes);
            debug!(
                "udt_amount: {}, to_remote_amount: {}, to_local_amount: {}",
                udt_amount, self.to_remote_amount, self.to_local_amount
            );
            debug!("current_capacity: {}, remote_reserved_ckb_amount: {}, local_reserved_ckb_amount: {}",
                current_capacity, self.remote_reserved_ckb_amount, self.local_reserved_ckb_amount);
            let is_udt_amount_ok = udt_amount == self.get_liquid_capacity();
            return Ok(is_udt_amount_ok);
        } else {
            let is_complete = current_capacity == self.get_total_ckb_amount();
            Ok(is_complete)
        }
    }

    fn maybe_complete_tx_collaboration(&mut self, tx: Transaction) -> ProcessingChannelResult {
        let is_complete = self.is_tx_final(&tx)?;

        debug!(
            "Checking if funding transaction {:?} is complete: {}",
            &tx, is_complete
        );

        if is_complete {
            // We need to send a SendFiberMessage command here (instead of a ControlFiberChannel),
            // to guarantee that the TxComplete message immediately is sent to the network actor.
            // Otherwise, it is possible that when the network actor is processing ControlFiberChannel,
            // it receives another SendFiberMessage command, and that message (e.g. CommitmentSigned)
            // is processed first, thus breaking the order of messages.
            let commitment_tx_partial_signature = self.build_init_commitment_tx_signature()?;
            self.network()
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                        self.get_remote_peer_id(),
                        FiberMessage::tx_complete(TxComplete {
                            channel_id: self.get_id(),
                            next_commitment_nonce: self.get_next_commitment_nonce(),
                            commitment_tx_partial_signature,
                        }),
                    )),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            let old_flags = match self.state {
                ChannelState::CollaboratingFundingTx(flags) => flags,
                _ => {
                    panic!(
                        "Expect to be in CollaboratingFundingTx state while running update_funding_tx, current state {:?}", &self.state,
                    );
                }
            };
            self.update_state(ChannelState::CollaboratingFundingTx(
                old_flags | CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT,
            ));
        }
        Ok(())
    }

    fn build_init_commitment_tx_signature(&self) -> Result<PartialSignature, SigningError> {
        let sign_ctx = self.get_commitment_sign_context(false);
        let x_only_aggregated_pubkey = sign_ctx.common_ctx.x_only_aggregated_pubkey();
        let ([to_local_output, to_remote_output], [to_local_output_data, to_remote_output_data]) =
            self.build_settlement_transaction_outputs(false);
        let version = 1u64;
        let commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                commitment_lock_script_args.as_slice(),
            ]
            .concat(),
        );

        let signature = sign_ctx.sign(message.as_slice());
        signature
    }

    fn check_init_commitment_tx_signature(
        &self,
        signature: PartialSignature,
    ) -> Result<SettlementData, ProcessingChannelError> {
        let sign_ctx = self.get_commitment_sign_context(true);
        let x_only_aggregated_pubkey = sign_ctx.common_ctx.x_only_aggregated_pubkey();

        let ([to_local_output, to_remote_output], [to_local_output_data, to_remote_output_data]) =
            self.build_settlement_transaction_outputs(true);
        let version = 1u64;
        let commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                commitment_lock_script_args.as_slice(),
            ]
            .concat(),
        );

        let settlement_data = {
            let aggregated_signature =
                sign_ctx.sign_and_aggregate(message.as_slice(), signature)?;

            SettlementData {
                x_only_aggregated_pubkey,
                aggregated_signature,
                to_local_output,
                to_local_output_data,
                to_remote_output,
                to_remote_output_data,
                tlcs: vec![],
            }
        };
        Ok(settlement_data)
    }

    // TODO: More checks to the funding tx.
    fn check_tx_complete_preconditions(&mut self) -> ProcessingChannelResult {
        match self.funding_tx.as_ref() {
            None => {
                return Err(ProcessingChannelError::InvalidState(
                    "Received TxComplete message without a funding transaction".to_string(),
                ));
            }
            Some(tx) => {
                debug!(
                    "Received TxComplete message, funding tx is present {:?}",
                    tx
                );
                let check = self.is_tx_final(tx);
                if !check.is_ok_and(|ok| ok) {
                    return Err(ProcessingChannelError::InvalidState(
                        "Received TxComplete message, but funding tx is not final".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn fill_in_channel_id(&mut self) {
        let local = &self.get_local_channel_public_keys().tlc_base_key;
        let remote = &self.get_remote_channel_public_keys().tlc_base_key;
        let channel_id = derive_channel_id_from_tlc_keys(local, remote);
        debug!("Channel Id changed from {:?} to {:?}", self.id, channel_id,);
        self.id = channel_id;
    }

    // Whose pubkey should go first in musig2?
    // We define a definitive order for the pubkeys in musig2 to makes it easier
    // to aggregate musig2 signatures.
    fn should_local_go_first_in_musig2(&self) -> bool {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        local_pubkey <= remote_pubkey
    }

    // Order some items (like pubkey and nonce) from local and remote in musig2.
    fn order_things_for_musig2<T>(&self, local: T, remote: T) -> [T; 2] {
        if self.should_local_go_first_in_musig2() {
            [local, remote]
        } else {
            [remote, local]
        }
    }

    fn get_funding_common_context(&self) -> Musig2CommonContext {
        let local_first = self.should_local_go_first_in_musig2();
        let key_agg_ctx = self.get_deterministic_musig2_agg_context();
        let remote_nonce = self.get_last_committed_remote_nonce().funding;
        let local_nonce = self.get_local_nonce_funding();
        let agg_nonce = AggNonce::sum(if local_first {
            [local_nonce, remote_nonce]
        } else {
            [remote_nonce, local_nonce]
        });
        Musig2CommonContext {
            local_first,
            key_agg_ctx,
            agg_nonce,
        }
    }

    fn get_commitment_common_context(&self, for_remote: bool) -> Musig2CommonContext {
        let key_agg_ctx = self.get_musig2_agg_context(for_remote);
        let remote_nonce = self.get_last_committed_remote_nonce().commitment;
        let local_nonce = self.get_commitment_nonce().commitment;
        let agg_nonce = AggNonce::sum(if for_remote {
            [local_nonce, remote_nonce]
        } else {
            [remote_nonce, local_nonce]
        });
        Musig2CommonContext {
            local_first: for_remote,
            key_agg_ctx,
            agg_nonce,
        }
    }

    fn get_revoke_common_context(&self, for_remote: bool) -> Option<Musig2CommonContext> {
        let key_agg_ctx = self.get_musig2_agg_context(for_remote);
        let remote_nonce = if for_remote {
            self.remote_revocation_nonce_for_verify.as_ref()
        } else {
            self.remote_revocation_nonce_for_send.as_ref()
        }
        .map(|r| r.revoke.clone())?;
        let local_nonce = self.get_revocation_nonce(for_remote).revoke;
        let agg_nonce = AggNonce::sum(if for_remote {
            [local_nonce, remote_nonce]
        } else {
            [remote_nonce, local_nonce]
        });
        Some(Musig2CommonContext {
            local_first: for_remote,
            key_agg_ctx,
            agg_nonce,
        })
    }

    fn get_ack_common_context(&self, for_remote: bool) -> Option<Musig2CommonContext> {
        let key_agg_ctx = self.get_musig2_agg_context(for_remote);
        let remote_nonce = if for_remote {
            self.remote_revocation_nonce_for_verify.as_ref()
        } else {
            self.remote_revocation_nonce_for_send.as_ref()
        }
        .map(|r| r.ack.clone())?;
        let local_nonce = self.get_revocation_nonce(for_remote).ack;
        let agg_nonce = AggNonce::sum(if for_remote {
            [local_nonce, remote_nonce]
        } else {
            [remote_nonce, local_nonce]
        });
        Some(Musig2CommonContext {
            local_first: for_remote,
            key_agg_ctx,
            agg_nonce,
        })
    }

    fn get_musig2_agg_context(&self, for_remote: bool) -> KeyAggContext {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        let pubkeys = if for_remote {
            [local_pubkey, remote_pubkey]
        } else {
            [remote_pubkey, local_pubkey]
        };
        KeyAggContext::new(pubkeys).expect("Valid pubkeys")
    }

    // This is used to verify transactions which consume the funding cell.
    fn get_funding_verify_context(&self) -> Musig2VerifyContext {
        let common_ctx = self.get_funding_common_context();
        Musig2VerifyContext {
            common_ctx,
            pubkey: *self.get_remote_funding_pubkey(),
            pubnonce: self.get_last_committed_remote_nonce().funding,
        }
    }

    // This is used to sign transactions which consume the funding cell.
    fn get_funding_sign_context(&self) -> Musig2SignContext {
        let common_ctx = self.get_funding_common_context();
        let seckey = self.signer.funding_key.clone();
        let [secnonce] = self
            .signer
            .derive_musig2_nonce(self.get_local_commitment_number(), [Musig2Context::Funding]);

        Musig2SignContext {
            common_ctx,
            seckey,
            secnonce,
        }
    }

    // This is used to verify settlement transaction which consume the commitment cell.
    fn get_commitment_verify_context(&self) -> Musig2VerifyContext {
        let common_ctx = self.get_commitment_common_context(false);

        Musig2VerifyContext {
            common_ctx,
            pubkey: *self.get_remote_funding_pubkey(),
            pubnonce: self.get_last_committed_remote_nonce().commitment,
        }
    }

    // This is used to sign settlement transaction which consume the commitment cell.
    fn get_commitment_sign_context(&self, for_remote: bool) -> Musig2SignContext {
        let common_ctx = self.get_commitment_common_context(for_remote);
        let seckey = self.signer.funding_key.clone();
        let [secnonce] = self.signer.derive_musig2_nonce(
            self.get_local_commitment_number(),
            [Musig2Context::Commitment],
        );

        Musig2SignContext {
            common_ctx,
            seckey,
            secnonce,
        }
    }

    // This is used to sign revocation transactions which consume the commitment cell.
    fn get_revoke_sign_context(&self, for_remote: bool) -> Option<Musig2SignContext> {
        let common_ctx = self.get_revoke_common_context(for_remote)?;
        let seckey = self.signer.funding_key.clone();
        let commitment_number = if for_remote {
            self.get_local_commitment_number()
        } else {
            self.get_remote_commitment_number()
        };
        let [secnonce] = self
            .signer
            .derive_musig2_nonce(commitment_number, [Musig2Context::Revoke]);

        Some(Musig2SignContext {
            common_ctx,
            seckey,
            secnonce,
        })
    }

    // This is used to sign commitment transaction (in RevokeAndAck message) which consume the funding cell.
    fn get_ack_sign_context(&self, for_remote: bool) -> Option<Musig2SignContext> {
        let common_ctx = self.get_ack_common_context(for_remote)?;
        let seckey = self.signer.funding_key.clone();
        let commitment_number = if for_remote {
            self.get_local_commitment_number()
        } else {
            self.get_remote_commitment_number()
        };
        let [secnonce] = self
            .signer
            .derive_musig2_nonce(commitment_number, [Musig2Context::Ack]);

        Some(Musig2SignContext {
            common_ctx,
            seckey,
            secnonce,
        })
    }

    // Should the local send tx_signatures first?
    // In order to avoid deadlock, we need to define an order for sending tx_signatures.
    // Currently the order of sending tx_signatures is defined as follows:
    // If the amount to self is less than the amount to remote, then we should send,
    // else if the amount to self is equal to the amount to remote and we have
    // smaller funding_pubkey, then we should send first. Otherwise, we should wait
    // the counterparty to send tx_signatures first.
    fn should_local_send_tx_signatures_first(&self) -> bool {
        self.to_local_amount < self.to_remote_amount
            || self.to_local_amount == self.to_remote_amount
                && self.should_local_go_first_in_musig2()
    }

    async fn build_shutdown_tx(&self) -> Result<TransactionView, ProcessingChannelError> {
        let (Some(local_shutdown_info), Some(remote_shutdown_info), Some(channel_outpoint)) = (
            self.local_shutdown_info.as_ref(),
            self.remote_shutdown_info.as_ref(),
            self.get_funding_transaction_outpoint(),
        ) else {
            return Err(ProcessingChannelError::InvalidState(
                "Shutdown transaction build failed without local or remote shutdown info or funding transaction outpoint".to_string(),
            ));
        };

        let local_shutdown_script = local_shutdown_info.close_script.clone();
        let remote_shutdown_script = remote_shutdown_info.close_script.clone();
        let local_shutdown_fee = calculate_shutdown_tx_fee(
            local_shutdown_info.fee_rate,
            &self.funding_udt_type_script,
            (
                remote_shutdown_script.clone(),
                local_shutdown_script.clone(),
            ),
        );
        let remote_shutdown_fee = calculate_shutdown_tx_fee(
            remote_shutdown_info.fee_rate,
            &self.funding_udt_type_script,
            (
                local_shutdown_script.clone(),
                remote_shutdown_script.clone(),
            ),
        );

        debug!(
            "build_shutdown_tx local_shutdown_fee: local {}, remote {}",
            local_shutdown_fee, remote_shutdown_fee
        );

        let cell_deps = get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script)
            .await
            .map_err(|e| ProcessingChannelError::InternalError(e.to_string()))?;
        let tx_builder = TransactionBuilder::default().cell_deps(cell_deps).input(
            CellInput::new_builder()
                .previous_output(channel_outpoint)
                .build(),
        );

        if let Some(type_script) = &self.funding_udt_type_script {
            debug!(
                "shutdown UDT local_amount: {}, remote_amount: {}",
                self.to_local_amount, self.to_remote_amount
            );

            let local_capacity: u64 = self.local_reserved_ckb_amount - local_shutdown_fee;
            debug!(
                "shutdown_tx local_capacity: {} - {} = {}",
                self.local_reserved_ckb_amount, local_shutdown_fee, local_capacity
            );
            let to_local_output = CellOutput::new_builder()
                .lock(local_shutdown_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(local_capacity.pack())
                .build();
            let to_local_output_data = self.to_local_amount.to_le_bytes().pack();

            let remote_capacity: u64 = self.remote_reserved_ckb_amount - remote_shutdown_fee;
            debug!(
                "shutdown_tx remote_capacity: {} - {} = {}",
                self.remote_reserved_ckb_amount, remote_shutdown_fee, remote_capacity
            );
            let to_remote_output = CellOutput::new_builder()
                .lock(remote_shutdown_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(remote_capacity.pack())
                .build();
            let to_remote_output_data = self.to_remote_amount.to_le_bytes().pack();

            let outputs = self.order_things_for_musig2(to_local_output, to_remote_output);
            let outputs_data =
                self.order_things_for_musig2(to_local_output_data, to_remote_output_data);
            let tx = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(outputs_data.to_vec())
                .build();
            Ok(tx)
        } else {
            debug!(
                "Final balance partition before shutting down: local {} (fee {}), remote {} (fee {})",
                self.to_local_amount, local_shutdown_fee,
                self.to_remote_amount, remote_shutdown_fee
            );
            let local_value =
                self.to_local_amount as u64 + self.local_reserved_ckb_amount - local_shutdown_fee;
            let remote_value = self.to_remote_amount as u64 + self.remote_reserved_ckb_amount
                - remote_shutdown_fee;
            debug!(
                "Building shutdown transaction with values: local {}, remote {}",
                local_value, remote_value
            );
            let to_local_output = CellOutput::new_builder()
                .capacity(local_value.pack())
                .lock(local_shutdown_script)
                .build();
            let to_remote_output = CellOutput::new_builder()
                .capacity(remote_value.pack())
                .lock(remote_shutdown_script)
                .build();
            let outputs = self.order_things_for_musig2(to_local_output, to_remote_output);
            let tx = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(vec![Default::default(), Default::default()])
                .build();
            Ok(tx)
        }
    }

    // The parameter `for_remote` here specifies whether we are building the commitment transaction
    // for the local party or the remote party. If `for_remote` is false, then we are building a
    // commitment transaction which can be broadcasted by ourself (with valid partial
    // signature from the other party), else we are building a commitment transaction
    // for the remote party (we build this commitment transaction
    // normally because we want to send a partial signature to remote).
    // The function returns a tuple, the first element is the commitment transaction itself,
    // and the second element is the settlement transaction.
    fn build_commitment_and_settlement_tx(
        &self,
        for_remote: bool,
    ) -> Result<(TransactionView, TransactionView), ProcessingChannelError> {
        let commitment_tx = {
            let funding_out_point = self.must_get_funding_transaction_outpoint();
            let (output, output_data) = self.build_commitment_transaction_output(for_remote);

            TransactionBuilder::default()
                .input(
                    CellInput::new_builder()
                        .previous_output(funding_out_point.clone())
                        .build(),
                )
                .output(output)
                .output_data(output_data)
                .build()
        };

        let settlement_tx = {
            let commtimtent_out_point = OutPoint::new(commitment_tx.hash(), 0);
            let (outputs, outputs_data) = self.build_settlement_transaction_outputs(for_remote);

            TransactionBuilder::default()
                .input(
                    CellInput::new_builder()
                        .previous_output(commtimtent_out_point.clone())
                        .build(),
                )
                .set_outputs(outputs.to_vec())
                .set_outputs_data(outputs_data.to_vec())
                .build()
        };

        Ok((commitment_tx, settlement_tx))
    }

    fn build_commitment_transaction_output(&self, for_remote: bool) -> (CellOutput, Bytes) {
        let x_only_aggregated_pubkey = self.get_commitment_lock_script_xonly(for_remote);
        let version = self.get_current_commitment_number(for_remote);
        let tlcs = self.get_active_tlcs_for_commitment(for_remote);

        let mut commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();
        if !tlcs.is_empty() {
            commitment_lock_script_args.extend_from_slice(&blake2b_256(&tlcs)[0..20]);
        }

        let commitment_lock_script =
            get_script_by_contract(Contract::CommitmentLock, &commitment_lock_script_args);

        let commitment_tx_fee =
            calculate_commitment_tx_fee(self.commitment_fee_rate, &self.funding_udt_type_script);

        if let Some(udt_type_script) = &self.funding_udt_type_script {
            let capacity = self.local_reserved_ckb_amount + self.remote_reserved_ckb_amount
                - commitment_tx_fee;
            let output = CellOutput::new_builder()
                .lock(commitment_lock_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity(capacity.pack())
                .build();

            let output_data = self.get_liquid_capacity().to_le_bytes().pack();
            (output, output_data)
        } else {
            let capacity = self.get_total_ckb_amount() - commitment_tx_fee;
            let output = CellOutput::new_builder()
                .lock(commitment_lock_script)
                .capacity(capacity.pack())
                .build();
            let output_data = Bytes::default();
            (output, output_data)
        }
    }

    pub(crate) fn get_commitment_lock_script_xonly(&self, for_remote: bool) -> [u8; 32] {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        let pubkeys = if for_remote {
            [local_pubkey, remote_pubkey]
        } else {
            [remote_pubkey, local_pubkey]
        };
        let key_agg_ctx = KeyAggContext::new(pubkeys).expect("Valid pubkeys");
        key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly()
    }

    fn build_settlement_transaction_outputs(
        &self,
        for_remote: bool,
    ) -> ([CellOutput; 2], [Bytes; 2]) {
        let pending_tlcs = self
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .filter(move |tlc| match tlc.outbound_status() {
                OutboundTlcStatus::LocalAnnounced => for_remote,
                OutboundTlcStatus::Committed => true,
                OutboundTlcStatus::RemoteRemoved => true,
                OutboundTlcStatus::RemoveWaitPrevAck => true,
                OutboundTlcStatus::RemoveWaitAck => true,
                OutboundTlcStatus::RemoveAckConfirmed => true,
            })
            .chain(self.tlc_state.received_tlcs.tlcs.iter().filter(move |tlc| {
                match tlc.inbound_status() {
                    InboundTlcStatus::RemoteAnnounced => !for_remote,
                    InboundTlcStatus::AnnounceWaitPrevAck => !for_remote,
                    InboundTlcStatus::AnnounceWaitAck => true,
                    InboundTlcStatus::Committed => true,
                    InboundTlcStatus::LocalRemoved => true,
                    InboundTlcStatus::RemoveAckConfirmed => true,
                }
            }));

        let mut offered_pending = 0;
        let mut offered_fulfilled = 0;
        let mut received_pending = 0;
        let mut received_fulfilled = 0;

        for info in pending_tlcs {
            if info.is_offered() {
                let confirmed_remove_reason = (info.outbound_status()
                    == OutboundTlcStatus::RemoveWaitAck
                    || info.outbound_status() == OutboundTlcStatus::RemoveAckConfirmed
                    || (info.outbound_status() == OutboundTlcStatus::RemoteRemoved && !for_remote))
                    .then(|| info.removed_reason.as_ref().unwrap());
                match confirmed_remove_reason {
                    Some(RemoveTlcReason::RemoveTlcFulfill(_)) => {
                        offered_fulfilled += info.amount;
                    }
                    Some(RemoveTlcReason::RemoveTlcFail(_)) => {
                        // This TLC failed, so it is not counted in the pending amount and the fulfilled amount
                    }
                    None => {
                        offered_pending += info.amount;
                    }
                }
            }
            if info.is_received() {
                let confirmed_remove_reason = (info.inbound_status()
                    == InboundTlcStatus::RemoveAckConfirmed
                    || (info.inbound_status() == InboundTlcStatus::LocalRemoved && for_remote))
                    .then(|| info.removed_reason.as_ref().unwrap());
                match confirmed_remove_reason {
                    Some(RemoveTlcReason::RemoveTlcFulfill(_)) => {
                        received_fulfilled += info.amount;
                    }
                    Some(RemoveTlcReason::RemoveTlcFail(_)) => {
                        // This TLC failed, so it is not counted in the pending amount and the fulfilled amount
                    }
                    None => {
                        received_pending += info.amount;
                    }
                }
            }
        }
        let to_local_value =
            self.to_local_amount + received_fulfilled - offered_pending - offered_fulfilled;
        let to_remote_value =
            self.to_remote_amount + offered_fulfilled - received_pending - received_fulfilled;

        #[cfg(debug_assertions)]
        {
            self.tlc_state.debug();
            debug!(
                "build_settlement_transaction_outputs to_local_value: {}, to_remote_value: {} for_remote: {:?}",
                to_local_value,
                to_remote_value,
                for_remote,
            );
        }

        let commitment_tx_fee =
            calculate_commitment_tx_fee(self.commitment_fee_rate, &self.funding_udt_type_script);

        let to_local_output_script = self.get_local_shutdown_script();
        let to_remote_output_script = self.get_remote_shutdown_script();

        // to simplify the fee calculation, we assume that the fee is double paid by both parties
        if let Some(udt_type_script) = &self.funding_udt_type_script {
            let to_local_output = CellOutput::new_builder()
                .lock(to_local_output_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity((self.local_reserved_ckb_amount - commitment_tx_fee).pack())
                .build();
            let to_local_output_data = to_local_value.to_le_bytes().pack();

            let to_remote_output = CellOutput::new_builder()
                .lock(to_remote_output_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity((self.remote_reserved_ckb_amount - commitment_tx_fee).pack())
                .build();
            let to_remote_output_data = to_remote_value.to_le_bytes().pack();
            if for_remote {
                (
                    [to_local_output, to_remote_output],
                    [to_local_output_data, to_remote_output_data],
                )
            } else {
                (
                    [to_remote_output, to_local_output],
                    [to_remote_output_data, to_local_output_data],
                )
            }
        } else {
            let to_local_output = CellOutput::new_builder()
                .lock(to_local_output_script)
                .capacity(
                    (to_local_value as u64 + self.local_reserved_ckb_amount - commitment_tx_fee)
                        .pack(),
                )
                .build();
            let to_local_output_data = Bytes::default();

            let to_remote_output = CellOutput::new_builder()
                .lock(to_remote_output_script)
                .capacity(
                    (to_remote_value as u64 + self.remote_reserved_ckb_amount - commitment_tx_fee)
                        .pack(),
                )
                .build();
            let to_remote_output_data = Bytes::default();

            if for_remote {
                (
                    [to_local_output, to_remote_output],
                    [to_local_output_data, to_remote_output_data],
                )
            } else {
                (
                    [to_remote_output, to_local_output],
                    [to_remote_output_data, to_local_output_data],
                )
            }
        }
    }

    pub fn build_and_verify_commitment_tx(
        &self,
        funding_tx_partial_signature: PartialSignature,
        commitment_tx_partial_signature: PartialSignature,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let (commitment_tx, settlement_tx) = self.build_commitment_and_settlement_tx(false)?;

        let funding_verify_ctx = self.get_funding_verify_context();
        funding_verify_ctx.verify(
            funding_tx_partial_signature,
            &compute_tx_message(&commitment_tx),
        )?;

        let to_local_output = settlement_tx
            .outputs()
            .get(0)
            .expect("get output 0 of settlement tx");
        let to_local_output_data = settlement_tx
            .outputs_data()
            .get(0)
            .expect("get output 0 data of settlement tx");
        let to_remote_output = settlement_tx
            .outputs()
            .get(1)
            .expect("get output 1 of settlement tx");
        let to_remote_output_data = settlement_tx
            .outputs_data()
            .get(1)
            .expect("get output 1 data of settlement tx");
        let args = commitment_tx
            .outputs()
            .get(0)
            .expect("get output 0 of commitment tx")
            .lock()
            .args()
            .raw_data();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                &args[0..36],
            ]
            .concat(),
        );

        let commitment_verify_ctx = self.get_commitment_verify_context();
        commitment_verify_ctx.verify(commitment_tx_partial_signature, message.as_slice())?;

        Ok(PartiallySignedCommitmentTransaction {
            version: self.get_remote_commitment_number(),
            commitment_tx,
            settlement_tx,
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
        })
    }

    fn build_and_sign_commitment_tx(
        &self,
    ) -> Result<(PartialSignature, PartialSignature), ProcessingChannelError> {
        let (commitment_tx, settlement_tx) = self.build_commitment_and_settlement_tx(true)?;
        let funding_tx_partial_signature = self
            .get_funding_sign_context()
            .sign(&compute_tx_message(&commitment_tx))?;

        let to_local_output = settlement_tx
            .outputs()
            .get(0)
            .expect("get output 0 of settlement tx");
        let to_local_output_data = settlement_tx
            .outputs_data()
            .get(0)
            .expect("get output 0 data of settlement tx");
        let to_remote_output = settlement_tx
            .outputs()
            .get(1)
            .expect("get output 1 of settlement tx");
        let to_remote_output_data = settlement_tx
            .outputs_data()
            .get(1)
            .expect("get output 1 data of settlement tx");
        let args = commitment_tx
            .outputs()
            .get(0)
            .expect("get output 0 of commitment tx")
            .lock()
            .args()
            .raw_data();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                &args[0..36],
            ]
            .concat(),
        );

        let commitment_tx_partial_signature = self
            .get_commitment_sign_context(true)
            .sign(message.as_slice())?;

        Ok((
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
        ))
    }

    /// Get the latest commitment transaction with updated cell deps
    pub async fn get_latest_commitment_transaction(
        &self,
    ) -> Result<TransactionView, ProcessingChannelError> {
        let tx = self
            .latest_commitment_transaction
            .clone()
            .expect("latest_commitment_transaction should exist");
        let cell_deps = get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script)
            .await
            .map_err(|e| ProcessingChannelError::InternalError(e.to_string()))?;
        let raw_tx = tx.raw().as_builder().cell_deps(cell_deps).build();
        let tx = tx.as_builder().raw(raw_tx).build();
        Ok(tx.into_view())
    }

    /// Verify the partial signature from the peer and create a complete transaction
    /// with valid witnesses.
    fn verify_and_complete_tx(
        &self,
        funding_tx_partial_signature: PartialSignature,
        commitment_tx_partial_signature: PartialSignature,
    ) -> Result<(TransactionView, SettlementData), ProcessingChannelError> {
        let tx = self.build_and_verify_commitment_tx(
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
        )?;
        self.complete_partially_signed_tx(&tx)
    }

    fn get_delay_epoch_as_lock_args_bytes(&self) -> [u8; 8] {
        let since = Since::new(
            SinceType::EpochNumberWithFraction,
            self.commitment_delay_epoch,
            true,
        );
        since.value().to_le_bytes()
    }

    fn notify_funding_tx(&self, network: &ActorRef<NetworkActorMessage>) {
        let tx = if let Some(ref tx) = self.funding_tx {
            tx
        } else {
            return;
        };

        let should_notify = match self.state {
            ChannelState::CollaboratingFundingTx(_)
            | ChannelState::SigningCommitment(_)
            | ChannelState::AwaitingTxSignatures(_) => true,
            ChannelState::AwaitingChannelReady(flags)
                if !flags.contains(AwaitingChannelReadyFlags::OUR_CHANNEL_READY) =>
            {
                true
            }
            _ => false,
        };
        if should_notify {
            let _ = network.send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::NotifyFundingTx(tx.clone()),
            ));
        }
    }

    pub(crate) async fn update_close_transaction_confirmed(
        &mut self,
        tx_hash: H256,
        force: bool,
        close_by_us: bool,
    ) -> ProcessingChannelResult {
        match self.state {
            ChannelState::ShuttingDown(flags)
                if flags.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION) => {}
            ChannelState::ChannelReady if force && !close_by_us => {}
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                            "Expecting commitment transaction confirmed event in unexpected state {:?} force {force} close_by_us {close_by_us}", &self.state)
                        ));
            }
        };

        let closed_state = if force {
            debug!("Channel closed with uncooperative close");
            if close_by_us {
                ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL)
            } else {
                ChannelState::Closed(CloseFlags::UNCOOPERATIVE_REMOTE)
            }
        } else {
            debug!("Channel closed with cooperative close");
            ChannelState::Closed(CloseFlags::COOPERATIVE)
        };
        self.update_state(closed_state);
        self.shutdown_transaction_hash.replace(tx_hash);
        // Broadcast the channel update message which disables the channel.
        if self.is_public() {
            let update = self.generate_disabled_channel_update();

            self.network()
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::BroadcastMessages(vec![
                        BroadcastMessageWithTimestamp::ChannelUpdate(update),
                    ]),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }

        Ok(())
    }

    fn can_abort_funding_on_timeout(&self) -> bool {
        // Can abort funding on timeout if the channel is not ready and we have
        // not signed the funding tx yet.
        match self.state {
            ChannelState::NegotiatingFunding(_)
            | ChannelState::CollaboratingFundingTx(_)
            | ChannelState::SigningCommitment(_) => true,
            // Once we have sent the signature, the peer may succeed to submit
            // the funding tx on-chain.The best solution is waiting for the
            // confirmations or spending any input of the funding tx.
            ChannelState::AwaitingTxSignatures(flags)
                if !flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT) =>
            {
                true
            }
            _ => false,
        }
    }

    pub fn has_pending_operations(&self) -> bool {
        !self.retryable_tlc_operations.is_empty()
    }
}

pub trait ChannelActorStateStore {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState>;
    fn insert_channel_actor_state(&self, state: ChannelActorState);
    fn delete_channel_actor_state(&self, id: &Hash256);
    fn get_channel_ids_by_peer(&self, peer_id: &PeerId) -> Vec<Hash256>;
    fn get_active_channel_ids_by_peer(&self, peer_id: &PeerId) -> Vec<Hash256> {
        self.get_channel_ids_by_peer(peer_id)
            .into_iter()
            .filter(
                |id| matches!(self.get_channel_actor_state(id), Some(state) if !state.is_closed()),
            )
            .collect()
    }
    fn get_channel_states(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Hash256, ChannelState)>;
    fn get_active_channel_states(
        &self,
        peer_id: Option<PeerId>,
    ) -> Vec<(PeerId, Hash256, ChannelState)> {
        self.get_channel_states(peer_id)
            .into_iter()
            .filter(|(_, _, state)| !state.is_closed())
            .collect()
    }
    fn get_channel_state_by_outpoint(&self, id: &OutPoint) -> Option<ChannelActorState>;
    fn insert_payment_custom_records(
        &self,
        payment_hash: &Hash256,
        custom_records: PaymentCustomRecords,
    );
    fn get_payment_custom_records(&self, payment_hash: &Hash256) -> Option<PaymentCustomRecords>;
    fn insert_payment_hold_tlc(&self, payment_hash: Hash256, hold_tlc: HoldTlc);
    fn remove_payment_hold_tlc(&self, payment_hash: &Hash256, channel_id: &Hash256, tlc_id: u64);
    fn get_payment_hold_tlcs(&self, payment_hash: Hash256) -> Vec<HoldTlc>;
    fn get_node_hold_tlcs(&self) -> HashMap<Hash256, Vec<HoldTlc>>;
}

/// A wrapper on CommitmentTransaction that has a partial signature along with
/// the ckb transaction.
#[derive(Clone, Debug)]
pub struct PartiallySignedCommitmentTransaction {
    // The version number of the commitment transaction.
    pub version: u64,
    // The commitment transaction.
    pub commitment_tx: TransactionView,
    // The settlement transaction.
    pub settlement_tx: TransactionView,
    // The partial signature to unlock the funding transaction.
    pub funding_tx_partial_signature: PartialSignature,
    // The partial signature to unlock the commitment transaction.
    pub commitment_tx_partial_signature: PartialSignature,
}

/// for xudt compatibility issue,
/// refer to: https://github.com/nervosnetwork/fiber-scripts/pull/5
pub const XUDT_COMPATIBLE_WITNESS: [u8; 16] = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];

pub fn create_witness_for_funding_cell(
    lock_key_xonly: [u8; 32],
    signature: CompactSignature,
) -> [u8; FUNDING_CELL_WITNESS_LEN] {
    let mut witness = Vec::with_capacity(FUNDING_CELL_WITNESS_LEN);
    witness.extend_from_slice(&XUDT_COMPATIBLE_WITNESS);
    witness.extend_from_slice(lock_key_xonly.as_slice());
    witness.extend_from_slice(signature.serialize().as_slice());
    witness
        .try_into()
        .expect("Witness length should be correct")
}

pub fn create_witness_for_commitment_cell(
    lock_key_xonly: [u8; 32],
    signature: CompactSignature,
) -> [u8; COMMITMENT_CELL_WITNESS_LEN] {
    let mut witness = Vec::with_capacity(COMMITMENT_CELL_WITNESS_LEN);
    witness.extend_from_slice(&XUDT_COMPATIBLE_WITNESS);
    witness.extend_from_slice(&[0xFE]);
    witness.extend_from_slice(lock_key_xonly.as_slice());
    witness.extend_from_slice(signature.serialize().as_slice());
    witness
        .try_into()
        .expect("Witness length should be correct")
}

pub fn create_witness_for_commitment_cell_with_pending_tlcs(
    index: u8,
    pending_tlcs: &[u8],
) -> Vec<u8> {
    let mut witness = Vec::new();
    witness.extend_from_slice(&XUDT_COMPATIBLE_WITNESS);
    witness.extend_from_slice(&[index]);
    witness.extend_from_slice(&[(pending_tlcs.len() / 85) as u8]);
    witness.extend_from_slice(pending_tlcs);
    witness.extend_from_slice(&[0u8; 65]);
    witness
}

// The common musig2 configuration that is used both by signing and verifying.
#[derive(Debug)]
struct Musig2CommonContext {
    // This parameter is also saved to the context because it is useful for
    // aggregating partial signatures.
    local_first: bool,
    key_agg_ctx: KeyAggContext,
    agg_nonce: AggNonce,
}

impl Musig2CommonContext {
    fn aggregate_partial_signatures_for_msg(
        &self,
        local_signature: PartialSignature,
        remote_signature: PartialSignature,
        message: &[u8],
    ) -> Result<CompactSignature, VerifyError> {
        let partial_signatures = if self.local_first {
            [local_signature, remote_signature]
        } else {
            [remote_signature, local_signature]
        };
        aggregate_partial_signatures(
            &self.key_agg_ctx,
            &self.agg_nonce,
            partial_signatures,
            message,
        )
    }

    pub fn x_only_aggregated_pubkey(&self) -> [u8; 32] {
        self.key_agg_ctx
            .aggregated_pubkey::<Point>()
            .serialize_xonly()
    }
}

struct Musig2VerifyContext {
    common_ctx: Musig2CommonContext,
    pubkey: Pubkey,
    pubnonce: PubNonce,
}

impl Musig2VerifyContext {
    fn verify(&self, signature: PartialSignature, message: &[u8]) -> Result<(), VerifyError> {
        verify_partial(
            &self.common_ctx.key_agg_ctx,
            signature,
            &self.common_ctx.agg_nonce,
            self.pubkey,
            &self.pubnonce,
            message,
        )
    }
}

struct Musig2SignContext {
    common_ctx: Musig2CommonContext,
    seckey: Privkey,
    secnonce: SecNonce,
}

#[cfg(test)]
static SECNONCES: LazyLock<Mutex<HashMap<[u8; 64], Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

impl Musig2SignContext {
    fn sign(&self, message: &[u8]) -> Result<PartialSignature, SigningError> {
        #[cfg(test)]
        {
            // Check if the secnonce is reused for different messages.
            let mut secnonces = SECNONCES.lock().unwrap();
            if let Some(old) = secnonces.insert(self.secnonce.to_bytes(), message.to_vec()) {
                if old.as_slice() != message {
                    panic!(
                        "Musig2 secnonce {:?} is reused for different messages: {:?} and {:?} backtrace: {}",
                        self.secnonce.public_nonce(), old, message, Backtrace::capture()
                    );
                }
            }
        }

        sign_partial(
            &self.common_ctx.key_agg_ctx,
            self.seckey.clone(),
            self.secnonce.clone(),
            &self.common_ctx.agg_nonce,
            message,
        )
    }

    fn sign_and_aggregate(
        &self,
        message: &[u8],
        remote_signature: PartialSignature,
    ) -> Result<CompactSignature, RoundFinalizeError> {
        let local_signature = sign_partial(
            &self.common_ctx.key_agg_ctx,
            self.seckey.clone(),
            self.secnonce.clone(),
            &self.common_ctx.agg_nonce,
            message,
        )?;
        Ok(self.common_ctx.aggregate_partial_signatures_for_msg(
            local_signature,
            remote_signature,
            message,
        )?)
    }
}

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBasePublicKeys {
    /// The public key which is used to sign all commitment transactions, as it appears in the
    /// on-chain channel lock-in 2-of-2 multisig output.
    pub funding_pubkey: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment public key
    /// which is used to encumber HTLC-in-flight outputs.
    pub tlc_base_key: Pubkey,
}

impl From<&OpenChannel> for ChannelBasePublicKeys {
    fn from(value: &OpenChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey,
            tlc_base_key: value.tlc_basepoint,
        }
    }
}

impl From<&AcceptChannel> for ChannelBasePublicKeys {
    fn from(value: &AcceptChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey,
            tlc_base_key: value.tlc_basepoint,
        }
    }
}

type ShortHash = [u8; 20];

pub(crate) fn get_tweak_by_commitment_point(commitment_point: &Pubkey) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(&commitment_point.serialize());
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}

pub(crate) fn derive_private_key(secret: &Privkey, commitment_point: &Pubkey) -> Privkey {
    secret.tweak(get_tweak_by_commitment_point(commitment_point))
}

fn derive_public_key(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    base_key.tweak(get_tweak_by_commitment_point(commitment_point))
}

pub(crate) fn derive_tlc_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

pub enum Musig2Context {
    Funding,
    Commitment,
    Revoke,
    Ack,
}

impl std::fmt::Display for Musig2Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let context_str = match self {
            Musig2Context::Funding => "FUNDING",
            Musig2Context::Commitment => "COMMITMENT",
            Musig2Context::Revoke => "REVOKE",
            Musig2Context::Ack => "ACK",
        };
        write!(f, "{}", context_str)
    }
}

/// A simple implementation of [`WriteableEcdsaChannelSigner`] that just keeps the private keys in memory.
///
/// This implementation performs no policy checks and is insufficient by itself as
/// a secure external signer.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InMemorySigner {
    /// Holder secret key in the 2-of-2 multisig script of a channel. This key also backs the
    /// holder's anchor output in a commitment transaction, if one is present.
    pub funding_key: Privkey,
    /// Holder HTLC secret key used in commitment transaction HTLC outputs.
    pub tlc_base_key: Privkey,
    /// SecNonce used to generate valid signature in musig.
    // TODO: use rust's ownership to make sure musig_nonce is used once.
    pub musig2_base_nonce: Privkey,
    /// Seed to derive above keys (per commitment).
    pub commitment_seed: [u8; 32],
}

impl InMemorySigner {
    pub fn generate_from_seed(params: &[u8]) -> Self {
        let seed = ckb_hash::blake2b_256(params);

        let commitment_seed = {
            let mut hasher = new_blake2b();
            hasher.update(&seed);
            hasher.update(&b"commitment seed"[..]);
            let mut result = [0u8; 32];
            hasher.finalize(&mut result);
            result
        };

        let key_derive = |seed: &[u8], info: &[u8]| {
            let result = blake2b_hash_with_salt(seed, info);
            Privkey::from_slice(&result)
        };

        let funding_key = key_derive(&seed, b"funding key");
        let tlc_base_key = key_derive(funding_key.as_ref(), b"HTLC base key");
        let musig2_base_nonce = key_derive(tlc_base_key.as_ref(), b"musig nocne");

        Self {
            funding_key,
            tlc_base_key,
            musig2_base_nonce,
            commitment_seed,
        }
    }

    fn get_base_public_keys(&self) -> ChannelBasePublicKeys {
        ChannelBasePublicKeys {
            funding_pubkey: self.funding_key.pubkey(),
            tlc_base_key: self.tlc_base_key.pubkey(),
        }
    }

    pub fn get_commitment_point(&self, commitment_number: u64) -> Pubkey {
        get_commitment_point(&self.commitment_seed, commitment_number)
    }

    pub fn get_commitment_secret(&self, commitment_number: u64) -> [u8; 32] {
        get_commitment_secret(&self.commitment_seed, commitment_number)
    }

    pub fn derive_tlc_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.tlc_base_key, &per_commitment_point)
    }

    pub fn derive_musig2_nonce<const N: usize>(
        &self,
        commitment_number: u64,
        contexts: [Musig2Context; N],
    ) -> [SecNonce; N] {
        let commitment_point = self.get_commitment_point(commitment_number);
        let seckey = derive_private_key(&self.musig2_base_nonce, &commitment_point);

        contexts.map(|context| {
            SecNonceBuilder::new(seckey.as_ref())
                .with_extra_input(&context.to_string())
                .build()
        })
    }
}
