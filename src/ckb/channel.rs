use bitflags::bitflags;
use ckb_hash::{blake2b_256, new_blake2b};
use ckb_types::{
    core::TransactionView,
    packed::{OutPoint, Transaction},
    prelude::IntoTransactionView,
};
use log::{debug, error, info, warn};
use molecule::prelude::Entity;
use musig2::{
    errors::{SigningError, VerifyError},
    sign_partial, verify_partial, AggNonce, KeyAggContext, PartialSignature, PubNonce, SecNonce,
};
use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use serde_with::serde_as;
use tentacle::secio::PeerId;
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;

use std::fmt::Debug;

use super::{
    network::{OpenChannelCommand, PCNMessageWithPeerId},
    serde_utils::EntityWrapperHex,
    types::{
        AcceptChannel, ChannelReady, CommitmentSigned, Hash256, OpenChannel, PCNMessage, Privkey,
        Pubkey, TxAdd, TxCollaborationMsg, TxComplete, TxRemove,
    },
    NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
};

pub enum ChannelActorMessage {
    /// Command are the messages that are sent to the channel actor to perform some action.
    /// It is normally generated from a user request.
    Command(ChannelCommand),
    /// PeerMessage are the messages sent from the peer.
    PeerMessage(PCNMessage),
}

#[derive(Clone, Debug, Deserialize)]
pub enum ChannelCommand {
    TxCollaborationCommand(TxCollaborationCommand),
    CommitmentSigned(),
}

#[derive(Clone, Debug, Deserialize)]
pub enum TxCollaborationCommand {
    TxAdd(TxAddCommand),
    TxRemove(TxRemoveCommand),
    TxComplete(TxCompleteCommand),
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChannelCommandWithId {
    pub channel_id: Hash256,
    pub command: ChannelCommand,
}

pub const HOLDER_INITIAL_COMMITMENT_NUMBER: u64 = 0;
pub const COUNTERPARTY_INITIAL_COMMITMENT_NUMBER: u64 = 2 ^ 48 - 1;
pub const DEFAULT_FEE_RATE: u64 = 0;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 0;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u64 = u64::MAX;
pub const DEFAULT_MAX_ACCEPT_TLCS: u64 = u64::MAX;
pub const DEFAULT_MIN_TLC_VALUE: u64 = 0;
pub const DEFAULT_TO_SELF_DELAY: u64 = 10;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct TxCommand {
    #[serde_as(as = "EntityWrapperHex<Transaction>")]
    pub transaction: Transaction,
}

pub type TxAddCommand = TxCommand;

pub type TxRemoveCommand = TxCommand;

#[derive(Clone, Debug, Deserialize)]
pub struct TxCompleteCommand {}

impl OpenChannelCommand {
    pub fn create_channel(&self) -> Result<ChannelActorState, ProcessingChannelError> {
        // Use a deterministic RNG for now to facilitate development.
        let seed = 42u64.to_le_bytes();

        Ok(ChannelActorState::new_outbound_channel(
            &seed,
            self.peer_id.clone(),
            self.total_value,
            DEFAULT_TO_SELF_DELAY,
        ))
    }
}

pub enum ChannelInitializationParameter {
    /// To open a new channel to another peer, we process OpenChannelCommand
    /// and create a new outgoing channel.
    OpenChannelCommand(OpenChannelCommand),
    /// To accept a new channel from another peer, we process received
    /// OpenChannel message and create a incoming channel.
    OpenChannelMessage(PeerId, usize, OpenChannel),
}

#[derive(Debug)]
pub struct ChannelActor {
    peer_id: PeerId,
    network: ActorRef<NetworkActorMessage>,
}

impl ChannelActor {
    pub fn new(peer_id: PeerId, network: ActorRef<NetworkActorMessage>) -> Self {
        Self { peer_id, network }
    }

    pub fn send_tx_collaboration_command(
        &self,
        state: &mut ChannelActorState,
        command: TxCollaborationCommand,
    ) -> Result<(), ProcessingChannelError> {
        let pcn_msg = match command {
            TxCollaborationCommand::TxAdd(tx_add) => PCNMessage::TxAdd(TxAdd {
                channel_id: state.id(),
                tx: tx_add.transaction.clone(),
            }),
            TxCollaborationCommand::TxRemove(tx_remove) => PCNMessage::TxRemove(TxRemove {
                channel_id: state.id(),
                tx: tx_remove.transaction.clone(),
            }),
            TxCollaborationCommand::TxComplete(_) => PCNMessage::TxComplete(TxComplete {
                channel_id: state.id(),
            }),
        };
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId::new(
                    self.peer_id.clone(),
                    pcn_msg,
                )),
            ))
            .expect("network actor alive");
        Ok(())
    }

    pub fn handle_commitment_signed_command(
        &self,
        state: &mut ChannelActorState,
    ) -> ProcessingChannelResult {
        let flags = match state.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, as collaboration is not completed yet.",
                    &state.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                debug!(
                    "Processing commitment_signed command in from CollaboratingFundingTx state {:?}",
                    &state.state
                );
                SigningCommitmentFlags::empty()
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
                debug!(
                    "Processing commitment_signed command in from SigningCommitment state {:?}",
                    &state.state
                );
                flags
            }

            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &state.state
                )));
            }
        };

        let (tx, partial_signature) = state.build_and_sign_funding_tx()?;
        debug!(
            "Build a funding tx ({:?}) with partial signature {:?}",
            &tx, &partial_signature
        );

        let commitment_signed = CommitmentSigned {
            channel_id: state.id(),
            partial_signature,
            next_local_nonce: state.next_musig2_pubnonce(),
        };
        debug!(
            "Sending built commitment_signed message: {:?}",
            &commitment_signed
        );
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                    peer_id: state.peer_id.clone(),
                    message: PCNMessage::CommitmentSigned(commitment_signed),
                }),
            ))
            .expect("network actor alive");

        state.holder_commitment_number = state.next_commitment_number(true);
        state.state = ChannelState::SigningCommitment(
            flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT,
        );
        Ok(())
    }

    // This is the dual of `handle_tx_collaboration_msg`. Any logic error here is likely
    // to present in the other function as well.
    pub fn handle_tx_collaboration_command(
        &self,
        state: &mut ChannelActorState,
        command: TxCollaborationCommand,
    ) -> Result<(), ProcessingChannelError> {
        debug!("Handling tx collaboration command: {:?}", &command);
        let is_complete_command = match command {
            TxCollaborationCommand::TxComplete(_) => true,
            _ => false,
        };
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
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Acceptor tries to start sending tx collaboration message",
                )));
            }
            ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT)
                if matches!(command, TxCollaborationCommand::TxRemove(_)) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Trying to remove tx in the initial state",
                )));
            }
            ChannelState::NegotiatingFunding(_) => {
                debug!("Beginning processing tx collaboration command");
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
                if flags.contains(CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT) {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Trying to process a tx collaboration command {:?} while in collaboration already completed on our side",
                        &command
                    )));
                }
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

        self.send_tx_collaboration_command(state, command.clone())?;

        // TODO: Note that we may deadlock here if send_tx_collaboration_command does successfully send the message,
        // as in that case both us and the remote are waiting for each other to send the message.
        match command {
            TxCollaborationCommand::TxAdd(tx_add) => {
                state.add_tx_to_funding_tx(tx_add.transaction)?;
                state.state = ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG,
                );
            }
            TxCollaborationCommand::TxRemove(tx_remove) => {
                state.remove_tx_from_funding_tx(tx_remove.transaction)?;
                state.state = ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG,
                );
            }
            TxCollaborationCommand::TxComplete(_) => {
                state.state = ChannelState::CollaboratingFundingTx(
                    flags | CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT,
                );
            }
        }

        Ok(())
    }

    pub fn handle_command(
        &self,
        state: &mut ChannelActorState,
        command: ChannelCommand,
    ) -> Result<(), ProcessingChannelError> {
        match command {
            ChannelCommand::TxCollaborationCommand(tx_collaboration_command) => {
                self.handle_tx_collaboration_command(state, tx_collaboration_command)
            }
            ChannelCommand::CommitmentSigned() => self.handle_commitment_signed_command(state),
        }
    }
}

#[rasync_trait]
impl Actor for ChannelActor {
    type Msg = ChannelActorMessage;
    type State = ChannelActorState;
    type Arguments = ChannelInitializationParameter;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // startup the event processing
        match args {
            ChannelInitializationParameter::OpenChannelMessage(
                peer_id,
                channel_user_id,
                open_channel,
            ) => {
                debug!("Openning channel {:?}", &open_channel);

                let counterpart_pubkeys = (&open_channel).into();
                let OpenChannel {
                    channel_id,
                    chain_hash,
                    funding_type_script,
                    funding_amount,
                    to_self_delay,
                    first_per_commitment_point,
                    second_per_commitment_point,
                    next_local_nonce,
                    ..
                } = &open_channel;

                if *chain_hash != [0u8; 32].into() {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(format!(
                        "Invalid chain hash {:?}",
                        chain_hash
                    ))));
                }

                if funding_type_script.is_some() {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(
                        "Funding type script is not none".to_string(),
                    )));
                }

                let seed = channel_user_id
                    .to_be_bytes()
                    .into_iter()
                    .chain(peer_id.as_bytes().iter().cloned())
                    .collect::<Vec<u8>>();

                let mut state = ChannelActorState::new_inbound_channel(
                    *channel_id,
                    &seed,
                    peer_id.clone(),
                    *funding_amount,
                    *to_self_delay,
                    counterpart_pubkeys,
                    next_local_nonce.clone(),
                    first_per_commitment_point.clone(),
                    second_per_commitment_point.clone(),
                );

                let commitment_number = COUNTERPARTY_INITIAL_COMMITMENT_NUMBER;

                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: open_channel.funding_amount,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    to_self_delay: *to_self_delay,
                    funding_pubkey: state.signer.funding_key.pubkey(),
                    revocation_basepoint: state.signer.revocation_base_key.pubkey(),
                    payment_basepoint: state.signer.payment_key.pubkey(),
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    delayed_payment_basepoint: state.signer.delayed_payment_base_key.pubkey(),
                    tlc_basepoint: state.signer.tlc_base_key.pubkey(),
                    first_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number - 1),
                    next_local_nonce: state.get_holder_musig2_pubnonce(),
                };

                let command = PCNMessageWithPeerId {
                    peer_id,
                    message: PCNMessage::AcceptChannel(accept_channel),
                };
                // TODO: maybe we should not use try_send here.
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(command),
                    ))
                    .expect("network actor alive");

                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelCreated(state.id(), self.peer_id.clone(), myself),
                    ))
                    .expect("peer actor alive");
                state.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT);
                Ok(state)
            }
            ChannelInitializationParameter::OpenChannelCommand(open_channel) => {
                info!("Trying to open a channel to {:?}", &open_channel.peer_id);

                let mut channel = open_channel.create_channel()?;

                let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
                let message = PCNMessage::OpenChannel(OpenChannel {
                    chain_hash: Hash256::default(),
                    channel_id: channel.id(),
                    funding_type_script: None,
                    funding_amount: channel.total_value,
                    funding_fee_rate: DEFAULT_FEE_RATE,
                    commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    to_self_delay: DEFAULT_TO_SELF_DELAY,
                    channel_flags: 0,
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    funding_pubkey: channel.holder_channel_parameters().pubkeys.funding_pubkey,
                    revocation_basepoint: channel
                        .holder_channel_parameters()
                        .pubkeys
                        .revocation_base_key,
                    payment_basepoint: channel.holder_channel_parameters().pubkeys.payment_base_key,
                    delayed_payment_basepoint: channel
                        .holder_channel_parameters()
                        .pubkeys
                        .delayed_payment_base_key,
                    tlc_basepoint: channel.holder_channel_parameters().pubkeys.tlc_base_key,
                    next_local_nonce: channel.get_holder_musig2_pubnonce(),
                });

                debug!(
                    "Created OpenChannel message to {:?}: {:?}",
                    &open_channel.peer_id, &message
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                            peer_id: open_channel.peer_id,
                            message,
                        }),
                    ))
                    .expect("network actor alive");
                // TODO: note that we can't actually guarantee that this OpenChannel message is sent here.
                // It is even possible that the peer_id is bogus, and we can't send a message to it.
                // We need some book-keeping service to remove all the OUR_INIT_SENT channels.
                channel.state =
                    ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT);
                debug!(
                    "Channel to peer {:?} with id {:?} created: {:?}",
                    &self.peer_id,
                    &channel.id(),
                    &channel
                );
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelCreated(
                            channel.id(),
                            self.peer_id.clone(),
                            myself,
                        ),
                    ))
                    .expect("network actor alive");
                Ok(channel)
            }
        }
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ChannelActorMessage::PeerMessage(message) => {
                if let Err(error) = state.handle_peer_message(message, self.network.clone()) {
                    error!("Error while processing channel message: {:?}", error);
                }
            }
            ChannelActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(state, command) {
                    error!("Error while processing channel command: {:?}", err);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FundingTxInput(Transaction);

impl Eq for FundingTxInput {}

impl PartialEq for FundingTxInput {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes() == other.0.as_bytes()
    }
}

impl From<Transaction> for FundingTxInput {
    fn from(tx: Transaction) -> Self {
        Self(tx)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelActorState {
    pub state: ChannelState,
    pub peer_id: PeerId,
    pub temp_id: Hash256,

    pub funding_tx: Option<TransactionView>,

    // Is this channel initially inbound?
    // An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,

    pub total_value: u64,
    pub to_self_value: u64,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    pub holder_channel_parameters: ChannelParametersOneParty,

    // Commitment numbers that are used to derive keys.
    pub holder_commitment_number: u64,
    pub counterparty_commitment_number: u64,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).
    pub id: Option<Hash256>,
    pub counterparty_commitment_point: Option<Pubkey>,
    pub counterparty_prev_commitment_point: Option<Pubkey>,
    pub counterparty_channel_parameters: Option<ChannelParametersOneParty>,
}

fn blake2b_hash_with_salt(data: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(salt);
    hasher.update(data);
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ClosedChannel {}

#[derive(Debug)]
pub enum ChannelEvent {
    AcceptChannel(AcceptChannel),
    TxCollaborationMsg(TxCollaborationMsg),
    CommitmentSigned(CommitmentSigned),
    ChannelReady(ChannelReady),
}

pub type ProcessingChannelResult = Result<(), ProcessingChannelError>;

#[derive(Error, Debug)]
pub enum ProcessingChannelError {
    #[error("Invalid chain hash: {0}")]
    InvalidChainHash(Hash256),
    #[error("Unsupported operation: {0}")]
    Unsupported(String),
    #[error("Invalid state: ")]
    InvalidState(String),
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("Unimplemented operation: {0}")]
    Unimplemented(String),
    #[error("Failed to send command: {0}")]
    CommanderSendingError(#[from] TrySendError<NetworkActorCommand>),
    #[error("Musig2 VerifyError: {0}")]
    Musig2VerifyError(#[from] VerifyError),
    #[error("Musig2 SigningError: {0}")]
    Musig2SigningError(#[from] SigningError),
}

bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct NegotiatingFundingFlags: u32 {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = NegotiatingFundingFlags::OUR_INIT_SENT.bits() | NegotiatingFundingFlags::THEIR_INIT_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct CollaboratingFundingTxFlags: u32 {
        const AWAITING_REMOTE_TX_COLLABORATION_MSG = 1;
        const PREPARING_LOCAL_TX_COLLABORATION_MSG = 1 << 1;
        const OUR_TX_COMPLETE_SENT = 1 << 2;
        const THEIR_TX_COMPLETE_SENT = 1 << 3;
        const COLLABRATION_COMPLETED = CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT.bits() | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct SigningCommitmentFlags: u32 {
        const OUR_COMMITMENT_SIGNED_SENT = 1;
        const THEIR_COMMITMENT_SIGNED_SENT = 1 << 1;
        const COMMITMENT_SIGNED_SENT = SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT.bits() | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct AwaitingChannelReadyFlags: u32 {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = AwaitingChannelReadyFlags::OUR_CHANNEL_READY.bits() | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct ChannelReadyFlags: u32 {
        /// Indicates that we have sent a `commitment_signed` but are awaiting the responding
        ///	`revoke_and_ack` message. During this period, we can't generate new messages as
        /// we'd be unable to determine which TLCs they included in their `revoke_and_ack`
        ///	implicit ACK, so instead we have to hold them away temporarily to be sent later.
        const AWAITING_REMOTE_REVOKE = 1;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(CollaboratingFundingTxFlags),
    /// We have sent `funding_created` and are awaiting a `funding_signed` to advance to
    /// `AwaitingChannelReady`. Note that this is nonsense for an inbound channel as we immediately generate
    /// `funding_signed` upon receipt of `funding_created`, so simply skip this state.
    SigningCommitment(SigningCommitmentFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady(ChannelReadyFlags),
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    /// is about to drop us, but we store this anyway.
    ShutdownComplete,
}

fn new_channel_id_from_seed(seed: &[u8]) -> Hash256 {
    blake2b_256(seed).into()
}

fn derive_channel_id_from_revocation_keys(
    revocation_basepoint1: &Pubkey,
    revocation_basepoint2: &Pubkey,
) -> Hash256 {
    let holder_revocation = revocation_basepoint1.0.serialize();
    let counterparty_revocation = revocation_basepoint2.0.serialize();

    let preimage = if holder_revocation >= counterparty_revocation {
        counterparty_revocation
            .into_iter()
            .chain(holder_revocation.into_iter())
            .collect::<Vec<_>>()
    } else {
        holder_revocation
            .into_iter()
            .chain(counterparty_revocation.into_iter())
            .collect()
    };
    new_channel_id_from_seed(&preimage)
}

pub fn get_commitment_secret(commitment_seed: &[u8; 32], commitment_number: u64) -> [u8; 32] {
    // Note that here, we hold the same assumption to bolts for commitment number,
    // i.e. this number should be in the range [0, 2^48).
    let mut res: [u8; 32] = commitment_seed.clone();
    for i in 0..48 {
        let bitpos = 47 - i;
        if commitment_number & (1 << bitpos) == (1 << bitpos) {
            res[bitpos / 8] ^= 1 << (bitpos & 7);
            res = blake2b_256(&res);
        }
    }
    res
}

pub fn get_commitment_point(commitment_seed: &[u8; 32], commitment_number: u64) -> Pubkey {
    Privkey::from(&get_commitment_secret(commitment_seed, commitment_number)).pubkey()
}

impl ChannelActorState {
    pub fn new_inbound_channel<'a>(
        temp_channel_id: Hash256,
        seed: &[u8],
        peer_id: PeerId,
        counterparty_value: u64,
        counterparty_delay: u64,
        counterparty_pubkeys: ChannelBasePublicKeys,
        counterparty_nonce: PubNonce,
        counterparty_commitment_point: Pubkey,
        counterparty_prev_commitment_point: Pubkey,
    ) -> Self {
        let commitment_number = COUNTERPARTY_INITIAL_COMMITMENT_NUMBER;
        let signer = InMemorySigner::generate_from_seed(seed);
        let holder_pubkeys = signer.to_channel_public_keys(commitment_number);

        let channel_id = derive_channel_id_from_revocation_keys(
            &holder_pubkeys.revocation_base_key,
            &counterparty_pubkeys.revocation_base_key,
        );

        debug!(
            "Generated channel id ({:?}) for temporary channel {:?}",
            &channel_id, &temp_channel_id
        );

        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
            peer_id,
            funding_tx: None,
            is_acceptor: true,
            total_value: counterparty_value,
            temp_id: temp_channel_id,
            id: Some(channel_id),
            to_self_value: 0,
            holder_channel_parameters: ChannelParametersOneParty {
                pubkeys: holder_pubkeys,
                nonce: signer.musig2_base_nonce.public_nonce(),
                selected_contest_delay: counterparty_delay as u64,
            },
            signer,
            counterparty_channel_parameters: Some(ChannelParametersOneParty {
                pubkeys: counterparty_pubkeys,
                nonce: counterparty_nonce,
                selected_contest_delay: counterparty_delay,
            }),
            holder_commitment_number: commitment_number,
            counterparty_commitment_number: HOLDER_INITIAL_COMMITMENT_NUMBER,
            counterparty_commitment_point: Some(counterparty_commitment_point),
            counterparty_prev_commitment_point: Some(counterparty_prev_commitment_point),
        }
    }

    pub fn new_outbound_channel(
        seed: &[u8],
        peer_id: PeerId,
        value: u64,
        to_self_delay: u64,
    ) -> Self {
        let new_channel_id = new_channel_id_from_seed(seed);
        let signer = InMemorySigner::generate_from_seed(seed);
        let nonce = signer.musig2_base_nonce.public_nonce();
        let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
        let holder_pubkeys = signer.to_channel_public_keys(commitment_number);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            peer_id,
            funding_tx: None,
            is_acceptor: false,
            total_value: value,
            temp_id: new_channel_id.into(),
            id: None,
            to_self_value: value,
            signer,
            holder_channel_parameters: ChannelParametersOneParty {
                pubkeys: holder_pubkeys,
                nonce,
                selected_contest_delay: to_self_delay,
            },
            counterparty_channel_parameters: None,
            holder_commitment_number: commitment_number,
            counterparty_commitment_number: COUNTERPARTY_INITIAL_COMMITMENT_NUMBER,
            counterparty_commitment_point: None,
            counterparty_prev_commitment_point: None,
        }
    }

    pub fn id(&self) -> Hash256 {
        self.id.unwrap_or(self.temp_id)
    }

    pub fn holder_channel_parameters(&self) -> &ChannelParametersOneParty {
        &self.holder_channel_parameters
    }

    pub fn counterparty_channel_parameters(&self) -> &ChannelParametersOneParty {
        self.counterparty_channel_parameters.as_ref().unwrap()
    }

    pub fn next_commitment_number(&self, local: bool) -> u64 {
        if local {
            self.holder_commitment_number + 1
        } else {
            self.counterparty_commitment_number - 1
        }
    }

    pub fn next_musig2_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.next_commitment_number(true))
    }

    // TODO: make sure we only send this nonce to the counterparty once.
    pub fn next_musig2_pubnonce(&self) -> PubNonce {
        self.next_musig2_secnonce().public_nonce()
    }

    pub fn handle_peer_message(
        &mut self,
        message: PCNMessage,
        network: ActorRef<NetworkActorMessage>,
    ) -> Result<(), ProcessingChannelError> {
        match message {
            PCNMessage::OpenChannel(_) => {
                panic!("OpenChannel message should be processed while prestarting")
            }
            PCNMessage::AcceptChannel(accept_channel) => {
                self.handle_accept_channel_message(accept_channel)?;
                self.fill_in_channel_id();
                network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelAccepted(self.id(), self.temp_id),
                    ))
                    .expect("network actor alive");
                Ok(())
            }
            PCNMessage::TxAdd(tx) => {
                self.handle_tx_collaboration_msg(TxCollaborationMsg::TxAdd(tx))
            }
            PCNMessage::TxRemove(tx) => {
                self.handle_tx_collaboration_msg(TxCollaborationMsg::TxRemove(tx))
            }
            PCNMessage::TxComplete(tx) => {
                self.handle_tx_collaboration_msg(TxCollaborationMsg::TxComplete(tx))
            }
            PCNMessage::CommitmentSigned(commitment_signed) => {
                self.handle_commitment_signed_message(commitment_signed)?;
                if let ChannelState::SigningCommitment(flags) = self.state {
                    if !flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT) {
                        // TODO: maybe we should send ouor commitment_signed message here.
                        debug!("CommitmentSigned message received, but we haven't sent our commitment_signed message yet");
                    }
                }
                {}
                Ok(())
            }
            PCNMessage::ChannelReady(channel_ready) => {
                match self.state {
                    ChannelState::SigningCommitment(_) => {
                        self.state = ChannelState::AwaitingChannelReady(
                            AwaitingChannelReadyFlags::OUR_CHANNEL_READY,
                        );
                    }
                    ChannelState::AwaitingChannelReady(
                        AwaitingChannelReadyFlags::THEIR_CHANNEL_READY,
                    ) => {
                        self.state = ChannelState::ChannelReady(ChannelReadyFlags::empty());
                    }
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(
                            "received ChannelReady message, but we're not ready for ChannelReady"
                                .to_string(),
                        ));
                    }
                }
                debug!("ChannelReady: {:?}", &channel_ready);
                Ok(())
            }
            _ => {
                warn!("Received unsupported message: {:?}", &message);
                Ok(())
            }
        }
    }

    pub fn handle_accept_channel_message(
        &mut self,
        accept_channel: AcceptChannel,
    ) -> ProcessingChannelResult {
        if accept_channel.funding_amount != self.total_value {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "funding_amount mismatch (expected {}, got {})",
                self.total_value, accept_channel.funding_amount
            )));
        }

        if self.state != ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT) {
            return Err(ProcessingChannelError::InvalidState(format!(
                "accepting a channel while in state {:?}, expecting NegotiatingFundingFlags::OUR_INIT_SENT",
                self.state
            )));
        }

        self.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT);
        let counterparty_pubkeys = (&accept_channel).into();
        self.counterparty_channel_parameters = Some(ChannelParametersOneParty {
            pubkeys: counterparty_pubkeys,
            nonce: accept_channel.next_local_nonce.clone(),
            selected_contest_delay: accept_channel.to_self_delay as u64,
        });

        debug!(
            "Successfully processed AcceptChannel message {:?}",
            &accept_channel
        );
        Ok(())
    }

    // This is the dual of `handle_tx_collaboration_command`. Any logic error here is likely
    // to present in the other function as well.
    pub fn handle_tx_collaboration_msg(
        &mut self,
        msg: TxCollaborationMsg,
    ) -> ProcessingChannelResult {
        debug!("Processing tx collaboration message: {:?}", &msg);
        let is_complete_message = match msg {
            TxCollaborationMsg::TxComplete(_) => true,
            _ => false,
        };
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
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Initiator received a tx collaboration message",
                )));
            }
            ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT)
                if matches!(msg, TxCollaborationMsg::TxRemove(_)) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Recevied a TxRemove message from the start of tx collaboration"
                )));
            }
            ChannelState::NegotiatingFunding(_) => {
                debug!("Beginning processing tx collaboration message");
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
                        "Trying to process a tx collaboration message {:?} while in collaboration already completed on our side",
                        &msg
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
            TxCollaborationMsg::TxAdd(msg) => {
                self.add_tx_to_funding_tx(msg.tx)?;
                self.state = ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::PREPARING_LOCAL_TX_COLLABORATION_MSG,
                );
            }
            TxCollaborationMsg::TxRemove(msg) => {
                self.remove_tx_from_funding_tx(msg.tx)?;
                self.state = ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::PREPARING_LOCAL_TX_COLLABORATION_MSG,
                );
            }
            TxCollaborationMsg::TxComplete(_msg) => {
                self.state = ChannelState::CollaboratingFundingTx(
                    flags | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT,
                );
            }
        }
        Ok(())
    }

    pub fn handle_commitment_signed_message(
        &mut self,
        commitment_signed: CommitmentSigned,
    ) -> ProcessingChannelResult {
        let flags = match self.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, as collaboration is not completed yet.",
                    &self.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                debug!(
                    "Processing commitment_signed command in from CollaboratingFundingTx state {:?}",
                    &self.state
                );
                SigningCommitmentFlags::empty()
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed message in state {:?}, as we have already received our commitment_signed message.",
                    &self.state
                )));
            }
            ChannelState::SigningCommitment(flags) => {
                debug!(
                    "Processing commitment_signed command in from SigningCommitment state {:?}",
                    &self.state
                );
                flags
            }

            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &self.state
                )));
            }
        };

        let tx = self.build_and_verify_commitment_tx(commitment_signed.partial_signature)?;

        debug!(
            "Successfuly handled commitment signed message: {:?}, tx: {:?}",
            &commitment_signed, &tx
        );

        debug!("Updating peer next local nonce");
        self.counterparty_channel_parameters.as_mut().unwrap().nonce =
            commitment_signed.next_local_nonce;

        self.state = ChannelState::SigningCommitment(
            flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT,
        );
        Ok(())
    }

    pub fn add_tx_to_funding_tx(&mut self, tx: Transaction) -> ProcessingChannelResult {
        // TODO check if the tx is valid
        self.funding_tx = Some(tx.into_view());
        Ok(())
    }

    pub fn remove_tx_from_funding_tx(&mut self, tx: Transaction) -> ProcessingChannelResult {
        // TODO check if the tx is valid and removed inputs belong to the remote peer
        self.funding_tx = Some(tx.into_view());
        Ok(())
    }

    pub fn fill_in_channel_id(&mut self) {
        assert!(self.id.is_none(), "Channel id is already filled in");
        assert!(
            self.counterparty_channel_parameters.is_some(),
            "Counterparty pubkeys is required to derive actual channel id"
        );
        let counterparty_revocation = &self
            .counterparty_channel_parameters()
            .pubkeys
            .revocation_base_key;
        let holder_revocation = &self.holder_channel_parameters().pubkeys.revocation_base_key;
        let channel_id =
            derive_channel_id_from_revocation_keys(&holder_revocation, &counterparty_revocation);

        self.id = Some(channel_id);
        debug!(
            "Channel Id changed from {:?} to {:?}",
            &self.temp_id,
            &self.id.unwrap()
        );
    }

    pub fn is_funded(&self) -> bool {
        match self.state {
            ChannelState::ChannelReady(_) => {
                assert!(self.funding_tx.is_some());
                assert!(self.id.is_some());
                assert!(self.counterparty_commitment_point.is_some());
                assert!(self.counterparty_prev_commitment_point.is_some());
                assert!(self.counterparty_channel_parameters.is_some());
                true
            }
            _ => false,
        }
    }

    pub fn must_get_funding_transaction(&self) -> OutPoint {
        self.funding_tx
            .as_ref()
            .expect("Funding transaction is present")
            .output_pts()
            .first()
            .expect("Funding transaction output is present")
            .clone()
    }

    fn should_holder_go_first(&self) -> bool {
        let holder_pubkey = self.holder_channel_parameters().pubkeys.funding_pubkey;
        let counterparty_pubkey = self
            .counterparty_channel_parameters()
            .pubkeys
            .funding_pubkey;
        holder_pubkey <= counterparty_pubkey
    }

    pub fn get_musig2_agg_context(&self) -> KeyAggContext {
        let holder_pubkey = self
            .holder_channel_parameters()
            .pubkeys
            .funding_pubkey
            .clone();
        let counterparty_pubkey = self
            .counterparty_channel_parameters()
            .pubkeys
            .funding_pubkey
            .clone();
        let keys = if self.should_holder_go_first() {
            vec![holder_pubkey, counterparty_pubkey]
        } else {
            vec![counterparty_pubkey, holder_pubkey]
        };
        KeyAggContext::new(keys).expect("Valid pubkeys")
    }

    pub fn get_holder_musig2_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.holder_commitment_number)
    }

    pub fn get_holder_musig2_pubnonce(&self) -> PubNonce {
        self.get_holder_musig2_secnonce().public_nonce()
    }

    pub fn get_musig2_agg_pubnonce(&self) -> AggNonce {
        let holder_nonce = self.get_holder_musig2_pubnonce();
        let counterparty_nonce = self.counterparty_channel_parameters().nonce.clone();
        let nonces = if self.should_holder_go_first() {
            vec![holder_nonce, counterparty_nonce]
        } else {
            vec![counterparty_nonce, holder_nonce]
        };
        AggNonce::sum(nonces)
    }

    pub fn get_amounts_for_both_party(&self, local: bool) -> (u64, u64) {
        // TODO: consider transaction fee here.
        // TODO: exclude all the timelocked values here.
        if local {
            (self.to_self_value, self.total_value - self.to_self_value)
        } else {
            (self.total_value - self.to_self_value, self.to_self_value)
        }
    }

    pub fn get_tlcs_for_commitment_tx(&self) -> Vec<TLCOutputInCommitment> {
        // TODO: get tlc outputs for commitment transaction
        vec![]
    }

    pub fn build_commitment_tx(&self, local: bool) -> CommitmentTransaction {
        let (to_broadcaster_value, to_countersignatory_value) =
            self.get_amounts_for_both_party(local);

        let commitment_number = if local {
            self.holder_commitment_number
        } else {
            self.counterparty_commitment_number
        };
        debug!(
            "Building commitment transaction #{} for {}",
            commitment_number,
            if local { "us" } else { "them" },
        );
        let (
            broadcaster,
            countersignatory,
            broadcaster_commitment_point,
            countersignatory_commitment_point,
        ) = if local {
            (
                self.holder_channel_parameters(),
                self.counterparty_channel_parameters(),
                self.signer.get_commitment_point(commitment_number),
                self.counterparty_commitment_point.unwrap(),
            )
        } else {
            (
                self.counterparty_channel_parameters(),
                self.holder_channel_parameters(),
                self.counterparty_commitment_point.unwrap(),
                self.signer.get_commitment_point(commitment_number),
            )
        };
        let tx_creation_keys = TxCreationKeys {
            broadcaster_delayed_payment_key: derive_delayed_payment_pubkey(
                broadcaster.delayed_payment_base_key(),
                &broadcaster_commitment_point,
            ),
            countersignatory_payment_key: derive_payment_pubkey(
                countersignatory.payment_base_key(),
                &countersignatory_commitment_point,
            ),
            countersignatory_revocation_key: derive_revocation_pubkey(
                countersignatory.revocation_base_key(),
                &countersignatory_commitment_point,
            ),
            broadcaster_tlc_key: derive_tlc_pubkey(
                broadcaster.tlc_base_key(),
                &broadcaster_commitment_point,
            ),
            countersignatory_tlc_key: derive_tlc_pubkey(
                countersignatory.tlc_base_key(),
                &countersignatory_commitment_point,
            ),
        };

        let tlcs = self.get_tlcs_for_commitment_tx();

        CommitmentTransaction {
            commitment_number,
            to_broadcaster_value,
            to_countersignatory_value,
            tlcs,
            keys: tx_creation_keys,
        }
    }

    pub fn get_verify_context(&self) -> VerifyContext {
        VerifyContext {
            key_agg_ctx: self.get_musig2_agg_context(),
            aggnonce: self.get_musig2_agg_pubnonce(),
            pubkey: *self.get_counterparty_funding_pubkey(),
            pubnonce: self.counterparty_channel_parameters().nonce.clone(),
        }
    }

    pub fn build_and_verify_commitment_tx(
        &self,
        signature: PartialSignature,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let commitment_tx = self.build_commitment_tx(false);
        commitment_tx.verify(signature, self.get_verify_context())
    }

    pub fn get_sign_context(&self) -> SignContext {
        SignContext {
            key_agg_ctx: self.get_musig2_agg_context(),
            aggnonce: self.get_musig2_agg_pubnonce(),
            seckey: self.signer.funding_key.clone(),
            secnonce: self.get_holder_musig2_secnonce(),
        }
    }

    pub fn build_and_sign_commitment_tx(
        &self,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let commitment_tx = self.build_commitment_tx(true);
        commitment_tx.sign(self.get_sign_context())
    }

    pub fn build_and_sign_funding_tx(
        &self,
    ) -> Result<(Transaction, PartialSignature), ProcessingChannelError> {
        let funding_tx = self.build_funding_tx(true);
        let message = funding_tx.as_slice();
        debug!(
            "Signing funding transaction {:?} with message {:?}",
            &funding_tx,
            hex::encode(&message)
        );
        let key_agg_ctx = self.get_musig2_agg_context();
        let aggregated_nonce = self.get_musig2_agg_pubnonce();
        let secnonce = self.signer.musig2_base_nonce.clone();
        let partial_signature: PartialSignature = sign_partial(
            &key_agg_ctx,
            self.signer.funding_key,
            secnonce.clone(),
            &aggregated_nonce,
            message,
        )?;
        debug!("Funding tx signed successfully: {:?}", &partial_signature);
        Ok((funding_tx, partial_signature))
    }

    pub fn build_funding_tx(&self, local: bool) -> Transaction {
        let (_commitment_number, _broadcaster_pubkeys, _countersignatory_pubkeys) = if local {
            (
                self.holder_commitment_number,
                self.holder_channel_parameters().pubkeys.clone(),
                self.counterparty_channel_parameters().pubkeys.clone(),
            )
        } else {
            (
                self.counterparty_commitment_number,
                self.counterparty_channel_parameters().pubkeys.clone(),
                self.holder_channel_parameters().pubkeys.clone(),
            )
        };
        // TODO: Building funding here;
        Transaction::default()
    }

    pub fn get_counterparty_funding_pubkey(&self) -> &Pubkey {
        &self
            .counterparty_channel_parameters()
            .pubkeys
            .funding_pubkey
    }

    pub fn must_get_funded_channel_parameters(&self) -> FundedChannelParameters<'_> {
        FundedChannelParameters {
            holder: &self.holder_channel_parameters(),
            counterparty: self.counterparty_channel_parameters(),
            funding_outpoint: self.must_get_funding_transaction(),
        }
    }

    pub fn must_get_directed_channel_parameters(
        &self,
        holder_is_broadcaster: bool,
    ) -> DirectedChannelTransactionParameters<'_> {
        DirectedChannelTransactionParameters {
            inner: self.must_get_funded_channel_parameters(),
            holder_is_broadcaster,
        }
    }
}

/// The commitment transactions are the transaction that each parties holds to
/// spend the funding transaction and create a new partition of the channel
/// balance between each parties. This struct contains all the information
/// that we need to build the actual CKB transaction.
/// Note that these commitment transactions are asymmetrical,
/// meaning that counterparties have different resulting CKB transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitmentTransaction {
    // Will change after each commitment to derive new commitment secrets.
    // Currently always 0.
    pub commitment_number: u64,
    // The broadcaster's balance, may be spent after timelock by the broadcaster or
    // by the countersignatory with revocation key.
    pub to_broadcaster_value: u64,
    // The countersignatory's balance, may be spent immediately by the countersignatory.
    pub to_countersignatory_value: u64,
    // The list of TLC commitmentments. These outputs are already multisiged to another
    // set of transactions, whose output may be spent by countersignatory with revocation key,
    // the original sender after delay, or the receiver if they has correct preimage,
    pub tlcs: Vec<TLCOutputInCommitment>,
    // A cache of the parties' pubkeys required to construct the transaction.
    pub keys: TxCreationKeys,
}

/// A wrapper on CommitmentTransaction that has a verified ckb transaction built
/// along with it.
#[derive(Clone, Debug)]
pub struct VerifiedCommitmentTransaction {
    pub inner: CommitmentTransaction,
    pub tx: TransactionView,
}

/// A wrapper on CommitmentTransaction that has a partial signature along with
/// the ckb transaction.
#[derive(Clone, Debug)]
pub struct PartiallySignedCommitmentTransaction {
    inner: CommitmentTransaction,
    tx: TransactionView,
    signature: PartialSignature,
}

pub struct VerifyContext {
    pub key_agg_ctx: KeyAggContext,
    pub aggnonce: AggNonce,
    pub pubkey: Pubkey,
    pub pubnonce: PubNonce,
}

pub struct SignContext {
    key_agg_ctx: KeyAggContext,
    aggnonce: AggNonce,
    seckey: Privkey,
    secnonce: SecNonce,
}

impl CommitmentTransaction {
    pub fn gen_tx(&self) -> TransactionView {
        // TODO: implement build transaction here.
        Transaction::default().into_view()
    }

    pub fn sign(
        self,
        sign_ctx: SignContext,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let tx = self.gen_tx();
        let message = tx.hash();

        let SignContext {
            key_agg_ctx,
            aggnonce,
            seckey,
            secnonce,
        } = sign_ctx;
        let signature = sign_partial(
            &key_agg_ctx,
            seckey,
            secnonce,
            &aggnonce,
            message.as_slice(),
        )?;
        Ok(PartiallySignedCommitmentTransaction {
            inner: self,
            tx,
            signature,
        })
    }

    pub fn verify(
        self,
        signature: PartialSignature,
        verify_ctx: VerifyContext,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let tx = self.gen_tx();
        let message = tx.hash();

        let VerifyContext {
            key_agg_ctx,
            aggnonce,
            pubkey,
            pubnonce: nonce,
        } = verify_ctx;

        verify_partial(
            &key_agg_ctx,
            signature,
            &aggnonce,
            pubkey,
            &nonce,
            message.as_slice(),
        )?;
        Ok(PartiallySignedCommitmentTransaction {
            inner: self,
            tx,
            signature,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelParametersOneParty {
    pub pubkeys: ChannelBasePublicKeys,
    pub nonce: PubNonce,
    pub selected_contest_delay: u64,
}

impl ChannelParametersOneParty {
    pub fn funding_pubkey(&self) -> &Pubkey {
        &self.pubkeys.funding_pubkey
    }

    pub fn payment_base_key(&self) -> &Pubkey {
        &self.pubkeys.payment_base_key
    }

    pub fn delayed_payment_base_key(&self) -> &Pubkey {
        &self.pubkeys.delayed_payment_base_key
    }

    pub fn revocation_base_key(&self) -> &Pubkey {
        &self.pubkeys.revocation_base_key
    }

    pub fn tlc_base_key(&self) -> &Pubkey {
        &self.pubkeys.tlc_base_key
    }
}

pub struct FundedChannelParameters<'a> {
    pub holder: &'a ChannelParametersOneParty,
    pub counterparty: &'a ChannelParametersOneParty,
    pub funding_outpoint: OutPoint,
}

/// Static channel fields used to build transactions given per-commitment fields, organized by
/// broadcaster/countersignatory.
///
/// This is derived from the holder/counterparty-organized ChannelTransactionParameters via the
/// as_holder_broadcastable and as_counterparty_broadcastable functions.
pub struct DirectedChannelTransactionParameters<'a> {
    /// The holder's channel static parameters
    inner: FundedChannelParameters<'a>,
    /// Whether the holder is the broadcaster
    holder_is_broadcaster: bool,
}

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelBasePublicKeys {
    /// The public key which is used to sign all commitment transactions, as it appears in the
    /// on-chain channel lock-in 2-of-2 multisig output.
    pub funding_pubkey: Pubkey,
    /// The base point which is used (with derive_public_revocation_key) to derive per-commitment
    /// revocation keys. This is combined with the per-commitment-secret generated by the
    /// counterparty to create a secret which the counterparty can reveal to revoke previous
    /// states.
    pub revocation_base_key: Pubkey,
    /// The public key on which the non-broadcaster (ie the countersignatory) receives an immediately
    /// spendable primary channel balance on the broadcaster's commitment transaction. This key is
    /// static across every commitment transaction.
    pub payment_base_key: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment payment
    /// public key which receives non-HTLC-encumbered funds which are only available for spending
    /// after some delay (or can be claimed via the revocation path).
    pub delayed_payment_base_key: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment public key
    /// which is used to encumber HTLC-in-flight outputs.
    pub tlc_base_key: Pubkey,
}

impl From<&OpenChannel> for ChannelBasePublicKeys {
    fn from(value: &OpenChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey.clone(),
            revocation_base_key: value.revocation_basepoint.clone(),
            payment_base_key: value.payment_basepoint.clone(),
            delayed_payment_base_key: value.delayed_payment_basepoint.clone(),
            tlc_base_key: value.tlc_basepoint.clone(),
        }
    }
}

impl From<&AcceptChannel> for ChannelBasePublicKeys {
    fn from(value: &AcceptChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey.clone(),
            revocation_base_key: value.revocation_basepoint.clone(),
            payment_base_key: value.payment_basepoint.clone(),
            delayed_payment_base_key: value.delayed_payment_basepoint.clone(),
            tlc_base_key: value.tlc_basepoint.clone(),
        }
    }
}

pub struct CounterpartyChannelTransactionParameters {
    /// Counter-party public keys
    pub pubkeys: ChannelBasePublicKeys,
    /// The contest delay selected by the counterparty, which applies to holder-broadcast transactions
    pub selected_contest_delay: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TLCOutputInCommitment {
    /// Whether the HTLC was "offered" (ie outbound in relation to this commitment transaction).
    /// Note that this is not the same as whether it is ountbound *from us*. To determine that you
    /// need to compare this value to whether the commitment transaction in question is that of
    /// the counterparty or our own.
    pub offered: bool,
    /// The value, in msat, of the HTLC. The value as it appears in the commitment transaction is
    /// this divided by 1000.
    pub amount: u64,
    /// The CLTV lock-time at which this HTLC expires.
    pub lock_time: u32,
    /// The hash of the preimage which unlocks this HTLC.
    pub payment_hash: Hash256,
    /// The position within the commitment transactions' outputs. This may be None if the value is
    /// below the dust limit (in which case no output appears in the commitment transaction and the
    /// value is spent to additional transaction fees).
    pub transaction_output_index: Option<u32>,
}

/// The set of public keys which are used in the creation of one commitment transaction.
/// These are derived from the channel base keys and per-commitment data.
///
/// A broadcaster key is provided from potential broadcaster of the computed transaction.
/// A countersignatory key is coming from a protocol participant unable to broadcast the
/// transaction.
///
/// These keys are assumed to be good, either because the code derived them from
/// channel basepoints via the new function, or they were obtained via
/// CommitmentTransaction.trust().keys() because we trusted the source of the
/// pre-calculated keys.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct TxCreationKeys {
    /// Broadcaster's Payment Key (which isn't allowed to be spent from for some delay)
    pub broadcaster_delayed_payment_key: Pubkey,
    /// Countersignatory's payment key, used to receiving balance that should go to
    /// the countersignatory immediately.
    pub countersignatory_payment_key: Pubkey,
    /// The revocation key which is used to allow the broadcaster of the commitment
    /// transaction to provide their counterparty the ability to punish them if they broadcast
    /// an old state.
    pub countersignatory_revocation_key: Pubkey,
    /// Broadcaster's HTLC Key
    pub broadcaster_tlc_key: Pubkey,
    /// Countersignatory's HTLC Key
    pub countersignatory_tlc_key: Pubkey,
}

pub fn derive_private_key(secret: &Privkey, _per_commitment_point: &Pubkey) -> Privkey {
    // TODO: Currently we only copy the input secret. We need to actually derive new private keys
    // from the per_commitment_point.
    secret.clone()
}

/// A simple implementation of [`WriteableEcdsaChannelSigner`] that just keeps the private keys in memory.
///
/// This implementation performs no policy checks and is insufficient by itself as
/// a secure external signer.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct InMemorySigner {
    /// Holder secret key in the 2-of-2 multisig script of a channel. This key also backs the
    /// holder's anchor output in a commitment transaction, if one is present.
    pub funding_key: Privkey,
    /// Holder secret key for blinded revocation pubkey.
    pub revocation_base_key: Privkey,
    /// Holder secret key used for our balance in counterparty-broadcasted commitment transactions.
    pub payment_key: Privkey,
    /// Holder secret key used in an HTLC transaction.
    pub delayed_payment_base_key: Privkey,
    /// Holder HTLC secret key used in commitment transaction HTLC outputs.
    pub tlc_base_key: Privkey,
    /// SecNonce used to generate valid signature in musig.
    // TODO: use rust's ownership to make sure musig_nonce is used once.
    pub musig2_base_nonce: SecNonce,
    /// Seed to derive above keys (per commitment).
    pub commitment_seed: [u8; 32],
}

pub fn derive_revocation_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    base_key.clone()
}

pub fn derive_payment_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    base_key.clone()
}

pub fn derive_delayed_payment_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    base_key.clone()
}

pub fn derive_tlc_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    base_key.clone()
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
        let revocation_base_key = key_derive(funding_key.as_ref(), b"revocation base key");
        let payment_key = key_derive(revocation_base_key.as_ref(), b"payment key");
        let delayed_payment_base_key =
            key_derive(payment_key.as_ref(), b"delayed payment base key");
        let tlc_base_key = key_derive(delayed_payment_base_key.as_ref(), b"HTLC base key");
        let misig_nonce = key_derive(tlc_base_key.as_ref(), b"musig nocne");
        let musig_nonce = SecNonce::build(misig_nonce.as_ref()).build();

        Self {
            funding_key,
            revocation_base_key,
            payment_key,
            delayed_payment_base_key,
            tlc_base_key,
            musig2_base_nonce: musig_nonce,
            commitment_seed,
        }
    }

    fn to_channel_public_keys(&self, commitment_number: u64) -> ChannelBasePublicKeys {
        ChannelBasePublicKeys {
            funding_pubkey: self.funding_key.pubkey(),
            revocation_base_key: self.derive_revocation_key(commitment_number).pubkey(),
            payment_base_key: self.derive_payment_key(commitment_number).pubkey(),
            delayed_payment_base_key: self.derive_delayed_payment_key(commitment_number).pubkey(),
            tlc_base_key: self.derive_tlc_key(commitment_number).pubkey(),
        }
    }

    pub fn get_commitment_point(&self, commitment_number: u64) -> Pubkey {
        get_commitment_point(&self.commitment_seed, commitment_number)
    }

    pub fn get_commitment_secret(&self, commitment_number: u64) -> [u8; 32] {
        get_commitment_secret(&self.commitment_seed, commitment_number)
    }

    pub fn derive_revocation_key(&self, commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(commitment_number);
        derive_private_key(&self.revocation_base_key, &per_commitment_point)
    }

    pub fn derive_payment_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.payment_key, &per_commitment_point)
    }

    pub fn derive_delayed_payment_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.delayed_payment_base_key, &per_commitment_point)
    }

    pub fn derive_tlc_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.tlc_base_key, &per_commitment_point)
    }

    pub fn derive_musig2_nonce(&self, _new_commitment_number: u64) -> SecNonce {
        // TODO: generate new musig nonce here
        self.musig2_base_nonce.clone()
    }
}

#[cfg(test)]
mod tests {
    use ckb_types::{
        packed::Transaction,
        prelude::IntoTransactionView,
        prelude::{Pack, PackVec},
    };
    use molecule::prelude::{Builder, Entity};

    use crate::ckb::types::Privkey;

    use super::{derive_private_key, derive_public_key};

    #[test]
    fn test_derive_private_and_public_keys() {
        let privkey = Privkey::from(&[1; 32]);
        let per_commitment_point = Privkey::from(&[2; 32]).pubkey();
        let derived_privkey = derive_private_key(&privkey, &per_commitment_point);
        let derived_pubkey = derive_public_key(&privkey.pubkey(), &per_commitment_point);
        assert_eq!(derived_privkey.pubkey(), derived_pubkey);
    }

    #[test]
    fn test_deserialize_transaction() {
        let tx_builder = Transaction::new_builder();
        let tx_builder = tx_builder.witnesses(vec![vec![0u8].pack()].pack());
        let tx = tx_builder.build();
        let tx_view = tx.into_view();
        dbg!(tx_view);
    }
}
