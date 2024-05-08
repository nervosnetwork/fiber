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
    aggregate_partial_signatures,
    errors::{SigningError, VerifyError},
    sign_partial, verify_partial, AggNonce, CompactSignature, KeyAggContext, PartialSignature,
    PubNonce, SecNonce,
};
use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef, SpawnErr};
use serde::Deserialize;
use serde_with::serde_as;
use tentacle::secio::PeerId;
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;

use std::{borrow::Borrow, collections::HashMap, fmt::Debug};

use super::{
    key::blake2b_hash_with_salt,
    network::{OpenChannelCommand, PCNMessageWithPeerId},
    serde_utils::EntityWrapperHex,
    types::{
        AcceptChannel, AddTlc, ChannelReady, CommitmentSigned, Hash256, LockTime, OpenChannel,
        PCNMessage, Privkey, Pubkey, RemoveTlc, RemoveTlcReason, TxCollaborationMsg, TxComplete,
        TxSignatures, TxUpdate,
    },
    NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
};

pub enum ChannelActorMessage {
    /// Command are the messages that are sent to the channel actor to perform some action.
    /// It is normally generated from a user request.
    Command(ChannelCommand),
    /// Some system events associated to a channel, such as the funding transaction confirmed.
    Event(ChannelEvent),
    /// PeerMessage are the messages sent from the peer.
    PeerMessage(PCNMessage),
}

#[derive(Clone, Debug, Deserialize)]
pub enum ChannelCommand {
    TxCollaborationCommand(TxCollaborationCommand),
    // TODO: maybe we should automatically send commitment_signed message after receiving
    // tx_complete event.
    CommitmentSigned(),
    AddTlc(AddTlcCommand),
    RemoveTlc(RemoveTlcCommand),
}

#[derive(Clone, Debug, Deserialize)]
pub enum TxCollaborationCommand {
    TxUpdate(TxUpdateCommand),
    TxComplete(TxCompleteCommand),
}

#[derive(Copy, Clone, Debug, Deserialize)]
pub struct AddTlcCommand {
    amount: u128,
    preimage: Option<Hash256>,
    expiry: LockTime,
}

#[derive(Copy, Clone, Debug, Deserialize)]
pub struct RemoveTlcCommand {
    id: u64,
    reason: RemoveTlcReason,
}

fn get_random_preimage() -> Hash256 {
    let mut preimage = [0u8; 32];
    preimage.copy_from_slice(&rand::random::<[u8; 32]>());
    preimage.into()
}

impl From<(u64, AddTlcCommand)> for TLC {
    fn from((id, command): (u64, AddTlcCommand)) -> Self {
        let preimage = command.preimage.unwrap_or(get_random_preimage());
        let hash = blake2b_256(&preimage);
        TLC {
            id,
            amount: command.amount,
            payment_hash: hash.into(),
            lock_time: command.expiry,
            is_offered: true,
            payment_preimage: Some(preimage),
        }
    }
}

impl From<AddTlc> for TLC {
    fn from(message: AddTlc) -> Self {
        TLC {
            id: message.tlc_id,
            amount: message.amount,
            payment_hash: message.payment_hash,
            lock_time: message.expiry,
            is_offered: false,
            payment_preimage: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChannelCommandWithId {
    pub channel_id: Hash256,
    pub command: ChannelCommand,
}

pub const HOLDER_INITIAL_COMMITMENT_NUMBER: u64 = 0;
pub const COUNTERPARTY_INITIAL_COMMITMENT_NUMBER: u64 = (2 ^ 48) - 1;
pub const DEFAULT_FEE_RATE: u64 = 0;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 0;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u128 = u128::MAX;
pub const DEFAULT_MAX_ACCEPT_TLCS: u64 = u64::MAX;
pub const DEFAULT_MIN_TLC_VALUE: u128 = 0;
pub const DEFAULT_TO_SELF_DELAY_BLOCKS: u64 = 10;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct TxUpdateCommand {
    #[serde_as(as = "EntityWrapperHex<Transaction>")]
    pub transaction: Transaction,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TxCompleteCommand {}

impl OpenChannelCommand {
    pub fn create_channel(&self) -> Result<ChannelActorState, ProcessingChannelError> {
        // Use a deterministic RNG for now to facilitate development.
        let seed = 42u64.to_le_bytes();

        Ok(ChannelActorState::new_outbound_channel(
            &seed,
            self.peer_id.clone(),
            self.funding_amount,
            LockTime::new(DEFAULT_TO_SELF_DELAY_BLOCKS),
        ))
    }
}

pub enum ChannelInitializationParameter {
    /// To open a new channel to another peer, the funding amount and
    /// a unique channel seed to generate unique channel id,
    /// must be given.
    OpenChannel(u128, [u8; 32]),
    /// To accept a new channel from another peer, the funding amount,
    /// a unique channel seed to generate unique channel id,
    /// and original OpenChannel message must be given.
    AcceptChannel(u128, [u8; 32], OpenChannel),
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
            TxCollaborationCommand::TxUpdate(tx_update) => PCNMessage::TxUpdate(TxUpdate {
                channel_id: state.get_id(),
                tx: tx_update.transaction.clone(),
            }),
            TxCollaborationCommand::TxComplete(_) => PCNMessage::TxComplete(TxComplete {
                channel_id: state.get_id(),
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

        let PartiallySignedCommitmentTransaction {
            inner: _inner,
            tx,
            signature,
        } = state.build_and_sign_commitment_tx()?;
        debug!(
            "Build a funding tx ({:?}) with partial signature {:?}",
            &tx, &signature
        );

        let commitment_signed = CommitmentSigned {
            channel_id: state.get_id(),
            partial_signature: signature,
            next_local_nonce: state.get_next_holder_nonce(),
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

        state.holder_commitment_number = state.get_next_commitment_number(true);
        let flags = flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT;
        state.state = ChannelState::SigningCommitment(flags);
        state.maybe_transition_to_tx_signatures(flags, self.network.clone())?;
        Ok(())
    }

    pub fn handle_add_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: AddTlcCommand,
    ) -> ProcessingChannelResult {
        state.check_state_for_tlc_update()?;

        // TODO: we are filling the user command with a new id here.
        // The advantage of this is that we don't need to burden the users to
        // provide a next id for each tlc. The disadvantage is that users may
        // inadvertently click the same button twice, and we will process the same
        // twice, the frontend needs to prevent this kind of behaviour.
        // Is this what we want?
        let id = state.next_offering_tlc_id;
        assert!(
            state.pending_offered_tlcs.get(&id).is_none(),
            "Must not have the same id in pending offered tlcs"
        );
        let tlc = TLC::from((id, command));

        // TODO: Note that since we are message sending is async,
        // we can't guarantee anything about the order of message sending
        // and state updating. And any of these may fail while the other succeedes.
        // We may need to handle all these possibilities.
        // To make things worse, we currently don't have a way to ACK all the messages.

        // Send tlc update message to peer.
        let msg = PCNMessageWithPeerId {
            peer_id: self.peer_id.clone(),
            message: PCNMessage::AddTlc(AddTlc {
                channel_id: state.get_id(),
                tlc_id: tlc.id,
                amount: tlc.amount,
                payment_hash: tlc.payment_hash,
                expiry: tlc.lock_time,
            }),
        };
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendPcnMessage(msg),
            ))
            .expect("network actor alive");

        // Update the state for this tlc.
        state.pending_offered_tlcs.insert(id, tlc);
        state.next_offering_tlc_id += 1;
        Ok(())
    }

    pub fn handle_remove_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: RemoveTlcCommand,
    ) -> ProcessingChannelResult {
        state.check_state_for_tlc_update()?;
        // Notes: state updating and message sending are not atomic.
        match state.pending_received_tlcs.remove(&command.id) {
            Some(tlc) => {
                let msg = PCNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: PCNMessage::RemoveTlc(RemoveTlc {
                        channel_id: state.get_id(),
                        tlc_id: tlc.id,
                        reason: command.reason,
                    }),
                };
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(msg),
                    ))
                    .expect("network actor alive");
                Ok(())
            }
            None => {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Trying to remove tlc with id {:?} that is not in pending received tlcs",
                    command.id
                )));
            }
        }
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
                return Err(ProcessingChannelError::InvalidState(
                    "Acceptor tries to start sending tx collaboration message".to_string(),
                ));
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
            TxCollaborationCommand::TxUpdate(tx_update) => {
                state.update_funding_tx(tx_update.transaction)?;
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
            ChannelCommand::AddTlc(command) => self.handle_add_tlc_command(state, command),
            ChannelCommand::RemoveTlc(command) => self.handle_remove_tlc_command(state, command),
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
            ChannelInitializationParameter::AcceptChannel(
                my_funding_amount,
                seed,
                open_channel,
            ) => {
                let peer_id = self.peer_id.clone();
                debug!(
                    "Accepting channel {:?} to peer {:?}",
                    &open_channel, &peer_id
                );

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
                    // We have not implemented funding type script yet.
                    // But don't panic, otherwise adversary can easily take us down.
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(
                        "Funding type script is not none, but we are currently unable to process this".to_string(),
                    )));
                }

                let mut state = ChannelActorState::new_inbound_channel(
                    *channel_id,
                    my_funding_amount,
                    &seed,
                    peer_id.clone(),
                    *funding_amount,
                    *to_self_delay,
                    counterpart_pubkeys,
                    next_local_nonce.clone(),
                    *first_per_commitment_point,
                    *second_per_commitment_point,
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
                        NetworkActorEvent::ChannelCreated(
                            state.get_id(),
                            self.peer_id.clone(),
                            myself,
                        ),
                    ))
                    .expect("peer actor alive");
                state.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT);
                Ok(state)
            }
            ChannelInitializationParameter::OpenChannel(funding_amount, seed) => {
                let peer_id = self.peer_id.clone();
                info!("Trying to open a channel to {:?}", &peer_id);

                let mut channel = ChannelActorState::new_outbound_channel(
                    &seed,
                    self.peer_id.clone(),
                    funding_amount,
                    LockTime::new(DEFAULT_TO_SELF_DELAY_BLOCKS),
                );

                let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
                let message = PCNMessage::OpenChannel(OpenChannel {
                    chain_hash: Hash256::default(),
                    channel_id: channel.get_id(),
                    funding_type_script: None,
                    funding_amount: channel.to_self_amount,
                    funding_fee_rate: DEFAULT_FEE_RATE,
                    commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    to_self_delay: LockTime::new(DEFAULT_TO_SELF_DELAY_BLOCKS),
                    channel_flags: 0,
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    funding_pubkey: channel
                        .get_holder_channel_parameters()
                        .pubkeys
                        .funding_pubkey,
                    revocation_basepoint: channel
                        .get_holder_channel_parameters()
                        .pubkeys
                        .revocation_base_key,
                    payment_basepoint: channel
                        .get_holder_channel_parameters()
                        .pubkeys
                        .payment_base_key,
                    delayed_payment_basepoint: channel
                        .get_holder_channel_parameters()
                        .pubkeys
                        .delayed_payment_base_key,
                    tlc_basepoint: channel.get_holder_channel_parameters().pubkeys.tlc_base_key,
                    next_local_nonce: channel.get_holder_musig2_pubnonce(),
                });

                debug!(
                    "Created OpenChannel message to {:?}: {:?}",
                    &peer_id, &message
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                            peer_id,
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
                    &channel.get_id(),
                    &channel
                );

                // There is a slim chance that this message is not immediately processed by
                // the network actor, while the peer already receive the message AcceptChannel and
                // starts sending following messages. This is a problem of transactionally updating
                // states across multiple actors (NetworkActor and ChannelActor).
                // See also the notes [state updates across multiple actors](docs/notes/state-update-across-multiple-actors.md).
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelCreated(
                            channel.get_id(),
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
            ChannelActorMessage::Event(e) => match e {
                ChannelEvent::FundingTransactionConfirmed() => {
                    let flags = match state.state {
                        ChannelState::AwaitingChannelReady(flags) => flags,
                        ChannelState::AwaitingTxSignatures(f)
                            if f.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) =>
                        {
                            AwaitingChannelReadyFlags::empty()
                        }
                        _ => {
                            panic!("Expecting funding transaction confirmed event in state AwaitingChannelReady or after TX_SIGNATURES_SENT, but got state {:?}", &state.state);
                        }
                    };
                    self.network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                                peer_id: state.peer_id.clone(),
                                message: PCNMessage::ChannelReady(ChannelReady {
                                    channel_id: state.get_id(),
                                }),
                            }),
                        ))
                        .expect("network actor alive");
                    let flags = flags | AwaitingChannelReadyFlags::OUR_CHANNEL_READY;
                    state.state = ChannelState::AwaitingChannelReady(flags);
                    if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                        state.state = ChannelState::ChannelReady(ChannelReadyFlags::empty());
                        self.network
                            .send_message(NetworkActorMessage::new_event(
                                NetworkActorEvent::ChannelReady(
                                    state.get_id(),
                                    self.peer_id.clone(),
                                ),
                            ))
                            .expect("network actor alive");
                    }
                }
            },
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

    pub to_self_amount: u128,
    pub to_remote_amount: u128,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    // Cached channel parameter for easier of access.
    pub holder_channel_parameters: ChannelParametersOneParty,

    // Commitment numbers that are used to derive keys.
    pub holder_commitment_number: u64,
    pub counterparty_commitment_number: u64,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).
    pub id: Option<Hash256>,

    // The id of our next offering tlc, must increment by 1 for each new offered tlc.
    pub next_offering_tlc_id: u64,
    // The id of our next receiving tlc, must increment by 1 for each new offered tlc.
    pub next_receiving_tlc_id: u64,
    // HashMap of tlc ids to pending offered tlcs. Resovled tlcs (both failed and succeeded)
    // will be removed from this map.
    pub pending_offered_tlcs: HashMap<u64, TLC>,
    // HashMap of tlc ids to pending offered tlcs. Resovled tlcs (both failed and succeeded)
    // will be removed from this map.
    pub pending_received_tlcs: HashMap<u64, TLC>,

    pub counterparty_nonce: Option<PubNonce>,
    // The commitment point used in the first commitment transaction after funding transaction.
    // We should use this commitment point if the funding channel is not created yet.
    // Otherwise, use counterparty_commitment_point instead.
    // We will ensure that counterparty_initial_commitment_point is always true after creating
    // the first commitment transaction after funding transaction.
    pub counterparty_initial_commitment_point: Option<Pubkey>,
    // The commitment point that is going to be used in the following commitment transaction.
    pub counterparty_commitment_point: Option<Pubkey>,
    pub counterparty_channel_parameters: Option<ChannelParametersOneParty>,
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ClosedChannel {}

#[derive(Debug)]
pub enum ChannelEvent {
    FundingTransactionConfirmed(),
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
    #[error("Repeated processing message: {0}")]
    RepeatedProcessing(String),
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("Unimplemented operation: {0}")]
    Unimplemented(String),
    #[error("Failed to send command: {0}")]
    CommanderSendingError(#[from] TrySendError<NetworkActorCommand>),
    #[error("Failed to spawn actor: {0}")]
    SpawnErr(#[from] SpawnErr),
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
    pub struct AwaitingTxSignaturesFlags: u32 {
        const OUR_TX_SIGNATURES_SENT = 1;
        const THEIR_TX_SIGNATURES_SENT = 1 << 1;
        const TX_SIGNATURES_SENT = AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT.bits() | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT.bits();
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
            .chain(holder_revocation)
            .collect::<Vec<_>>()
    } else {
        holder_revocation
            .into_iter()
            .chain(counterparty_revocation)
            .collect()
    };
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

// Constructors for the channel actor state.
impl ChannelActorState {
    pub fn new_inbound_channel<'a>(
        temp_channel_id: Hash256,
        holder_value: u128,
        seed: &[u8],
        peer_id: PeerId,
        counterparty_value: u128,
        counterparty_delay: LockTime,
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
            to_self_amount: holder_value,
            temp_id: temp_channel_id,
            id: Some(channel_id),
            next_offering_tlc_id: 0,
            next_receiving_tlc_id: 0,
            pending_offered_tlcs: Default::default(),
            pending_received_tlcs: Default::default(),
            to_remote_amount: counterparty_value,
            holder_channel_parameters: ChannelParametersOneParty {
                pubkeys: holder_pubkeys,
                selected_contest_delay: counterparty_delay,
            },
            signer,
            counterparty_channel_parameters: Some(ChannelParametersOneParty {
                pubkeys: counterparty_pubkeys,
                selected_contest_delay: counterparty_delay,
            }),
            holder_commitment_number: commitment_number,
            counterparty_commitment_number: HOLDER_INITIAL_COMMITMENT_NUMBER,
            counterparty_nonce: Some(counterparty_nonce),
            counterparty_commitment_point: Some(counterparty_commitment_point),
            counterparty_initial_commitment_point: Some(counterparty_prev_commitment_point),
        }
    }

    pub fn new_outbound_channel(
        seed: &[u8],
        peer_id: PeerId,
        value: u128,
        to_self_delay: LockTime,
    ) -> Self {
        let new_channel_id = new_channel_id_from_seed(seed);
        let signer = InMemorySigner::generate_from_seed(seed);
        let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
        let holder_pubkeys = signer.to_channel_public_keys(commitment_number);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            peer_id,
            funding_tx: None,
            is_acceptor: false,
            to_self_amount: value,
            temp_id: new_channel_id,
            id: None,
            next_offering_tlc_id: 0,
            next_receiving_tlc_id: 0,
            pending_offered_tlcs: Default::default(),
            pending_received_tlcs: Default::default(),
            to_remote_amount: value,
            signer,
            holder_channel_parameters: ChannelParametersOneParty {
                pubkeys: holder_pubkeys,
                selected_contest_delay: to_self_delay,
            },
            counterparty_channel_parameters: None,
            holder_commitment_number: commitment_number,
            counterparty_nonce: None,
            counterparty_commitment_number: COUNTERPARTY_INITIAL_COMMITMENT_NUMBER,
            counterparty_commitment_point: None,
            counterparty_initial_commitment_point: None,
        }
    }
}

// Properties for the channel actor state.
impl ChannelActorState {
    pub fn get_id(&self) -> Hash256 {
        self.id.unwrap_or(self.temp_id)
    }

    pub fn get_holder_nonce(&self) -> impl Borrow<PubNonce> {
        self.get_next_next_secnonce().public_nonce()
    }

    pub fn get_next_next_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_next_commitment_number(true))
    }

    pub fn get_next_holder_nonce(&self) -> PubNonce {
        self.signer
            .derive_musig2_nonce(self.get_next_commitment_number(true))
            .public_nonce()
    }

    pub fn get_counterparty_nonce(&self) -> &PubNonce {
        self.counterparty_nonce.as_ref().unwrap()
    }

    pub fn get_holder_channel_parameters(&self) -> &ChannelParametersOneParty {
        &self.holder_channel_parameters
    }

    pub fn get_counterparty_channel_parameters(&self) -> &ChannelParametersOneParty {
        self.counterparty_channel_parameters.as_ref().unwrap()
    }

    pub fn get_next_commitment_number(&self, local: bool) -> u64 {
        if local {
            self.holder_commitment_number + 1
        } else {
            self.counterparty_commitment_number - 1
        }
    }

    pub fn is_funded(&self) -> bool {
        match self.state {
            ChannelState::ChannelReady(_) => {
                assert!(self.funding_tx.is_some());
                assert!(self.id.is_some());
                assert!(self.counterparty_commitment_point.is_some());
                assert!(self.counterparty_initial_commitment_point.is_some());
                assert!(self.counterparty_channel_parameters.is_some());
                true
            }
            _ => false,
        }
    }

    pub fn get_holder_commitment_point(&self) -> Pubkey {
        self.signer
            .get_commitment_point(self.holder_commitment_number)
    }

    /// The first commitment point for the first commitment transaction after funding transaction
    /// is saved in `counterparty_initial_commitment_point`. Afterwards,
    /// `counterparty_initial_commitment_point` should be set to None and
    /// `counterparty_commitment_point` is used.
    pub fn get_counterparty_commitment_point(&self) -> &Pubkey {
        self.counterparty_initial_commitment_point
            .as_ref()
            .or(self.counterparty_commitment_point.as_ref())
            .expect("Counterparty commitment point is present")
    }

    pub fn get_funding_transaction(&self) -> OutPoint {
        // TODO: we should use the actual funding transaction and outpoint here.
        // We don't have a valid funding transaction yet, so we just use a bogus one.

        OutPoint::default()

        // self.funding_tx
        //     .as_ref()
        //     .expect("Funding transaction is present")
        //     .output_pts()
        //     .first()
        //     .expect("Funding transaction output is present")
        //     .clone()
    }

    pub fn get_musig2_agg_context(&self) -> KeyAggContext {
        let holder_pubkey = self.get_holder_channel_parameters().pubkeys.funding_pubkey;
        let counterparty_pubkey = self
            .get_counterparty_channel_parameters()
            .pubkeys
            .funding_pubkey;
        let keys = if self.should_holders_pubkey_go_first_in_musig2() {
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
        let holder_nonce = self.get_holder_nonce();
        let holder_nonce = holder_nonce.borrow();
        let counterparty_nonce = self.get_counterparty_nonce();
        let nonces = if self.should_holders_pubkey_go_first_in_musig2() {
            vec![holder_nonce, counterparty_nonce]
        } else {
            vec![counterparty_nonce, holder_nonce]
        };
        AggNonce::sum(nonces)
    }

    pub fn get_amounts_for_both_party(&self, local: bool) -> (u128, u128) {
        // TODO: consider transaction fee here.
        // TODO: exclude all the timelocked values here.
        if local {
            (
                self.to_remote_amount,
                self.to_self_amount - self.to_remote_amount,
            )
        } else {
            (
                self.to_self_amount - self.to_remote_amount,
                self.to_remote_amount,
            )
        }
    }

    pub fn get_tlcs_for_commitment_tx(&self) -> Vec<TLCOutputInCommitment> {
        // TODO: get tlc outputs for commitment transaction
        vec![]
    }

    pub fn get_counterparty_funding_pubkey(&self) -> &Pubkey {
        &self
            .get_counterparty_channel_parameters()
            .pubkeys
            .funding_pubkey
    }

    pub fn check_state_for_tlc_update(&self) -> ProcessingChannelResult {
        match self.state {
            ChannelState::ChannelReady(_) => Ok(()),
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid state {:?} for adding tlc",
                    self.state
                )))
            }
        }
    }
}

impl From<&ChannelActorState> for Musig2Context {
    fn from(value: &ChannelActorState) -> Self {
        Musig2Context {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            holder_seckey: value.signer.funding_key,
            holder_secnonce: value.get_holder_musig2_secnonce(),
            counterparty_pubkey: *value.get_counterparty_funding_pubkey(),
            counterparty_pubnonce: value.get_counterparty_nonce().clone(),
        }
    }
}

impl From<&ChannelActorState> for Musig2SignContext {
    fn from(value: &ChannelActorState) -> Self {
        Musig2SignContext {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            seckey: value.signer.funding_key,
            secnonce: value.get_holder_musig2_secnonce(),
        }
    }
}

impl From<&ChannelActorState> for Musig2VerifyContext {
    fn from(value: &ChannelActorState) -> Self {
        Musig2VerifyContext {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            pubkey: *value.get_counterparty_funding_pubkey(),
            pubnonce: value.get_counterparty_nonce().clone(),
        }
    }
}

// State transition handlers for the channel actor state.
impl ChannelActorState {
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
                        NetworkActorEvent::ChannelAccepted(self.get_id(), self.temp_id),
                    ))
                    .expect("network actor alive");
                Ok(())
            }
            PCNMessage::TxUpdate(tx) => {
                self.handle_tx_collaboration_msg(TxCollaborationMsg::TxUpdate(tx))
            }
            PCNMessage::TxComplete(tx) => {
                self.handle_tx_collaboration_msg(TxCollaborationMsg::TxComplete(tx))
            }
            PCNMessage::CommitmentSigned(commitment_signed) => {
                self.handle_commitment_signed_message(commitment_signed, network.clone())?;
                if let ChannelState::SigningCommitment(flags) = self.state {
                    if !flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT) {
                        // TODO: maybe we should send our commitment_signed message here.
                        debug!("CommitmentSigned message received, but we haven't sent our commitment_signed message yet");
                    }
                }
                {}
                Ok(())
            }
            PCNMessage::TxSignatures(tx_signatures) => {
                // We're the one who send tx_signature first, and we received a tx_signature message.
                // This means that the tx_signature procedure is now completed. Just change state,
                // and exit.
                if self.should_holder_send_tx_signatures_first() {
                    network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionPending(
                                Transaction::default(),
                                self.get_funding_transaction(),
                                self.get_id(),
                            ),
                        ))
                        .expect("network actor alive");

                    self.state =
                        ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::empty());
                    return Ok(());
                };
                self.handle_tx_signatures(network, Some(tx_signatures.witnesses))?;
                Ok(())
            }
            PCNMessage::ChannelReady(channel_ready) => {
                let flags = match self.state {
                    ChannelState::AwaitingTxSignatures(flags) => {
                        if flags.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) {
                            AwaitingChannelReadyFlags::empty()
                        } else {
                            return Err(ProcessingChannelError::InvalidState(format!(
                                "received ChannelReady message, but we're not ready for ChannelReady, state is currently {:?}",
                                self.state
                            )));
                        }
                    }
                    ChannelState::AwaitingChannelReady(flags) => flags,
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "received ChannelReady message, but we're not ready for ChannelReady, state is currently {:?}", self.state
                        )));
                    }
                };
                let flags = flags | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY;
                self.state = ChannelState::AwaitingChannelReady(flags);
                debug!(
                    "ChannelReady: {:?}, current state: {:?}",
                    &channel_ready, &self.state
                );

                if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                    self.state = ChannelState::ChannelReady(ChannelReadyFlags::empty());
                    network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::ChannelReady(self.get_id(), self.peer_id.clone()),
                        ))
                        .expect("network actor alive");
                }

                Ok(())
            }
            PCNMessage::AddTlc(add_tlc) => {
                self.check_state_for_tlc_update()?;

                let tlc = TLC::from(add_tlc);
                let id = tlc.id;

                match self.pending_received_tlcs.get(&id) {
                    Some(current) if current == &tlc => {
                        debug!(
                            "Repeated processing of AddTlcCommand with id {:?}: current tlc {:?}",
                            id, current,
                        )
                    }
                    Some(current) => {
                        return Err(ProcessingChannelError::RepeatedProcessing(format!(
                                    "Repeated processing of AddTlcCommand with id {:?}: current tlc {:?}, tlc to be inserted {:?}",
                                    id,
                                    current,
                                    &tlc
                                )));
                    }
                    None => {
                        debug!("Adding tlc {:?} to channel {:?}", &tlc, &self.get_id());
                    }
                }

                self.pending_received_tlcs.insert(tlc.id, tlc);
                // TODO: here we didn't send any ack message to the peer.
                // The peer may falsely believe that we have already processed this message,
                // while we have crashed. We need a way to make sure that the peer will resend
                // this message, and our processing of this message is idempotent.
                Ok(())
            }
            PCNMessage::RemoveTlc(remove_tlc) => {
                self.check_state_for_tlc_update()?;

                match self.pending_offered_tlcs.get(&remove_tlc.tlc_id) {
                    Some(current) => {
                        debug!(
                            "Removing tlc {:?} from channel {:?}",
                            &current,
                            &self.get_id()
                        );
                        // TODO: need pay the balance to the user.
                        Ok(())
                    }
                    None => Err(ProcessingChannelError::InvalidParameter(format!(
                        "TLC with id {:?} not found in pending_received_tlcs",
                        remove_tlc.tlc_id
                    ))),
                }
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
        if accept_channel.funding_amount != self.to_self_amount {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "funding_amount mismatch (expected {}, got {})",
                self.to_self_amount, accept_channel.funding_amount
            )));
        }

        if self.state != ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT) {
            return Err(ProcessingChannelError::InvalidState(format!(
                "accepting a channel while in state {:?}, expecting NegotiatingFundingFlags::OUR_INIT_SENT",
                self.state
            )));
        }

        self.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT);
        self.counterparty_nonce = Some(accept_channel.next_local_nonce.clone());

        let counterparty_pubkeys = (&accept_channel).into();
        self.counterparty_channel_parameters = Some(ChannelParametersOneParty {
            pubkeys: counterparty_pubkeys,
            selected_contest_delay: accept_channel.to_self_delay,
        });
        self.counterparty_initial_commitment_point =
            Some(accept_channel.first_per_commitment_point);
        self.counterparty_commitment_point = Some(accept_channel.second_per_commitment_point);

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
                return Err(ProcessingChannelError::InvalidState(
                    "Initiator received a tx collaboration message".to_string(),
                ));
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
            TxCollaborationMsg::TxUpdate(msg) => {
                self.update_funding_tx(msg.tx)?;
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
        network: ActorRef<NetworkActorMessage>,
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
        self.counterparty_nonce = Some(commitment_signed.next_local_nonce);
        self.counterparty_initial_commitment_point = None;
        let flags = flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT;
        self.state = ChannelState::SigningCommitment(flags);
        self.maybe_transition_to_tx_signatures(flags, network)?;
        Ok(())
    }

    pub fn maybe_transition_to_tx_signatures(
        &mut self,
        flags: SigningCommitmentFlags,
        network: ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        if flags.contains(SigningCommitmentFlags::COMMITMENT_SIGNED_SENT) {
            debug!("Commitment signed message sent by both sides, tranitioning to AwaitingTxSignatures state");
            self.state = ChannelState::AwaitingTxSignatures(AwaitingTxSignaturesFlags::empty());
            if self.should_holder_send_tx_signatures_first() {
                debug!("It is our turn to send tx_signatures, so we will do it now.");
                self.handle_tx_signatures(network, None)?;
            }
        }
        Ok(())
    }

    // TODO: currently witnesses in the tx_signatures molecule message are a list of bytes.
    // It is unclear how can we compose two partial sets witnesses into a complete
    // set of witnesses.
    pub fn handle_tx_signatures(
        &mut self,
        network: ActorRef<NetworkActorMessage>,
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
                    partial_witnesses.unwrap()
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

        let flags = if partial_witnesses.is_some() {
            flags | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT
        } else {
            flags
        };
        self.state = ChannelState::AwaitingTxSignatures(flags);

        let funding_tx = self
            .funding_tx
            .as_ref()
            .ok_or(ProcessingChannelError::InvalidState(
                "Funding transaction is not present".to_string(),
            ))?;

        let msg = match partial_witnesses {
            Some(ref _partial_witnesses) => {
                // TODO: filling the whole witnesses here.
                let full_witnesses: Vec<ckb_types::packed::Bytes> = vec![];
                let full_witnesses_u8 = full_witnesses
                    .iter()
                    .map(|w| w.as_slice().to_vec())
                    .collect();

                let funding_tx = funding_tx
                    .as_advanced_builder()
                    .set_witnesses(full_witnesses)
                    .build();
                self.funding_tx = Some(funding_tx.clone());
                self.state = ChannelState::AwaitingTxSignatures(
                    flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT,
                );

                // Since we have received a valid tx_signatures message, we're now sure that
                // we can broadcast a valid transaction to the network, i.e. we can wait for
                // the funding transaction to be confirmed.
                network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::FundingTransactionPending(
                            Transaction::default(),
                            self.get_funding_transaction(),
                            self.get_id(),
                        ),
                    ))
                    .expect("network actor alive");

                PCNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: PCNMessage::TxSignatures(TxSignatures {
                        channel_id: self.get_id(),
                        witnesses: full_witnesses_u8,
                        tx_hash: funding_tx.hash().into(),
                    }),
                }
            }
            None => {
                // TODO: creating partial witnesses here.
                let partial_witnesses = vec![];
                self.state = ChannelState::AwaitingTxSignatures(
                    flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT,
                );

                PCNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: PCNMessage::TxSignatures(TxSignatures {
                        channel_id: self.get_id(),
                        witnesses: partial_witnesses,
                        tx_hash: funding_tx.hash().into(),
                    }),
                }
            }
        };
        debug!(
            "Handled tx_signatures, peer: {:?}, previous witnesses: {:?}, messge to send: {:?}",
            &self.peer_id, &partial_witnesses, &msg
        );
        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendPcnMessage(msg),
            ))
            .expect("network actor alive");
        Ok(())
    }

    pub fn update_funding_tx(&mut self, tx: Transaction) -> ProcessingChannelResult {
        // TODO check if the tx is valid
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
            .get_counterparty_channel_parameters()
            .pubkeys
            .revocation_base_key;
        let holder_revocation = &self
            .get_holder_channel_parameters()
            .pubkeys
            .revocation_base_key;
        let channel_id =
            derive_channel_id_from_revocation_keys(holder_revocation, counterparty_revocation);

        self.id = Some(channel_id);
        debug!(
            "Channel Id changed from {:?} to {:?}",
            &self.temp_id,
            &self.id.unwrap()
        );
    }

    fn should_holders_pubkey_go_first_in_musig2(&self) -> bool {
        let holder_pubkey = self.get_holder_channel_parameters().pubkeys.funding_pubkey;
        let counterparty_pubkey = self
            .get_counterparty_channel_parameters()
            .pubkeys
            .funding_pubkey;
        holder_pubkey <= counterparty_pubkey
    }

    fn should_holder_send_tx_signatures_first(&self) -> bool {
        // TODO: 我们会定义出资金额较少的一方必须先发送
        // tx_signatures 消息, 在金额相同的情况下, 按 funding_pubkey
        // 排序较小的先发送, 这样可以避免双方同时等待对方 tx_signatures
        // 消息导致的死锁问题.
        self.should_holders_pubkey_go_first_in_musig2()
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
                self.get_holder_channel_parameters(),
                self.get_counterparty_channel_parameters(),
                self.signer.get_commitment_point(commitment_number),
                *self.get_counterparty_commitment_point(),
            )
        } else {
            (
                self.get_counterparty_channel_parameters(),
                self.get_holder_channel_parameters(),
                *self.get_counterparty_commitment_point(),
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

    pub fn build_and_verify_commitment_tx(
        &self,
        signature: PartialSignature,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let commitment_tx = self.build_commitment_tx(false);
        commitment_tx.verify(signature, self.into())
    }

    pub fn build_and_sign_commitment_tx(
        &self,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let commitment_tx = self.build_commitment_tx(true);
        commitment_tx.sign(self.into())
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
    pub to_broadcaster_value: u128,
    // The countersignatory's balance, may be spent immediately by the countersignatory.
    pub to_countersignatory_value: u128,
    // The list of TLC commitmentments. These outputs are already multisiged to another
    // set of transactions, whose output may be spent by countersignatory with revocation key,
    // the original sender after delay, or the receiver if they has correct preimage,
    pub tlcs: Vec<TLCOutputInCommitment>,
    // A cache of the parties' pubkeys required to construct the transaction.
    pub keys: TxCreationKeys,
}

/// A wrapper on CommitmentTransaction that has a partial signature along with
/// the ckb transaction.
#[derive(Clone, Debug)]
pub struct PartiallySignedCommitmentTransaction {
    inner: CommitmentTransaction,
    tx: TransactionView,
    signature: PartialSignature,
}

pub struct Musig2Context {
    pub key_agg_ctx: KeyAggContext,
    pub agg_nonce: AggNonce,
    pub holder_seckey: Privkey,
    pub holder_secnonce: SecNonce,
    pub counterparty_pubkey: Pubkey,
    pub counterparty_pubnonce: PubNonce,
}

impl Musig2Context {
    pub fn split(self) -> (Musig2SignContext, Musig2VerifyContext) {
        let Musig2Context {
            key_agg_ctx,
            agg_nonce,
            holder_seckey,
            holder_secnonce,
            counterparty_pubkey,
            counterparty_pubnonce,
        } = self;
        (
            Musig2SignContext {
                key_agg_ctx: key_agg_ctx.clone(),
                agg_nonce: agg_nonce.clone(),
                seckey: holder_seckey,
                secnonce: holder_secnonce,
            },
            Musig2VerifyContext {
                key_agg_ctx,
                agg_nonce,
                pubkey: counterparty_pubkey,
                pubnonce: counterparty_pubnonce,
            },
        )
    }

    pub fn create_signature(
        self,
        message: &[u8],
        partial_signature: PartialSignature,
        partial_signature_given_first: bool,
    ) -> Result<CompactSignature, ProcessingChannelError> {
        let (sign_ctx, verify_ctx) = self.split();
        verify_ctx.verify(partial_signature, message)?;
        let partial_signature_2 = sign_ctx.sign(message)?;
        Ok(aggregate_partial_signatures(
            &verify_ctx.key_agg_ctx,
            &verify_ctx.agg_nonce,
            if partial_signature_given_first {
                vec![partial_signature, partial_signature_2]
            } else {
                vec![partial_signature_2, partial_signature]
            },
            message,
        )?)
    }
}

pub struct Musig2VerifyContext {
    pub key_agg_ctx: KeyAggContext,
    pub agg_nonce: AggNonce,
    pub pubkey: Pubkey,
    pub pubnonce: PubNonce,
}

impl Musig2VerifyContext {
    pub fn verify(&self, signature: PartialSignature, message: &[u8]) -> ProcessingChannelResult {
        Ok(verify_partial(
            &self.key_agg_ctx,
            signature,
            &self.agg_nonce,
            self.pubkey,
            &self.pubnonce,
            message,
        )?)
    }
}
pub struct Musig2SignContext {
    key_agg_ctx: KeyAggContext,
    agg_nonce: AggNonce,
    seckey: Privkey,
    secnonce: SecNonce,
}

impl Musig2SignContext {
    pub fn sign(self, message: &[u8]) -> Result<PartialSignature, ProcessingChannelError> {
        Ok(sign_partial(
            &self.key_agg_ctx,
            self.seckey,
            self.secnonce,
            &self.agg_nonce,
            message,
        )?)
    }
}

impl CommitmentTransaction {
    pub fn gen_tx(&self) -> TransactionView {
        // TODO: implement build transaction here.
        Transaction::default().into_view()
    }

    pub fn sign(
        self,
        sign_ctx: Musig2SignContext,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let tx = self.gen_tx();
        let message = tx.hash();

        let signature = sign_ctx.sign(message.as_slice())?;
        debug!(
            "Signed commitment tx ({:?}) message {:?} with signature {:?}",
            &tx, &message, &signature,
        );

        Ok(PartiallySignedCommitmentTransaction {
            inner: self,
            tx,
            signature,
        })
    }

    pub fn verify(
        self,
        signature: PartialSignature,
        verify_ctx: Musig2VerifyContext,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let tx = self.gen_tx();
        let message = tx.hash();
        debug!(
            "Verifying partial signature ({:?}) of commitment tx ({:?}) message {:?}",
            &signature, &tx, &message
        );
        verify_ctx.verify(signature, message.as_slice())?;
        Ok(PartiallySignedCommitmentTransaction {
            inner: self,
            tx,
            signature,
        })
    }

    pub fn create_transaction(
        &self,
        ctx: Musig2Context,
        partial_signature: PartialSignature,
        partial_signature_given_first: bool,
        set_signature: impl Fn(
            TransactionView,
            CompactSignature,
        ) -> Result<Transaction, ProcessingChannelError>,
    ) -> Result<Transaction, ProcessingChannelError> {
        let tx: TransactionView = self.gen_tx();
        let message = tx.hash();
        let signature = ctx.create_signature(
            message.as_slice(),
            partial_signature,
            partial_signature_given_first,
        )?;
        set_signature(tx, signature)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelParametersOneParty {
    pub pubkeys: ChannelBasePublicKeys,
    pub selected_contest_delay: LockTime,
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
            funding_pubkey: value.funding_pubkey,
            revocation_base_key: value.revocation_basepoint,
            payment_base_key: value.payment_basepoint,
            delayed_payment_base_key: value.delayed_payment_basepoint,
            tlc_base_key: value.tlc_basepoint,
        }
    }
}

impl From<&AcceptChannel> for ChannelBasePublicKeys {
    fn from(value: &AcceptChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey,
            revocation_base_key: value.revocation_basepoint,
            payment_base_key: value.payment_basepoint,
            delayed_payment_base_key: value.delayed_payment_basepoint,
            tlc_base_key: value.tlc_basepoint,
        }
    }
}

pub struct CounterpartyChannelTransactionParameters {
    /// Counter-party public keys
    pub pubkeys: ChannelBasePublicKeys,
    /// The contest delay selected by the counterparty, which applies to holder-broadcast transactions
    pub selected_contest_delay: u16,
}

/// A tlc output.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TLC {
    /// The id of a received TLC. Must be empty if this is an offered HTLC.
    /// We will fill in the id when we send this tlc to the counterparty.
    /// Otherwise must be the next sequence number of the counterparty.
    pub id: u64,
    /// Is this HTLC being received by us or offered by us?
    pub is_offered: bool,
    /// The value, in msat, of the HTLC. The value as it appears in the commitment transaction is
    /// this divided by 1000.
    pub amount: u128,
    /// The CLTV lock-time at which this HTLC expires.
    pub lock_time: LockTime,
    /// The hash of the preimage which unlocks this HTLC.
    pub payment_hash: Hash256,
    /// The preimage of the hash to be sent to the counterparty.
    pub payment_preimage: Option<Hash256>,
}

/// A tlc output in a commitment transaction, including both the tlc output
/// and the index in the commitment transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TLCOutputInCommitment {
    pub output: TLC,
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
    *secret
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
    *base_key
}

pub fn derive_payment_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    *base_key
}

pub fn derive_delayed_payment_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    *base_key
}

pub fn derive_tlc_pubkey(base_key: &Pubkey, _commitment_point: &Pubkey) -> Pubkey {
    *base_key
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

    use crate::{
        ckb::{
            network::{AcceptChannelCommand, OpenChannelCommand},
            test_utils::NetworkNode,
            NetworkActorCommand, NetworkActorMessage,
        },
        NetworkServiceEvent,
    };

    use super::{super::types::Privkey, derive_private_key, derive_tlc_pubkey};

    #[test]
    fn test_derive_private_and_public_keys() {
        let privkey = Privkey::from(&[1; 32]);
        let per_commitment_point = Privkey::from(&[2; 32]).pubkey();
        let derived_privkey = derive_private_key(&privkey, &per_commitment_point);
        let derived_pubkey = derive_tlc_pubkey(&privkey.pubkey(), &per_commitment_point);
        assert_eq!(derived_privkey.pubkey(), derived_pubkey);
    }

    #[test]
    fn test_deserialize_transaction() {
        let tx_builder = Transaction::new_builder();
        let tx_builder = tx_builder.witnesses(vec![[0u8].pack()].pack());
        let tx = tx_builder.build();
        let tx_view = tx.into_view();
        dbg!(tx_view);
    }

    #[tokio::test]
    async fn test_open_channel_to_peer() {
        let [node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes(2)
            .await
            .try_into()
            .unwrap();

        let open_channel_command = OpenChannelCommand {
            peer_id: node_b.peer_id.clone(),
            funding_amount: 1000,
        };
        node_a
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::OpenChannel(open_channel_command),
            ))
            .expect("node_a alive");
        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                    assert_eq!(peer_id, &node_a.peer_id);
                    true
                }
                _ => false,
            })
            .await;
    }

    #[tokio::test]
    async fn test_open_and_accept_channel() {
        let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes(2)
            .await
            .try_into()
            .unwrap();

        node_a
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::OpenChannel(OpenChannelCommand {
                    peer_id: node_b.peer_id.clone(),
                    funding_amount: 1000,
                }),
            ))
            .expect("node_a alive");
        let channel_id = node_b
            .expect_to_process_event(|event| match event {
                NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                    assert_eq!(peer_id, &node_a.peer_id);
                    Some(channel_id.clone())
                }
                _ => None,
            })
            .await;

        node_b
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::AcceptChannel(AcceptChannelCommand {
                    temp_channel_id: channel_id.clone(),
                    funding_amount: 1000,
                }),
            ))
            .expect("node_a alive");

        node_a
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                    assert_eq!(peer_id, &node_b.peer_id);
                    true
                }
                _ => false,
            })
            .await;

        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                    assert_eq!(peer_id, &node_a.peer_id);
                    true
                }
                _ => false,
            })
            .await;
    }
}
