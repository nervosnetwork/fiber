use crate::{debug, error, info, warn};
use bitflags::bitflags;

use ckb_hash::{blake2b_256, new_blake2b};
use ckb_sdk::Since;
use ckb_types::{
    core::{FeeRate, TransactionBuilder, TransactionView},
    packed::{Bytes, CellInput, CellOutput, OutPoint, Script, Transaction},
    prelude::{AsTransactionBuilder, IntoTransactionView, Pack, Unpack},
};

use molecule::prelude::{Builder, Entity};
use musig2::{
    aggregate_partial_signatures,
    errors::{SigningError, VerifyError},
    sign_partial, verify_partial, AggNonce, CompactSignature, KeyAggContext, PartialSignature,
    PubNonce, SecNonce,
};
use ractor::{
    async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef, RpcReplyPort, SpawnErr,
};

use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::secio::PeerId;
use thiserror::Error;
use tokio::sync::oneshot;

use std::{
    borrow::Borrow,
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    ckb::{
        config::{DEFAULT_UDT_MINIMAL_CKB_AMOUNT, MIN_OCCUPIED_CAPACITY},
        types::Shutdown,
    },
    ckb_chain::{
        contracts::{get_cell_deps, get_script_by_contract, Contract},
        FundingRequest,
    },
    NetworkServiceEvent,
};

use super::{
    config::{DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT, MIN_UDT_OCCUPIED_CAPACITY},
    hash_algorithm::HashAlgorithm,
    key::blake2b_hash_with_salt,
    network::CFNMessageWithPeerId,
    serde_utils::EntityHex,
    types::{
        AcceptChannel, AddTlc, CFNMessage, ChannelReady, ClosingSigned, CommitmentSigned, Hash256,
        LockTime, OpenChannel, Privkey, Pubkey, ReestablishChannel, RemoveTlc, RemoveTlcReason,
        RevokeAndAck, TxCollaborationMsg, TxComplete, TxUpdate,
    },
    NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
};

// - `version`: 8 bytes, u64 in little-endian
// - `funding_out_point`: 36 bytes, out point of the funding transaction
// - `pubkey`: 32 bytes, x only aggregated public key
// - `signature`: 64 bytes, aggregated signature
const FUNDING_CELL_WITNESS_LEN: usize = 16 + 8 + 36 + 32 + 64;
// Some part of the code liberally gets previous commitment number, which is
// the current commitment number minus 1. We deliberately set initial commitment number to 1,
// so that we can get previous commitment point/number without checking if the channel
// is funded or not.
pub const INITIAL_COMMITMENT_NUMBER: u64 = 0;

const ASSUME_NETWORK_ACTOR_ALIVE: &str = "network actor must be alive";

pub enum ChannelActorMessage {
    /// Command are the messages that are sent to the channel actor to perform some action.
    /// It is normally generated from a user request.
    Command(ChannelCommand),
    /// Some system events associated to a channel, such as the funding transaction confirmed.
    Event(ChannelEvent),
    /// PeerMessage are the messages sent from the peer.
    PeerMessage(CFNMessage),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddTlcResponse {
    pub tlc_id: u64,
}

#[derive(Debug)]
pub enum ChannelCommand {
    TxCollaborationCommand(TxCollaborationCommand),
    // TODO: maybe we should automatically send commitment_signed message after receiving
    // tx_complete event.
    CommitmentSigned(),
    AddTlc(AddTlcCommand, RpcReplyPort<Result<AddTlcResponse, String>>),
    RemoveTlc(RemoveTlcCommand, RpcReplyPort<Result<(), String>>),
    Shutdown(ShutdownCommand, RpcReplyPort<Result<(), String>>),
}

#[derive(Debug)]
pub enum TxCollaborationCommand {
    TxUpdate(TxUpdateCommand),
    TxComplete(),
}

#[derive(Debug)]
pub struct AddTlcCommand {
    pub amount: u128,
    pub preimage: Option<Hash256>,
    pub payment_hash: Option<Hash256>,
    pub expiry: LockTime,
    pub hash_algorithm: HashAlgorithm,
}

#[derive(Debug)]
pub struct RemoveTlcCommand {
    pub id: u64,
    pub reason: RemoveTlcReason,
}

#[derive(Debug)]
pub struct ShutdownCommand {
    pub close_script: Script,
    pub fee_rate: FeeRate,
}

fn get_random_preimage() -> Hash256 {
    let mut preimage = [0u8; 32];
    preimage.copy_from_slice(&rand::random::<[u8; 32]>());
    preimage.into()
}

#[derive(Debug)]
pub struct ChannelCommandWithId {
    pub channel_id: Hash256,
    pub command: ChannelCommand,
}

pub const DEFAULT_FEE_RATE: u64 = 1_000;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 1_000;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u128 = u128::MAX;
pub const DEFAULT_MAX_ACCEPT_TLCS: u64 = u64::MAX;
pub const DEFAULT_MIN_TLC_VALUE: u128 = 0;
pub const DEFAULT_TO_LOCAL_DELAY_BLOCKS: u64 = 10;

#[derive(Debug)]
pub struct TxUpdateCommand {
    pub transaction: Transaction,
    pub funding_source_lock_script: Script,
}

pub struct OpenChannelParameter {
    pub funding_amount: u128,
    pub seed: [u8; 32],
    pub funding_udt_type_script: Option<Script>,
    pub channel_id_sender: oneshot::Sender<Hash256>,
    pub commitment_fee_rate: Option<u64>,
    pub funding_fee_rate: Option<u64>,
}

pub struct AcceptChannelParameter {
    pub funding_amount: u128,
    pub reserved_ckb_amount: u64,
    pub seed: [u8; 32],
    pub open_channel: OpenChannel,
    pub channel_id_sender: Option<oneshot::Sender<Hash256>>,
}

pub enum ChannelInitializationParameter {
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

#[derive(Debug)]
pub struct ChannelActor<S> {
    peer_id: PeerId,
    network: ActorRef<NetworkActorMessage>,
    store: S,
}

impl<S> ChannelActor<S> {
    pub fn new(peer_id: PeerId, network: ActorRef<NetworkActorMessage>, store: S) -> Self {
        Self {
            peer_id,
            network,
            store,
        }
    }

    pub fn handle_peer_message(
        &self,
        state: &mut ChannelActorState,
        message: CFNMessage,
    ) -> Result<(), ProcessingChannelError> {
        match message {
            CFNMessage::OpenChannel(_) => {
                panic!("OpenChannel message should be processed while prestarting")
            }
            CFNMessage::AcceptChannel(accept_channel) => {
                state.handle_accept_channel_message(accept_channel)?;
                let old_id = state.get_id();
                state.fill_in_channel_id();
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelAccepted(
                            state.peer_id.clone(),
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
            CFNMessage::TxUpdate(tx) => {
                state.handle_tx_collaboration_msg(TxCollaborationMsg::TxUpdate(tx), &self.network)
            }
            CFNMessage::TxComplete(tx) => {
                state.handle_tx_collaboration_msg(
                    TxCollaborationMsg::TxComplete(tx),
                    &self.network,
                )?;
                if let ChannelState::CollaboratingFundingTx(flags) = state.state {
                    if flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) {
                        self.handle_commitment_signed_command(state)?;
                    }
                }
                Ok(())
            }
            CFNMessage::CommitmentSigned(commitment_signed) => {
                state.handle_commitment_signed_message(commitment_signed, &self.network)?;
                if let ChannelState::SigningCommitment(flags) = state.state {
                    if !flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT) {
                        // TODO: maybe we should send our commitment_signed message here.
                        debug!("CommitmentSigned message received, but we haven't sent our commitment_signed message yet");
                        // Notify outside observers.
                        self.network
                            .send_message(NetworkActorMessage::new_event(
                                NetworkActorEvent::NetworkServiceEvent(
                                    NetworkServiceEvent::CommitmentSignaturePending(
                                        state.peer_id.clone(),
                                        state.get_id(),
                                        state.get_current_commitment_number(false),
                                    ),
                                ),
                            ))
                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    }
                }
                Ok(())
            }
            CFNMessage::TxSignatures(tx_signatures) => {
                // We're the one who sent tx_signature first, and we received a tx_signature message.
                // This means that the tx_signature procedure is now completed. Just change state,
                // and exit.
                if state.should_local_send_tx_signatures_first() {
                    let new_witnesses: Vec<_> = tx_signatures
                        .witnesses
                        .into_iter()
                        .map(|x| x.pack())
                        .collect();
                    debug!(
                        "Updating funding tx witnesses of {:?} to {:?}",
                        state.get_funding_transaction().calc_tx_hash(),
                        new_witnesses.iter().map(|x| hex::encode(x.as_slice()))
                    );
                    state.funding_tx = Some(
                        state
                            .get_funding_transaction()
                            .as_advanced_builder()
                            .set_witnesses(new_witnesses)
                            .build()
                            .data(),
                    );
                    self.network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionPending(
                                state.get_funding_transaction().clone(),
                                state.get_funding_transaction_outpoint(),
                                state.get_id(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                    state.update_state(ChannelState::AwaitingChannelReady(
                        AwaitingChannelReadyFlags::empty(),
                    ));
                    return Ok(());
                };

                state.handle_tx_signatures(&self.network, Some(tx_signatures.witnesses))?;
                Ok(())
            }
            CFNMessage::RevokeAndAck(revoke_and_ack) => {
                state.handle_revoke_and_ack_message(revoke_and_ack)?;
                Ok(())
            }
            CFNMessage::ChannelReady(channel_ready) => {
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
                debug!(
                    "ChannelReady: {:?}, current state: {:?}",
                    &channel_ready, &state.state
                );

                if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                    state.on_channel_ready(&self.network);
                }

                Ok(())
            }
            CFNMessage::AddTlc(add_tlc) => {
                state.check_state_for_tlc_update()?;

                let tlc = state.create_inbounding_tlc(add_tlc)?;
                state.insert_tlc(tlc)?;

                // TODO: here we didn't send any ack message to the peer.
                // The peer may falsely believe that we have already processed this message,
                // while we have crashed. We need a way to make sure that the peer will resend
                // this message, and our processing of this message is idempotent.
                Ok(())
            }
            CFNMessage::RemoveTlc(remove_tlc) => {
                state.check_state_for_tlc_update()?;

                state
                    .remove_tlc_with_reason(TLCId::Offered(remove_tlc.tlc_id), remove_tlc.reason)?;
                Ok(())
            }
            CFNMessage::Shutdown(shutdown) => {
                let flags = match state.state {
                    ChannelState::ChannelReady() => ShuttingDownFlags::empty(),
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
                state.remote_shutdown_script = Some(shutdown.close_script);
                state.remote_shutdown_fee_rate = Some(shutdown.fee_rate.as_u64());

                let mut flags = flags | ShuttingDownFlags::THEIR_SHUTDOWN_SENT;

                if state.check_valid_to_auto_accept() {
                    let funding_source_lock_script =
                        state.funding_source_lock_script.as_ref().unwrap();
                    self.network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                                peer_id: self.peer_id.clone(),
                                message: CFNMessage::Shutdown(Shutdown {
                                    channel_id: state.get_id(),
                                    close_script: funding_source_lock_script.clone(),
                                    fee_rate: FeeRate::from_u64(0),
                                }),
                            }),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    state.local_shutdown_script = Some(funding_source_lock_script.clone());
                    state.local_shutdown_fee_rate = Some(0);
                    flags |= ShuttingDownFlags::OUR_SHUTDOWN_SENT;
                    debug!("Auto accept shutdown ...");
                }
                state.update_state(ChannelState::ShuttingDown(flags));
                state.maybe_transition_to_shutdown(&self.network)?;

                Ok(())
            }
            CFNMessage::ClosingSigned(closing) => {
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
                state.remote_shutdown_signature = Some(partial_signature);

                state.maybe_transition_to_shutdown(&self.network)?;
                Ok(())
            }
            CFNMessage::ReestablishChannel(reestablish_channel) => {
                state.handle_reestablish_channel_message(reestablish_channel, &self.network)?;
                Ok(())
            }
            CFNMessage::TxAbort(_) | CFNMessage::TxInitRBF(_) | CFNMessage::TxAckRBF(_) => {
                warn!("Received unsupported message: {:?}", &message);
                Ok(())
            }
        }
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
                debug!(
                    "Processing commitment_signed command in from SigningCommitment state {:?}",
                    &state.state
                );
                CommitmentSignedFlags::SigningCommitment(flags)
            }
            ChannelState::ChannelReady() => CommitmentSignedFlags::ChannelReady(),
            ChannelState::ShuttingDown(flags) => {
                if flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) {
                    debug!(
                        "Signing commitment transactions while shutdown is pending, current state {:?}",
                        &state.state
                    );
                    CommitmentSignedFlags::PendingShutdown(flags)
                } else {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Unable to process commitment_signed message in shutdowning state with flags {:?}",
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

        debug!(
            "Building and signing commitment tx for state {:?}",
            &state.state
        );
        let PartiallySignedCommitmentTransaction {
            tx,
            signature,
            witnesses,
            msg: _,
            version,
        } = state.build_and_sign_commitment_tx()?;
        debug!(
            "Built and signed commitment tx #{}: transaction: ({:?}), partial signature: {:?}",
            version, &tx, &signature
        );

        debug!(
            "Sending next local nonce {:?} (previous nonce {:?})",
            state.get_next_local_nonce(),
            state.get_local_nonce().borrow()
        );
        let commitment_signed = CommitmentSigned {
            channel_id: state.get_id(),
            partial_signature: signature,
            next_local_nonce: state.get_next_local_nonce(),
        };
        debug!(
            "Sending built commitment_signed message: {:?}",
            &commitment_signed
        );
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                    peer_id: state.peer_id.clone(),
                    message: CFNMessage::CommitmentSigned(commitment_signed),
                }),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        self.network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::LocalCommitmentSigned(
                    state.peer_id.clone(),
                    state.get_id(),
                    version,
                    tx,
                    witnesses,
                )),
            ))
            .expect("myself alive");

        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT;
                state.update_state(ChannelState::SigningCommitment(flags));
                state.maybe_transition_to_tx_signatures(flags, &self.network)?;
            }
            CommitmentSignedFlags::ChannelReady() => {}
            CommitmentSignedFlags::PendingShutdown(_) => {
                state.maybe_transition_to_shutdown(&self.network)?;
            }
        }
        Ok(())
    }

    pub fn handle_add_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: AddTlcCommand,
    ) -> Result<u64, ProcessingChannelError> {
        debug!("handle add tlc command : {:?}", &command);
        state.check_state_for_tlc_update()?;
        let tlc = state.create_outbounding_tlc(command);
        state.insert_tlc(tlc)?;

        debug!("Inserted tlc into channel state: {:?}", &tlc);
        // TODO: Note that since message sending is async,
        // we can't guarantee anything about the order of message sending
        // and state updating. And any of these may fail while the other succeeds.
        // We may need to handle all these possibilities.
        // To make things worse, we currently don't have a way to ACK all the messages.

        // Send tlc update message to peer.
        let msg = CFNMessageWithPeerId {
            peer_id: self.peer_id.clone(),
            message: CFNMessage::AddTlc(AddTlc {
                channel_id: state.get_id(),
                tlc_id: tlc.id.into(),
                amount: tlc.amount,
                payment_hash: tlc.payment_hash,
                expiry: tlc.lock_time,
                hash_algorithm: tlc.hash_algorithm,
            }),
        };
        debug!("Sending AddTlc message: {:?}", &msg);
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendCFNMessage(msg),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        Ok(tlc.id.into())
    }

    pub fn handle_remove_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: RemoveTlcCommand,
    ) -> ProcessingChannelResult {
        state.check_state_for_tlc_update()?;
        let tlc = state.remove_tlc_with_reason(TLCId::Received(command.id), command.reason)?;
        let msg = CFNMessageWithPeerId {
            peer_id: self.peer_id.clone(),
            message: CFNMessage::RemoveTlc(RemoveTlc {
                channel_id: state.get_id(),
                tlc_id: command.id,
                reason: command.reason,
            }),
        };
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendCFNMessage(msg),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        debug!(
            "Channel ({:?}) balance after removing tlc {:?}: local balance: {}, remote balance: {}",
            state.get_id(),
            tlc,
            state.to_local_amount,
            state.to_remote_amount
        );
        state.maybe_transition_to_shutdown(&self.network)?;

        Ok(())
    }

    pub fn handle_shutdown_command(
        &self,
        state: &mut ChannelActorState,
        command: ShutdownCommand,
    ) -> ProcessingChannelResult {
        debug!("Handling shutdown command: {:?}", &command);
        let flags = match state.state {
            ChannelState::ChannelReady() => {
                debug!("Handling shutdown command in ChannelReady state");
                ShuttingDownFlags::empty()
            }
            ChannelState::ShuttingDown(flags) => flags,
            _ => {
                debug!("Handling shutdown command in state {:?}", &state.state);
                return Err(ProcessingChannelError::InvalidState(
                    "Trying to send shutdown message while in invalid state".to_string(),
                ));
            }
        };

        state.verify_shutdown_fee_rate(command.fee_rate)?;
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: CFNMessage::Shutdown(Shutdown {
                        channel_id: state.get_id(),
                        close_script: command.close_script.clone(),
                        fee_rate: command.fee_rate,
                    }),
                }),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        state.local_shutdown_script = Some(command.close_script.clone());
        state.local_shutdown_fee_rate = Some(command.fee_rate.as_u64());
        let flags = flags | ShuttingDownFlags::OUR_SHUTDOWN_SENT;
        state.update_state(ChannelState::ShuttingDown(flags));
        debug!(
            "Channel state updated to {:?} after processing shutdown command",
            &state.state
        );

        state.maybe_transition_to_shutdown(&self.network)?;
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
                let cfn_msg = CFNMessage::TxUpdate(TxUpdate {
                    channel_id: state.get_id(),
                    tx: tx_update.transaction.clone(),
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId::new(
                            self.peer_id.clone(),
                            cfn_msg,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG,
                ));
                state.funding_tx = Some(tx_update.transaction.clone());
                state.funding_source_lock_script = Some(tx_update.funding_source_lock_script);
                state.maybe_complete_tx_collaboration(tx_update.transaction, &self.network)?;
            }
            TxCollaborationCommand::TxComplete() => {
                state.check_tx_complete_preconditions()?;
                let cfn_msg = CFNMessage::TxComplete(TxComplete {
                    channel_id: state.get_id(),
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId::new(
                            self.peer_id.clone(),
                            cfn_msg,
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
            ChannelCommand::AddTlc(command, reply) => {
                match self.handle_add_tlc_command(state, command) {
                    Ok(tlc_id) => {
                        self.handle_commitment_signed_command(state)?;
                        let _ = reply.send(Ok(AddTlcResponse { tlc_id }));
                        Ok(())
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::RemoveTlc(command, reply) => {
                match self.handle_remove_tlc_command(state, command) {
                    Ok(_) => {
                        self.handle_commitment_signed_command(state)?;
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::Shutdown(command, reply) => {
                match self.handle_shutdown_command(state, command) {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
        }
    }

    pub fn handle_event(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        event: ChannelEvent,
    ) -> Result<(), ProcessingChannelError> {
        match event {
            ChannelEvent::FundingTransactionConfirmed => {
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
                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                            peer_id: state.peer_id.clone(),
                            message: CFNMessage::ChannelReady(ChannelReady {
                                channel_id: state.get_id(),
                            }),
                        }),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                let flags = flags | AwaitingChannelReadyFlags::OUR_CHANNEL_READY;
                state.update_state(ChannelState::AwaitingChannelReady(flags));
                if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                    state.on_channel_ready(&self.network);
                }
            }
            ChannelEvent::PeerDisconnected => {
                myself.stop(Some("PeerDisconnected".to_string()));
            }
        }
        Ok(())
    }

    fn get_funding_and_reserved_amount(
        &self,
        funding_amount: u128,
        udt_type_script: &Option<Script>,
    ) -> Result<(u128, u64), ProcessingChannelError> {
        match udt_type_script {
            Some(_) => Ok((funding_amount, DEFAULT_UDT_MINIMAL_CKB_AMOUNT)),
            _ => {
                let reserved_ckb_amount = DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT;
                if funding_amount < reserved_ckb_amount.into() {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "The value of the channel should be greater than the reserve amount: {}",
                        reserved_ckb_amount
                    )));
                }
                let funding_amount = funding_amount - reserved_ckb_amount as u128;
                Ok((funding_amount, reserved_ckb_amount))
            }
        }
    }
}

#[rasync_trait]
impl<S> Actor for ChannelActor<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
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
            ChannelInitializationParameter::AcceptChannel(AcceptChannelParameter {
                funding_amount: my_funding_amount,
                reserved_ckb_amount: my_reserved_ckb_amount,
                seed,
                open_channel,
                channel_id_sender,
            }) => {
                let peer_id = self.peer_id.clone();
                debug!(
                    "Accepting channel {:?} to peer {:?}",
                    &open_channel, &peer_id
                );

                let counterpart_pubkeys = (&open_channel).into();
                let OpenChannel {
                    channel_id,
                    chain_hash,
                    commitment_fee_rate,
                    funding_fee_rate,
                    funding_udt_type_script,
                    funding_amount,
                    reserved_ckb_amount,
                    to_local_delay,
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

                let mut state = ChannelActorState::new_inbound_channel(
                    *channel_id,
                    my_funding_amount,
                    my_reserved_ckb_amount,
                    *commitment_fee_rate,
                    *funding_fee_rate,
                    funding_udt_type_script.clone(),
                    &seed,
                    peer_id.clone(),
                    *funding_amount,
                    *reserved_ckb_amount,
                    *to_local_delay,
                    counterpart_pubkeys,
                    next_local_nonce.clone(),
                    *first_per_commitment_point,
                    *second_per_commitment_point,
                );

                state.check_reserve_amount(my_reserved_ckb_amount, *reserved_ckb_amount)?;

                let commitment_number = INITIAL_COMMITMENT_NUMBER;

                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: my_funding_amount,
                    reserved_ckb_amount: my_reserved_ckb_amount,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    to_local_delay: *to_local_delay,
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
                        .get_commitment_point(commitment_number + 1),
                    next_local_nonce: state.get_local_musig2_pubnonce(),
                };

                let command = CFNMessageWithPeerId {
                    peer_id,
                    message: CFNMessage::AcceptChannel(accept_channel),
                };
                // TODO: maybe we should not use try_send here.
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendCFNMessage(command),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelCreated(
                            state.get_id(),
                            self.peer_id.clone(),
                            myself,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                state.update_state(ChannelState::NegotiatingFunding(
                    NegotiatingFundingFlags::INIT_SENT,
                ));
                if let Some(sender) = channel_id_sender {
                    sender.send(state.get_id()).expect("Receive not dropped");
                }
                Ok(state)
            }
            ChannelInitializationParameter::OpenChannel(OpenChannelParameter {
                funding_amount,
                seed,
                funding_udt_type_script,
                channel_id_sender,
                commitment_fee_rate,
                funding_fee_rate,
            }) => {
                let peer_id = self.peer_id.clone();
                info!("Trying to open a channel to {:?}", &peer_id);

                let commitment_fee_rate =
                    commitment_fee_rate.unwrap_or(DEFAULT_COMMITMENT_FEE_RATE);
                if commitment_fee_rate < DEFAULT_COMMITMENT_FEE_RATE {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(format!(
                        "The commitment fee rate is too low, expect {} >= {}",
                        commitment_fee_rate, DEFAULT_COMMITMENT_FEE_RATE
                    ))));
                }

                let funding_fee_rate = funding_fee_rate.unwrap_or(DEFAULT_FEE_RATE);
                if funding_fee_rate < DEFAULT_FEE_RATE {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(format!(
                        "The funding fee rate is too low, expect {} >= {}",
                        funding_fee_rate, DEFAULT_FEE_RATE
                    ))));
                }

                let (funding_amount, reserved_ckb_amount) =
                    self.get_funding_and_reserved_amount(funding_amount, &funding_udt_type_script)?;

                let mut channel = ChannelActorState::new_outbound_channel(
                    &seed,
                    self.peer_id.clone(),
                    funding_amount,
                    reserved_ckb_amount,
                    commitment_fee_rate,
                    funding_fee_rate,
                    funding_udt_type_script.clone(),
                    LockTime::new(DEFAULT_TO_LOCAL_DELAY_BLOCKS),
                );

                let commitment_number = INITIAL_COMMITMENT_NUMBER;
                let message = CFNMessage::OpenChannel(OpenChannel {
                    chain_hash: Hash256::default(),
                    channel_id: channel.get_id(),
                    funding_udt_type_script,
                    funding_amount: channel.to_local_amount,
                    reserved_ckb_amount: channel.local_reserved_ckb_amount,
                    funding_fee_rate,
                    commitment_fee_rate,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    to_local_delay: LockTime::new(DEFAULT_TO_LOCAL_DELAY_BLOCKS),
                    channel_flags: 0,
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    funding_pubkey: channel
                        .get_local_channel_parameters()
                        .pubkeys
                        .funding_pubkey,
                    revocation_basepoint: channel
                        .get_local_channel_parameters()
                        .pubkeys
                        .revocation_base_key,
                    payment_basepoint: channel
                        .get_local_channel_parameters()
                        .pubkeys
                        .payment_base_key,
                    delayed_payment_basepoint: channel
                        .get_local_channel_parameters()
                        .pubkeys
                        .delayed_payment_base_key,
                    tlc_basepoint: channel.get_local_channel_parameters().pubkeys.tlc_base_key,
                    next_local_nonce: channel.get_local_musig2_pubnonce(),
                });

                debug!(
                    "Created OpenChannel message to {:?}: {:?}",
                    &peer_id, &message
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                            peer_id,
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
                        // TODO: The channel id here is a temporary channel id,
                        // while the ChannelCreated event emitted by the counterpart
                        // is a real channel id. This may cause confusion.
                        NetworkActorEvent::ChannelCreated(
                            channel.get_id(),
                            self.peer_id.clone(),
                            myself,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                channel_id_sender
                    .send(channel.get_id())
                    .expect("Receive not dropped");
                Ok(channel)
            }
            ChannelInitializationParameter::ReestablishChannel(channel_id) => {
                let mut channel = self
                    .store
                    .get_channel_actor_state(&channel_id)
                    .expect("channel should exist");
                channel.reestablishing = true;

                let reestablish_channel = ReestablishChannel {
                    channel_id,
                    local_commitment_number: channel.get_current_commitment_number(true),
                    remote_commitment_number: channel.get_current_commitment_number(false),
                };

                let command = CFNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: CFNMessage::ReestablishChannel(reestablish_channel),
                };

                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendCFNMessage(command),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelCreated(
                            channel.get_id(),
                            self.peer_id.clone(),
                            myself,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                Ok(channel)
            }
        }
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ChannelActorMessage::PeerMessage(message) => {
                if state.reestablishing {
                    match message {
                        CFNMessage::ReestablishChannel(reestablish_channel) => {
                            state.handle_reestablish_channel_message(
                                reestablish_channel,
                                &self.network,
                            )?;
                        }
                        _ => {
                            debug!("Ignoring message while reestablishing: {:?}", message);
                        }
                    }
                } else if let Err(error) = self.handle_peer_message(state, message) {
                    error!("Error while processing channel message: {:?}", error);
                }
            }
            ChannelActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(state, command) {
                    error!("Error while processing channel command: {:?}", err);
                }
            }
            ChannelActorMessage::Event(e) => {
                if let Err(err) = self.handle_event(&myself, state, e) {
                    error!("Error while processing channel event: {:?}", err);
                }
            }
        }
        match state.state {
            ChannelState::Closed => {
                myself.stop(Some("ChannelClosed".to_string()));
                self.store.delete_channel_actor_state(&state.get_id());
            }
            _ => {
                self.store.insert_channel_actor_state(state.clone());
            }
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentNumbers {
    pub local: u64,
    pub remote: u64,
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

    pub fn flip(&self) -> Self {
        Self {
            local: self.remote,
            remote: self.local,
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct TLCIds {
    pub offering: u64,
    pub received: u64,
}

impl Default for TLCIds {
    fn default() -> Self {
        Self::new()
    }
}

impl TLCIds {
    pub fn new() -> Self {
        Self {
            offering: 0,
            received: 0,
        }
    }

    pub fn get_next_offering(&self) -> u64 {
        self.offering
    }

    pub fn get_next_received(&self) -> u64 {
        self.received
    }

    pub fn increment_offering(&mut self) {
        self.offering += 1;
    }

    pub fn increment_received(&mut self) {
        self.received += 1;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub enum TLCId {
    Offered(u64),
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

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelActorState {
    pub state: ChannelState,
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub id: Hash256,
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_tx: Option<Transaction>,

    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,

    #[serde_as(as = "Option<EntityHex>")]
    pub funding_source_lock_script: Option<Script>,

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
    // The side who starting a shutdown command need to pay at least this fee for building shutdown transaction,
    // the other side may choose to pay less fee rate if the current fee is enough.
    pub commitment_fee_rate: u64,

    // The fee rate used for funding transaction, the initiator may set it as `funding_fee_rate` option,
    // if it's not set, DEFAULT_FEE_RATE will be used as default value, two sides will use the same fee rate
    pub funding_fee_rate: u64,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    // Cached channel parameter for easier of access.
    pub local_channel_parameters: ChannelParametersOneParty,
    // The holder has set a shutdown script.
    #[serde_as(as = "Option<EntityHex>")]
    pub local_shutdown_script: Option<Script>,

    // Commitment numbers that are used to derive keys.
    // This value is guaranteed to be 0 when channel is just created.
    pub commitment_numbers: CommitmentNumbers,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).

    // The id of next offering/received tlc, must increment by 1 for each new tlc.
    pub tlc_ids: TLCIds,

    // BtreeMap of tlc ids to pending tlcs.
    // serde_as is required for serde to json, as json requires keys to be strings.
    // See https://stackoverflow.com/questions/51276896/how-do-i-use-serde-to-serialize-a-hashmap-with-structs-as-keys-to-json
    #[serde_as(as = "Vec<(_, _)>")]
    pub tlcs: BTreeMap<TLCId, DetailedTLCInfo>,

    // The counterparty has already sent a shutdown message with this script.
    #[serde_as(as = "Option<EntityHex>")]
    pub remote_shutdown_script: Option<Script>,

    pub remote_nonce: Option<PubNonce>,

    // All the commitment point that are sent from the counterparty.
    // We need to save all these points to derive the keys for the commitment transactions.
    pub remote_commitment_points: Vec<Pubkey>,
    pub remote_channel_parameters: Option<ChannelParametersOneParty>,
    pub local_shutdown_signature: Option<PartialSignature>,
    pub local_shutdown_fee_rate: Option<u64>,
    pub remote_shutdown_fee_rate: Option<u64>,
    pub remote_shutdown_signature: Option<PartialSignature>,

    // A flag to indicate whether the channel is reestablishing, we won't process any messages until the channel is reestablished.
    pub reestablishing: bool,

    // A redundant field to record the total amount of the channel.
    // Used only for debugging purposes.
    #[cfg(debug_assertions)]
    pub total_amount: u128,

    pub created_at: SystemTime,
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ClosedChannel {}

#[derive(Debug)]
pub enum ChannelEvent {
    FundingTransactionConfirmed,
    PeerDisconnected,
}

pub type ProcessingChannelResult = Result<(), ProcessingChannelError>;

#[derive(Error, Debug)]
pub enum ProcessingChannelError {
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("Repeated processing message: {0}")]
    RepeatedProcessing(String),
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("Failed to spawn actor: {0}")]
    SpawnErr(#[from] SpawnErr),
    #[error("Musig2 VerifyError: {0}")]
    Musig2VerifyError(#[from] VerifyError),
    #[error("Musig2 SigningError: {0}")]
    Musig2SigningError(#[from] SigningError),
}

bitflags! {
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
        const COLLABRATION_COMPLETED = CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT.bits() | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT.bits();
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
    }
}

// Depending on the state of the channel, we may process the commitment_signed command differently.
// Below are all the channel state flags variants that we may encounter
// in normal commitment_signed processing flow.
#[derive(Debug)]
enum CommitmentSignedFlags {
    SigningCommitment(SigningCommitmentFlags),
    PendingShutdown(ShuttingDownFlags),
    ChannelReady(),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "SCREAMING_SNAKE_CASE",
    tag = "state_name",
    content = "state_flags"
)]
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
    ChannelReady(),
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    /// is about to drop us, but we store this anyway.
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed,
}

pub fn new_channel_id_from_seed(seed: &[u8]) -> Hash256 {
    blake2b_256(seed).into()
}

fn derive_channel_id_from_revocation_keys(
    revocation_basepoint1: &Pubkey,
    revocation_basepoint2: &Pubkey,
) -> Hash256 {
    let local_revocation = revocation_basepoint1.0.serialize();
    let remote_revocation = revocation_basepoint2.0.serialize();
    let mut preimage = [local_revocation, remote_revocation];
    preimage.sort();
    new_channel_id_from_seed(&preimage.concat())
}

fn derive_temp_channel_id_from_revocation_key(revocation_basepoint: &Pubkey) -> Hash256 {
    let revocation = revocation_basepoint.0.serialize();
    let zero_point = [0; 33];
    let preimage = [zero_point, revocation].concat();
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
#[allow(clippy::too_many_arguments)]
impl ChannelActorState {
    pub fn new_inbound_channel(
        temp_channel_id: Hash256,
        local_value: u128,
        local_reserved_ckb_amount: u64,
        commitment_fee_rate: u64,
        funding_fee_rate: u64,
        funding_udt_type_script: Option<Script>,
        seed: &[u8],
        peer_id: PeerId,
        remote_value: u128,
        remote_reserved_ckb_amount: u64,
        remote_delay: LockTime,
        remote_pubkeys: ChannelBasePublicKeys,
        remote_nonce: PubNonce,
        first_commitment_point: Pubkey,
        second_commitment_point: Pubkey,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_base_pubkeys = signer.get_base_public_keys();

        let channel_id = derive_channel_id_from_revocation_keys(
            &local_base_pubkeys.revocation_base_key,
            &remote_pubkeys.revocation_base_key,
        );

        debug!(
            "Generated channel id ({:?}) for temporary channel {:?}",
            &channel_id, &temp_channel_id,
        );

        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
            peer_id,
            funding_tx: None,
            is_acceptor: true,
            funding_udt_type_script,
            to_local_amount: local_value,
            to_remote_amount: remote_value,
            commitment_fee_rate,
            funding_fee_rate,
            id: channel_id,
            tlc_ids: Default::default(),
            tlcs: Default::default(),
            local_shutdown_script: None,
            funding_source_lock_script: None,
            local_channel_parameters: ChannelParametersOneParty {
                pubkeys: local_base_pubkeys,
                selected_contest_delay: remote_delay,
            },
            signer,
            remote_channel_parameters: Some(ChannelParametersOneParty {
                pubkeys: remote_pubkeys,
                selected_contest_delay: remote_delay,
            }),
            commitment_numbers: Default::default(),
            remote_shutdown_script: None,
            remote_nonce: Some(remote_nonce),
            remote_commitment_points: vec![first_commitment_point, second_commitment_point],
            local_shutdown_signature: None,
            local_shutdown_fee_rate: None,
            remote_shutdown_signature: None,
            remote_shutdown_fee_rate: None,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount,

            reestablishing: false,
            #[cfg(debug_assertions)]
            total_amount: local_value + remote_value,
            created_at: SystemTime::now(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_outbound_channel(
        seed: &[u8],
        peer_id: PeerId,
        value: u128,
        local_reserved_ckb_amount: u64,
        commitment_fee_rate: u64,
        funding_fee_rate: u64,
        funding_udt_type_script: Option<Script>,
        to_local_delay: LockTime,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_pubkeys = signer.get_base_public_keys();
        let temp_channel_id =
            derive_temp_channel_id_from_revocation_key(&local_pubkeys.revocation_base_key);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            peer_id,
            funding_tx: None,
            funding_udt_type_script,
            is_acceptor: false,
            to_local_amount: value,
            to_remote_amount: 0,
            commitment_fee_rate,
            funding_fee_rate,
            id: temp_channel_id,
            tlc_ids: Default::default(),
            tlcs: Default::default(),
            signer,
            local_channel_parameters: ChannelParametersOneParty {
                pubkeys: local_pubkeys,
                selected_contest_delay: to_local_delay,
            },
            remote_channel_parameters: None,
            remote_nonce: None,
            commitment_numbers: Default::default(),
            remote_commitment_points: vec![],
            funding_source_lock_script: None,
            local_shutdown_script: None,
            local_shutdown_fee_rate: None,
            remote_shutdown_script: None,
            remote_shutdown_fee_rate: None,
            local_shutdown_signature: None,
            remote_shutdown_signature: None,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount: 0,

            reestablishing: false,
            created_at: SystemTime::now(),
            #[cfg(debug_assertions)]
            total_amount: value,
        }
    }

    /// This function will be only called when the channel is accepted
    fn check_reserve_amount(
        &self,
        local_reserved_amount: u64,
        remote_reserved_amount: u64,
    ) -> Result<(), ProcessingChannelError> {
        let udt_type_script = &self.funding_udt_type_script;
        let reserved_ckb_amount = if udt_type_script.is_some() {
            DEFAULT_UDT_MINIMAL_CKB_AMOUNT
        } else {
            DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT
        };
        if local_reserved_amount < reserved_ckb_amount
            || remote_reserved_amount < reserved_ckb_amount
        {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "The reserve amount should be greater than the minimal reserve amount: {}",
                reserved_ckb_amount
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

    pub fn get_sent_tlc_balance(&self) -> u128 {
        self.get_active_offered_tlcs(true)
            .map(|tlc| tlc.tlc.amount)
            .sum::<u128>()
    }

    pub fn get_received_tlc_balance(&self) -> u128 {
        self.get_active_received_tlcs(false)
            .map(|tlc| tlc.tlc.amount)
            .sum::<u128>()
    }

    pub fn get_created_at_in_microseconds(&self) -> u64 {
        self.created_at
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64
    }

    fn update_state(&mut self, new_state: ChannelState) {
        debug!(
            "Updating channel state from {:?} to {:?}",
            &self.state, &new_state
        );
        self.state = new_state;
    }

    // Send RevokeAndAck message to the counterparty, and update the
    // channel state accordingly.
    fn send_revoke_and_ack_message(&mut self, network: &ActorRef<NetworkActorMessage>) {
        // Now we should revoke previous transation by revealing preimage.
        let old_number = self.get_remote_commitment_number();
        let secret = self.signer.get_commitment_secret(old_number);

        self.update_state_on_raa_msg(false);

        let new_number = self.get_remote_commitment_number();
        let point = self.get_local_commitment_point(new_number);

        debug!(
            "Revealing revocation preimage #{}: {:?}",
            old_number, &secret
        );
        debug!("Sending new commitment point #{}: {:?}", new_number, &point);

        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: CFNMessage::RevokeAndAck(RevokeAndAck {
                        channel_id: self.get_id(),
                        per_commitment_secret: secret.into(),
                        next_per_commitment_point: point,
                    }),
                }),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    // After sending or receiving a RevokeAndAck message, all messages before
    // are considered confirmed by both parties. These messages include
    // AddTlc and RemoveTlc to operate on TLCs.
    // Update state on revoke and ack message received on sent.
    // This may fill in the creation_confirmed_at and removal_confirmed_at fields
    // of the tlcs. And update the to_local_amount and to_remote_amount.
    fn update_state_on_raa_msg(&mut self, is_received: bool) {
        #[cfg(debug_assertions)]
        {
            self.total_amount = self.to_local_amount + self.to_remote_amount;
        }

        if is_received {
            self.increment_local_commitment_number();
        } else {
            self.increment_remote_commitment_number();
        }

        // If this revoke_and_ack message is received from the counterparty,
        // then we should be operating on remote commitment numbers.
        let commitment_numbers = self.get_current_commitment_numbers();

        let (mut to_local_amount, mut to_remote_amount) =
            (self.to_local_amount, self.to_remote_amount);

        debug!("Updating local state on revoke_and_ack message {}, current commitment number: {:?}, to_local_amount: {}, to_remote_amount: {}",
            if is_received { "received" } else { "sent" }, commitment_numbers, to_local_amount, to_remote_amount);

        self.tlcs.values_mut().for_each(|tlc| {
            if tlc.removal_confirmed_at.is_some() {
                return;
            }

            let amount = tlc.tlc.amount;
            // This tlc has not been committed yet.
            if tlc.creation_confirmed_at.is_none() {
                debug!(
                    "Setting local_committed_at for tlc {:?} to commitment number {:?}",
                    tlc.tlc.id, commitment_numbers
                );
                tlc.creation_confirmed_at = Some(commitment_numbers);
            }
            match (tlc.removed_at, tlc.removal_confirmed_at) {
                (Some((_removed_at, reason)), None) => {
                    tlc.removal_confirmed_at = Some(commitment_numbers);
                     match reason {
                        RemoveTlcReason::RemoveTlcFulfill(_)  => {
                            if tlc.is_offered(){
                                to_local_amount -= amount;
                                to_remote_amount += amount;
                            } else {
                                to_local_amount += amount;
                                to_remote_amount -= amount;
                            };
                            debug!(
                                "Updated local amount to {} and remote amount to {} by removing fulfilled tlc {:?} from channel {:?} with reason {:?}",
                                to_local_amount, to_remote_amount, tlc.tlc.id, self.id, reason
                            );
                        },
                        RemoveTlcReason::RemoveTlcFail(_) => {
                            debug!("Removing failed tlc {:?} from channel {:?} with reason {:?}", tlc.tlc.id, self.id, reason);
                        },
                    };
                    debug!(
                        "Setting removal_confirmed_at for tlc {:?} to commitment number {:?}",
                        tlc.tlc.id, commitment_numbers)
                }
                (Some((removed_at, reason)), Some(removal_confirmed_at)) => {
                    debug!(
                        "TLC {:?} is already removed with reason {:?} at commitment number {:?} and is confirmed at {:?}",
                        tlc.tlc.id, reason, removed_at, removal_confirmed_at
                    );
                }
                _ => {
                    debug!("Ignoring processing TLC {:?} as it is not removed yet", tlc.tlc.id);
                }
            }
        });
        self.to_local_amount = to_local_amount;
        self.to_remote_amount = to_remote_amount;
        debug!("Updated local state on revoke_and_ack message {}: current commitment number: {:?}, to_local_amount: {}, to_remote_amount: {}",
        if is_received { "received" } else { "sent" }, commitment_numbers, to_local_amount, to_remote_amount);
        #[cfg(debug_assertions)]
        {
            self.total_amount = self.to_local_amount + self.to_remote_amount;
        }
    }
}

// Properties for the channel actor state.
impl ChannelActorState {
    pub fn get_id(&self) -> Hash256 {
        self.id
    }

    pub fn get_local_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_local_commitment_number())
    }

    pub fn get_local_nonce(&self) -> impl Borrow<PubNonce> {
        self.get_local_secnonce().public_nonce()
    }

    pub fn get_next_local_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_next_commitment_number(true))
    }

    pub fn get_next_local_nonce(&self) -> PubNonce {
        self.get_next_local_secnonce().public_nonce()
    }

    pub fn get_remote_nonce(&self) -> &PubNonce {
        self.remote_nonce.as_ref().unwrap()
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

    fn set_remote_commitment_number(&mut self, number: u64) {
        debug!(
            "Setting remote commitment number from {} to {}",
            self.commitment_numbers.remote, number
        );
        self.commitment_numbers.remote = number;
    }

    pub fn increment_local_commitment_number(&mut self) {
        debug!(
            "Incrementing local commitment number from {} to {}",
            self.get_local_commitment_number(),
            self.get_local_commitment_number() + 1
        );
        self.commitment_numbers.increment_local();
    }

    pub fn increment_remote_commitment_number(&mut self) {
        debug!(
            "Incrementing remote commitment number from {} to {}",
            self.get_remote_commitment_number(),
            self.get_remote_commitment_number() + 1
        );
        self.commitment_numbers.increment_remote();
    }

    pub fn get_current_commitment_number(&self, local: bool) -> u64 {
        if local {
            self.get_local_commitment_number()
        } else {
            self.get_remote_commitment_number()
        }
    }

    pub fn get_next_commitment_number(&self, local: bool) -> u64 {
        self.get_current_commitment_number(local) + 1
    }

    pub fn get_next_offering_tlc_id(&self) -> u64 {
        self.tlc_ids.get_next_offering()
    }

    pub fn get_next_received_tlc_id(&self) -> u64 {
        self.tlc_ids.get_next_received()
    }

    pub fn increment_next_offered_tlc_id(&mut self) {
        self.tlc_ids.increment_offering();
    }

    pub fn increment_next_received_tlc_id(&mut self) {
        self.tlc_ids.increment_received();
    }

    pub fn get_offered_tlc(&self, tlc_id: u64) -> Option<&DetailedTLCInfo> {
        self.tlcs.get(&TLCId::Offered(tlc_id))
    }

    pub fn get_received_tlc(&self, tlc_id: u64) -> Option<&DetailedTLCInfo> {
        self.tlcs.get(&TLCId::Received(tlc_id))
    }

    pub fn insert_tlc(&mut self, tlc: TLC) -> Result<DetailedTLCInfo, ProcessingChannelError> {
        if let Some(current) = self.tlcs.get(&tlc.id) {
            if current.tlc == tlc {
                debug!(
                    "Repeated processing of AddTlcCommand with id {:?}: current tlc {:?}",
                    tlc.id, current,
                );
                return Ok(*current);
            } else {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Trying to insert different tlcs with identical id {:?}: current tlc {:?}, new tlc {:?}",
                        tlc.id, current, tlc
                    )));
            }
        };
        if tlc.amount == 0 {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Expect the amount of tlc with id {:?} is larger than zero",
                tlc.id
            )));
        }
        if tlc.is_offered() {
            // TODO: We should actually also consider all our fulfilled tlcs here.
            // Because this is also the amount that we can actually spend.
            let sent_tlc_value = self.get_sent_tlc_balance();
            debug!("Value of local sent tlcs: {}", sent_tlc_value);
            debug_assert!(self.to_local_amount >= sent_tlc_value);
            // TODO: handle transaction fee here.
            if sent_tlc_value + tlc.amount > self.to_local_amount {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Adding tlc {:?} with amount {} exceeds local balance {}",
                    tlc.id,
                    tlc.amount,
                    self.to_local_amount - sent_tlc_value
                )));
            }
        } else {
            // TODO: We should actually also consider all their fulfilled tlcs here.
            // Because this is also the amount that we can actually spend.
            let received_tlc_value = self.get_received_tlc_balance();
            debug!("Value of remote received tlcs: {}", received_tlc_value);
            debug_assert!(self.to_remote_amount >= received_tlc_value);
            // TODO: handle transaction fee here.
            if received_tlc_value + tlc.amount > self.to_remote_amount {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Adding tlc {:?} with amount {} exceeds remote balance {}",
                    tlc.id,
                    tlc.amount,
                    self.to_remote_amount - received_tlc_value
                )));
            }
        }
        debug!(
            "Adding new tlc {:?} to channel {:?} with local balance {} and remote balance {}",
            &tlc,
            &self.get_id(),
            self.to_local_amount,
            self.to_remote_amount
        );
        let detailed_tlc = DetailedTLCInfo {
            tlc,
            created_at: self.get_current_commitment_numbers(),
            creation_confirmed_at: None,
            removed_at: None,
            removal_confirmed_at: None,
        };
        self.tlcs.insert(tlc.id, detailed_tlc);
        if tlc.is_offered() {
            self.increment_next_offered_tlc_id();
        } else {
            self.increment_next_received_tlc_id();
        }
        Ok(detailed_tlc)
    }

    // Remove a tlc with a reason. If the tlc is removed, then the channel
    // balance will be updated accordingly. Otherwise, it is guaranteed that
    // the channel state is not updated.
    pub fn remove_tlc_with_reason(
        &mut self,
        tlc_id: TLCId,
        reason: RemoveTlcReason,
    ) -> Result<DetailedTLCInfo, ProcessingChannelError> {
        let removed_at = self.get_current_commitment_numbers();

        let tlc = match self.tlcs.get_mut(&tlc_id) {
            None => {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Trying to remove non-existing tlc with id {:?}",
                    tlc_id
                )))
            }
            Some(current) => {
                match current.removed_at {
                    Some((current_removed_at, current_remove_reason))
                        if current_remove_reason == reason && removed_at == current_removed_at =>
                    {
                        debug!(
                            "Skipping removing of tlc {:?} as it is already removed at {:?} with the same reason {:?}", tlc_id, removed_at, reason
                        );
                        return Ok(*current);
                    }
                    Some((current_remove_reason, current_removed_at)) => {
                        return Err(ProcessingChannelError::InvalidParameter(
                            format!("Illegally removing the same tlc: {:?} was previously removed at {:?} for {:?}, and trying to remove it again at {:?} for {:?}",
                                tlc_id,  current_removed_at, reason, removed_at, current_remove_reason)));
                    }
                    None => {
                        debug!(
                            "Inserting remove reason {:?} at commitment number {:?} for tlc {:?}",
                            reason, removed_at, current
                        );
                        if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = reason {
                            let filled_payment_hash: Hash256 = current
                                .tlc
                                .hash_algorithm
                                .hash(fulfill.payment_preimage)
                                .into();
                            if current.tlc.payment_hash != filled_payment_hash {
                                return Err(ProcessingChannelError::InvalidParameter(format!(
                                    "Preimage {:?} is hashed to {}, which does not match payment hash {:?}",
                                    fulfill.payment_preimage, filled_payment_hash, current.tlc.payment_hash,
                                )));
                            }
                        }
                    }
                };
                current.removed_at = Some((removed_at, reason));
                current
            }
        };
        Ok(*tlc)
    }

    pub fn get_local_channel_parameters(&self) -> &ChannelParametersOneParty {
        &self.local_channel_parameters
    }

    pub fn get_remote_channel_parameters(&self) -> &ChannelParametersOneParty {
        self.remote_channel_parameters.as_ref().unwrap()
    }

    pub fn get_funding_transaction(&self) -> &Transaction {
        self.funding_tx
            .as_ref()
            .expect("Funding transaction is present")
    }

    pub fn get_funding_transaction_outpoint(&self) -> OutPoint {
        let tx = self.get_funding_transaction();
        // By convention, the funding tx output for the channel is the first output.
        OutPoint::new(tx.calc_tx_hash(), 0)
    }

    pub fn get_local_shutdown_script(&self) -> &Script {
        // TODO: what is the best strategy for shutdown script here?
        self.local_shutdown_script
            .as_ref()
            .expect("Holder shutdown script is present")
    }

    pub fn get_remote_shutdown_script(&self) -> &Script {
        self.remote_shutdown_script
            .as_ref()
            .expect("Counterparty shutdown script is present")
    }

    fn get_local_commitment_point(&self, commitment_number: u64) -> Pubkey {
        let commitment_point = self.signer.get_commitment_point(commitment_number);
        debug!(
            "Obtained {}th local commitment point: {:?}",
            commitment_number, commitment_point
        );
        commitment_point
    }

    /// Get the counterparty commitment point for the given commitment number.
    fn get_remote_commitment_point(&self, commitment_number: u64) -> Pubkey {
        debug!("Getting remote commitment point #{}", commitment_number);
        let index = commitment_number as usize;
        let commitment_point = self.remote_commitment_points[index];
        debug!(
            "Obtained remote commitment point #{} (counting from 0) out of total {} commitment points: {:?}",
            index,
            self.remote_commitment_points.len(),
            commitment_point
        );
        commitment_point
    }

    pub fn get_funding_lock_script_xonly(&self) -> [u8; 32] {
        let point: musig2::secp::Point = self.get_musig2_agg_context().aggregated_pubkey();
        point.serialize_xonly()
    }

    pub fn get_funding_lock_script(&self) -> Script {
        let args = blake2b_256(self.get_funding_lock_script_xonly());
        debug!(
            "Aggregated pubkey: {:?}, hash: {:?}",
            hex::encode(args),
            hex::encode(&args[..20])
        );
        get_script_by_contract(Contract::FundingLock, args.as_slice())
    }

    pub fn get_funding_request(&self) -> FundingRequest {
        FundingRequest {
            script: self.get_funding_lock_script(),
            udt_type_script: self.funding_udt_type_script.clone(),
            local_amount: self.to_local_amount as u64,
            funding_fee_rate: self.funding_fee_rate,
            remote_amount: self.to_remote_amount as u64,
            local_reserved_ckb_amount: self.local_reserved_ckb_amount,
            remote_reserved_ckb_amount: self.remote_reserved_ckb_amount,
        }
    }

    pub fn get_musig2_agg_pubkey(&self) -> Pubkey {
        self.get_musig2_agg_context().aggregated_pubkey()
    }

    pub fn get_musig2_agg_context(&self) -> KeyAggContext {
        let local_pubkey = self.get_local_channel_parameters().pubkeys.funding_pubkey;
        let remote_pubkey = self.get_remote_channel_parameters().pubkeys.funding_pubkey;
        let keys = self.order_things_for_musig2(local_pubkey, remote_pubkey);
        KeyAggContext::new(keys).expect("Valid pubkeys")
    }

    pub fn get_local_musig2_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_local_commitment_number())
    }

    pub fn get_local_musig2_pubnonce(&self) -> PubNonce {
        self.get_local_musig2_secnonce().public_nonce()
    }

    pub fn get_musig2_agg_pubnonce(&self) -> AggNonce {
        let local_nonce = self.get_local_nonce();
        let local_nonce = local_nonce.borrow();
        let remote_nonce = self.get_remote_nonce();
        let nonces = self.order_things_for_musig2(local_nonce, remote_nonce);
        debug!(
            "Got agg nonces {:?} from peer {:?}: {:?}",
            AggNonce::sum(nonces),
            &self.peer_id,
            nonces
        );
        AggNonce::sum(nonces)
    }

    // The parameter `local_commitment` indicates whether we are building a local or remote
    // commitment. This field is used in some edge cases where we need to know whether we are
    // safe to include a TLC in the commitment transaction.
    // For example, if A sends a AddTlc to B, then A immediately sends a CommitmentSigned to B,
    // this CommitmentSigned message should be the commitment transaction that includes the TLC.
    // Now imagine while A sends CommitmentSigned to B, B also sends a CommitmentSigned message to A,
    // then to verify this CommitmentSigned message, A needs to determine whether to include
    // the TLC in the commitment transaction. Because it is possible that B has not received the
    // AddTlc message from A, so A should not include the TLC in the commitment transaction.
    fn should_tlc_be_included_in_commitment_tx(
        info: &DetailedTLCInfo,
        local_commitment: bool,
    ) -> bool {
        let DetailedTLCInfo {
            tlc,
            creation_confirmed_at,
            removed_at,
            removal_confirmed_at,
            ..
        } = info;
        {
            let am_i_sending_add_tlc_message = {
                if tlc.is_offered() {
                    local_commitment
                } else {
                    !local_commitment
                }
            };
            let am_i_sending_remove_tlc_message = !am_i_sending_add_tlc_message;
            match (removal_confirmed_at, removed_at, creation_confirmed_at) {
                (Some(_), _, _) => {
                    debug!(
                        "Not including TLC {:?} to commitment transction as it is already removed",
                        tlc.id
                    );
                    return false;
                }
                (_, Some(_), Some(_)) => {
                    debug!("Will only include TLC {:?} to commitment transaction if I am sending remove tlc message ({})", tlc.id,am_i_sending_remove_tlc_message);
                    return am_i_sending_remove_tlc_message;
                }
                (_, Some(_), None) => {
                    panic!("TLC {:?} is removed but not confirmed yet", info);
                }
                (_, _, Some(n)) => {
                    debug!("Including TLC {:?} to commitment transaction because tlc confirmed at {:?}", tlc.id, n);
                    return true;
                }
                (None, None, None) => {
                    debug!("Will only include TLC {:?} to commitment transaction if I am sending add tlc message ({})", tlc.id, am_i_sending_add_tlc_message);
                    am_i_sending_add_tlc_message
                }
            }
        }
    }

    pub fn get_active_received_tlcs(
        &self,
        local_commitment: bool,
    ) -> impl Iterator<Item = &DetailedTLCInfo> {
        self.tlcs.values().filter(move |info| {
            Self::should_tlc_be_included_in_commitment_tx(info, local_commitment)
                && !info.is_offered()
        })
    }

    pub fn get_active_offered_tlcs(
        &self,
        local_commitment: bool,
    ) -> impl Iterator<Item = &DetailedTLCInfo> {
        self.tlcs.values().filter(move |info| {
            Self::should_tlc_be_included_in_commitment_tx(info, local_commitment)
                && info.is_offered()
        })
    }

    // The parameter local indicates whether we are interested in the value sent by the local party.
    fn get_tlc_value_received_from_remote(&self, local_commitment: bool) -> u128 {
        if local_commitment {
            self.get_active_received_tlcs(local_commitment)
                .map(|tlc| tlc.tlc.amount)
                .sum::<u128>()
        } else {
            self.get_active_offered_tlcs(local_commitment)
                .map(|tlc| tlc.tlc.amount)
                .sum::<u128>()
        }
    }

    // Get the pubkeys for the tlc. Tlc pubkeys are the pubkeys held by each party
    // while this tlc was created (pubkeys are derived from the commitment number
    // when this tlc was created). The pubkeys returned here are sorted.
    // The offerer who offered this tlc will have the first pubkey, and the receiver
    // will have the second pubkey.
    // This tlc must have valid local_committed_at and remote_committed_at fields.
    pub fn get_tlc_pubkeys(&self, tlc: &DetailedTLCInfo, local: bool) -> (Pubkey, Pubkey) {
        debug!("Getting tlc pubkeys for tlc: {:?}", tlc);
        let is_offered = tlc.tlc.is_offered();
        let CommitmentNumbers {
            local: local_commitment_number,
            remote: remote_commitment_number,
        } = tlc.get_commitment_numbers(local);
        debug!(
            "Local commitment number: {}, remote commitment number: {}",
            local_commitment_number, remote_commitment_number
        );
        let local_pubkey = derive_tlc_pubkey(
            &self.get_local_channel_parameters().pubkeys.tlc_base_key,
            &self.get_local_commitment_point(remote_commitment_number),
        );
        let remote_pubkey = derive_tlc_pubkey(
            &self.get_remote_channel_parameters().pubkeys.tlc_base_key,
            &self.get_remote_commitment_point(local_commitment_number),
        );

        if is_offered {
            (local_pubkey, remote_pubkey)
        } else {
            (remote_pubkey, local_pubkey)
        }
    }

    pub fn get_active_received_tlc_with_pubkeys(
        &self,
        local: bool,
    ) -> impl Iterator<Item = (&DetailedTLCInfo, Pubkey, Pubkey)> {
        self.get_active_received_tlcs(local).map(move |tlc| {
            let (k1, k2) = self.get_tlc_pubkeys(tlc, local);
            (tlc, k1, k2)
        })
    }

    pub fn get_active_offered_tlc_with_pubkeys(
        &self,
        local: bool,
    ) -> impl Iterator<Item = (&DetailedTLCInfo, Pubkey, Pubkey)> {
        self.get_active_offered_tlcs(local).map(move |tlc| {
            let (k1, k2) = self.get_tlc_pubkeys(tlc, local);
            (tlc, k1, k2)
        })
    }

    pub fn get_witness_args_for_active_tlcs(&self, local: bool) -> Vec<u8> {
        // Build a sorted array of TLC so that both party can generate the same commitment transaction.
        debug!("All tlcs: {:?}", self.tlcs);
        let tlcs = {
            let (mut received_tlcs, mut offered_tlcs) = (
                self.get_active_received_tlc_with_pubkeys(local)
                    .map(|(tlc, local, remote)| (*tlc, local, remote))
                    .collect::<Vec<_>>(),
                self.get_active_offered_tlc_with_pubkeys(local)
                    .map(|(tlc, local, remote)| (*tlc, local, remote))
                    .collect::<Vec<_>>(),
            );
            debug!("Received tlcs: {:?}", &received_tlcs);
            debug!("Offered tlcs: {:?}", &offered_tlcs);
            let (mut a, mut b) = if local {
                (received_tlcs, offered_tlcs)
            } else {
                for (tlc, _, _) in received_tlcs.iter_mut().chain(offered_tlcs.iter_mut()) {
                    // Need to flip these fields for the counterparty.
                    tlc.tlc.flip_mut();
                }
                (offered_tlcs, received_tlcs)
            };
            a.sort_by(|x, y| u64::from(x.0.tlc.id).cmp(&u64::from(y.0.tlc.id)));
            b.sort_by(|x, y| u64::from(x.0.tlc.id).cmp(&u64::from(y.0.tlc.id)));
            [a, b].concat()
        };
        debug!("Sorted tlcs: {:?}", &tlcs);
        tlcs.iter()
            .flat_map(|(tlc, local, remote)| {
                [
                    vec![tlc.tlc.get_htlc_type()],
                    tlc.tlc.amount.to_le_bytes().to_vec(),
                    tlc.tlc.get_hash().to_vec(),
                    local.serialize().to_vec(),
                    remote.serialize().to_vec(),
                    Since::from(tlc.tlc.lock_time)
                        .value()
                        .to_le_bytes()
                        .to_vec(),
                ]
                .concat()
            })
            .collect()
    }

    fn any_tlc_pending(&self) -> bool {
        self.tlcs.values().any(|tlc| {
            tlc.creation_confirmed_at.is_none()
                || tlc.removal_confirmed_at.is_none()
                || tlc.removed_at.is_none()
        })
    }

    pub fn get_remote_funding_pubkey(&self) -> &Pubkey {
        &self.get_remote_channel_parameters().pubkeys.funding_pubkey
    }

    fn verify_shutdown_fee_rate(&self, fee_rate: FeeRate) -> ProcessingChannelResult {
        if fee_rate.as_u64() < self.commitment_fee_rate {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Fee rate {} is less than commitment fee rate {}",
                fee_rate, self.commitment_fee_rate
            )));
        }

        let fee = self.calculate_shutdown_fee(fee_rate);
        debug!("shutdown fee: {:?}", fee);

        let available_max_fee = if self.funding_udt_type_script.is_none() {
            self.to_local_amount as u64 + self.local_reserved_ckb_amount - MIN_OCCUPIED_CAPACITY
        } else {
            self.local_reserved_ckb_amount - MIN_UDT_OCCUPIED_CAPACITY
        };
        debug!(
            "verify_shutdown_fee fee: {} available_max_fee: {}",
            fee, available_max_fee,
        );
        if fee > available_max_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Local balance is not enough to pay the fee, expect fee {} <= available_max_fee {}",
                fee, available_max_fee
            )));
        }
        Ok(())
    }

    fn check_valid_to_auto_accept(&self) -> bool {
        let Some(remote_fee_rate) = self.remote_shutdown_fee_rate else {
            return false;
        };
        if remote_fee_rate < self.commitment_fee_rate {
            return false;
        }
        let fee = self.calculate_shutdown_fee(FeeRate::from_u64(remote_fee_rate));
        let remote_available_max_fee = if self.funding_udt_type_script.is_none() {
            self.to_remote_amount as u64 + self.remote_reserved_ckb_amount - MIN_OCCUPIED_CAPACITY
        } else {
            self.remote_reserved_ckb_amount - MIN_UDT_OCCUPIED_CAPACITY
        };
        return fee <= remote_available_max_fee;
    }

    pub fn check_state_for_tlc_update(&self) -> ProcessingChannelResult {
        match self.state {
            ChannelState::ChannelReady() => Ok(()),
            ChannelState::ShuttingDown(_) => Ok(()),
            _ => Err(ProcessingChannelError::InvalidState(format!(
                "Invalid state {:?} for adding tlc",
                self.state
            ))),
        }
    }

    pub fn create_outbounding_tlc(&self, command: AddTlcCommand) -> TLC {
        // TODO: we are filling the user command with a new id here.
        // The advantage of this is that we don't need to burden the users to
        // provide a next id for each tlc. The disadvantage is that users may
        // inadvertently click the same button twice, and we will process the same
        // twice, the frontend needs to prevent this kind of behaviour.
        // Is this what we want?
        let id = self.get_next_offering_tlc_id();
        assert!(
            self.get_offered_tlc(id).is_none(),
            "Must not have the same id in pending offered tlcs"
        );

        let preimage = command.preimage.unwrap_or(get_random_preimage());
        let payment_hash = command
            .payment_hash
            .unwrap_or_else(|| command.hash_algorithm.hash(preimage).into());

        TLC {
            id: TLCId::Offered(id),
            amount: command.amount,
            payment_hash,
            lock_time: command.expiry,
            payment_preimage: Some(preimage),
            hash_algorithm: command.hash_algorithm,
        }
    }

    pub fn create_inbounding_tlc(&self, message: AddTlc) -> Result<TLC, ProcessingChannelError> {
        if self.get_received_tlc(message.tlc_id).is_some() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Trying to add tlc with existing id {:?}",
                message.tlc_id
            )));
        }
        if message.tlc_id != self.get_next_received_tlc_id() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Trying to add tlc with id {:?} while expecting id {:?}",
                message.tlc_id,
                self.get_next_received_tlc_id()
            )));
        }
        Ok(TLC {
            id: TLCId::Received(message.tlc_id),
            amount: message.amount,
            payment_hash: message.payment_hash,
            lock_time: message.expiry,
            payment_preimage: None,
            hash_algorithm: message.hash_algorithm,
        })
    }
}

impl From<&ChannelActorState> for Musig2SignContext {
    fn from(value: &ChannelActorState) -> Self {
        Musig2SignContext {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            seckey: value.signer.funding_key,
            secnonce: value.get_local_musig2_secnonce(),
        }
    }
}

impl From<&ChannelActorState> for Musig2VerifyContext {
    fn from(value: &ChannelActorState) -> Self {
        Musig2VerifyContext {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            pubkey: *value.get_remote_funding_pubkey(),
            pubnonce: value.get_remote_nonce().clone(),
        }
    }
}

// State transition handlers for the channel actor state.
impl ChannelActorState {
    pub fn create_witness_for_funding_cell(
        &self,
        signature: CompactSignature,
        version: u64,
    ) -> [u8; FUNDING_CELL_WITNESS_LEN] {
        create_witness_for_funding_cell(
            self.get_funding_lock_script_xonly(),
            self.get_funding_transaction_outpoint(),
            signature,
            version,
        )
    }

    pub fn aggregate_partial_signatures_to_consume_funding_cell(
        &self,
        partial_signatures: [PartialSignature; 2],
        version: u64,
        tx: &TransactionView,
    ) -> Result<TransactionView, ProcessingChannelError> {
        let funding_out_point = self.get_funding_transaction_outpoint();
        debug_assert_eq!(
            tx.input_pts_iter().next().as_ref(),
            Some(&funding_out_point),
            "The first input of the tx must be the funding cell outpoint"
        );

        let message = get_funding_cell_message_to_sign(version, funding_out_point, tx);
        debug!(
            "Message to sign to consume funding cell {:?} with version {:?}",
            hex::encode(message.as_slice()),
            version
        );

        let verify_ctx = Musig2VerifyContext::from(self);

        let signature = aggregate_partial_signatures_for_msg(
            message.as_slice(),
            verify_ctx,
            partial_signatures,
        )?;

        let witness = self.create_witness_for_funding_cell(signature, version);
        Ok(tx
            .as_advanced_builder()
            .set_witnesses(vec![witness.pack()])
            .build())
    }

    pub fn sign_tx_to_consume_funding_cell(
        &self,
        tx: &PartiallySignedCommitmentTransaction,
    ) -> Result<TransactionView, ProcessingChannelError> {
        debug!(
            "Signing and verifying commitment tx with message {:?} (version {})",
            hex::encode(tx.msg.as_slice()),
            tx.version
        );
        let sign_ctx = Musig2SignContext::from(self);
        let signature2 = sign_ctx.sign(tx.msg.as_slice())?;

        self.aggregate_partial_signatures_to_consume_funding_cell(
            [tx.signature, signature2],
            tx.version,
            &tx.tx,
        )
    }

    pub fn maybe_transition_to_shutdown(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        // This function will also be called when we resolve all pending tlcs.
        // If we are not in the ShuttingDown state, we should not do anything.
        let flags = match self.state {
            ChannelState::ShuttingDown(flags) => flags,
            _ => {
                return Ok(());
            }
        };

        if !flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) || self.any_tlc_pending() {
            debug!(
                "Will not shutdown the channel because we require all tlcs resolved and both parties sent the Shutdown message, current state: {:?}, pending tlcs: {:?}",
                &self.state,
                &self.tlcs
            );
            return Ok(());
        }

        debug!("All pending tlcs are resolved, transitioning to Shutdown state");
        self.update_state(ChannelState::ShuttingDown(
            flags | ShuttingDownFlags::DROPPING_PENDING,
        ));

        let (shutdown_tx, message) = self.build_shutdown_tx()?;
        let sign_ctx = Musig2SignContext::from(&*self);

        // Create our shutdown signature if we haven't already.
        let local_shutdown_signature = match self.local_shutdown_signature {
            Some(signature) => signature,
            None => {
                let signature = sign_ctx.sign(message.as_slice())?;
                self.local_shutdown_signature = Some(signature);
                debug!(
                    "We have signed shutdown tx ({:?}) message {:?} with signature {:?}",
                    &shutdown_tx, &message, &signature,
                );

                network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                            peer_id: self.peer_id.clone(),
                            message: CFNMessage::ClosingSigned(ClosingSigned {
                                partial_signature: signature,
                                channel_id: self.get_id(),
                            }),
                        }),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                signature
            }
        };

        match self.remote_shutdown_signature {
            Some(remote_shutdown_signature) => {
                self.update_state(ChannelState::Closed);
                let tx = self.aggregate_partial_signatures_to_consume_funding_cell(
                    [local_shutdown_signature, remote_shutdown_signature],
                    u64::MAX,
                    &shutdown_tx,
                )?;

                network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelClosed(self.get_id(), self.peer_id.clone(), tx),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }

            None => {
                debug!("We have sent our shutdown signature, waiting for counterparty's signature");
            }
        }

        Ok(())
    }

    pub fn handle_accept_channel_message(
        &mut self,
        accept_channel: AcceptChannel,
    ) -> ProcessingChannelResult {
        if self.state != ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT) {
            return Err(ProcessingChannelError::InvalidState(format!(
                "accepting a channel while in state {:?}, expecting NegotiatingFundingFlags::OUR_INIT_SENT",
                self.state
            )));
        }

        self.check_reserve_amount(
            accept_channel.reserved_ckb_amount,
            self.local_reserved_ckb_amount,
        )?;

        self.update_state(ChannelState::NegotiatingFunding(
            NegotiatingFundingFlags::INIT_SENT,
        ));

        self.to_remote_amount = accept_channel.funding_amount;
        self.remote_reserved_ckb_amount = accept_channel.reserved_ckb_amount;

        #[cfg(debug_assertions)]
        {
            self.total_amount = self.to_local_amount + self.to_remote_amount;
        }

        self.remote_nonce = Some(accept_channel.next_local_nonce.clone());
        let remote_pubkeys = (&accept_channel).into();
        self.remote_channel_parameters = Some(ChannelParametersOneParty {
            pubkeys: remote_pubkeys,
            selected_contest_delay: accept_channel.to_local_delay,
        });
        self.remote_commitment_points = vec![
            accept_channel.first_per_commitment_point,
            accept_channel.second_per_commitment_point,
        ];

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
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        debug!("Processing tx collaboration message: {:?}", &msg);
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
                debug!("Started negotiating funding tx collaboration");
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
                // TODO check if the tx is valid.
                self.funding_tx = Some(msg.tx.clone());
                if self.is_tx_final(&msg.tx)? {
                    self.maybe_complete_tx_collaboration(msg.tx, network)?;
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
            TxCollaborationMsg::TxComplete(_msg) => {
                self.check_tx_complete_preconditions()?;
                let flags = flags | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT;
                self.update_state(ChannelState::CollaboratingFundingTx(flags));
                if flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) {
                    // Notify outside observers.
                    network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::CommitmentSignaturePending(
                                    self.peer_id.clone(),
                                    self.get_id(),
                                    self.get_current_commitment_number(false),
                                ),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
        }
        Ok(())
    }

    pub fn handle_commitment_signed_message(
        &mut self,
        commitment_signed: CommitmentSigned,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        let flags = match self.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed message in state {:?}, as collaboration is not completed yet.",
                    &self.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                debug!(
                    "Processing commitment_signed message in state {:?}",
                    &self.state
                );
                CommitmentSignedFlags::SigningCommitment(SigningCommitmentFlags::empty())
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
                    "Processing commitment_signed message in state {:?}",
                    &self.state
                );
                CommitmentSignedFlags::SigningCommitment(flags)
            }
            ChannelState::ChannelReady() => {
                debug!("Processing commitment_signed message while channel ready");
                CommitmentSignedFlags::ChannelReady()
            }
            ChannelState::ShuttingDown(flags) => {
                if flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) {
                    debug!(
                        "Signing commitment transactions while shutdown is pending, current state {:?}",
                        &self.state
                    );
                    CommitmentSignedFlags::PendingShutdown(flags)
                } else {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Unable to process commitment_signed message in shutdowning state with flags {:?}",
                        &flags
                    )));
                }
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &self.state
                )));
            }
        };

        let tx = self.verify_and_complete_tx(commitment_signed.partial_signature)?;
        let num = self.get_current_commitment_number(false);

        debug!(
            "Successfully handled commitment signed message: {:?}, tx: {:?}",
            &commitment_signed, &tx
        );

        // Notify outside observers.
        network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(
                    NetworkServiceEvent::RemoteCommitmentSigned(
                        self.peer_id.clone(),
                        self.get_id(),
                        num,
                        tx,
                    ),
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        debug!(
            "Updating peer next remote nonce from {:?} to {:?}",
            self.get_remote_nonce(),
            &commitment_signed.next_local_nonce
        );
        self.remote_nonce = Some(commitment_signed.next_local_nonce);
        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT;
                self.update_state(ChannelState::SigningCommitment(flags));
                self.maybe_transition_to_tx_signatures(flags, network)?;
            }
            CommitmentSignedFlags::ChannelReady() | CommitmentSignedFlags::PendingShutdown(_) => {
                self.send_revoke_and_ack_message(network);
                match flags {
                    CommitmentSignedFlags::ChannelReady() => {}
                    CommitmentSignedFlags::PendingShutdown(_) => {
                        // TODO: Handle error in the below function call.
                        // We've already updated our state, we should never fail here.
                        self.maybe_transition_to_shutdown(network)?;
                    }
                    _ => {
                        unreachable!(
                            "Invalid flags for commitment signed message, should have handled {:?}",
                            flags
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn maybe_transition_to_tx_signatures(
        &mut self,
        flags: SigningCommitmentFlags,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        if flags.contains(SigningCommitmentFlags::COMMITMENT_SIGNED_SENT) {
            debug!("Commitment signed message sent by both sides, tranitioning to AwaitingTxSignatures state");
            self.update_state(ChannelState::AwaitingTxSignatures(
                AwaitingTxSignaturesFlags::empty(),
            ));
            if self.should_local_send_tx_signatures_first() {
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
        network: &ActorRef<NetworkActorMessage>,
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
            flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT
        };
        self.update_state(ChannelState::AwaitingTxSignatures(flags));

        let funding_tx = self
            .funding_tx
            .clone()
            .ok_or(ProcessingChannelError::InvalidState(
                "Funding transaction is not present".to_string(),
            ))?;

        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SignTx(
                    self.peer_id.clone(),
                    self.get_id(),
                    funding_tx,
                    partial_witnesses,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        let flags = flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT;
        self.update_state(ChannelState::AwaitingTxSignatures(flags));

        Ok(())
    }

    pub fn on_channel_ready(&mut self, network: &ActorRef<NetworkActorMessage>) {
        self.update_state(ChannelState::ChannelReady());
        self.increment_local_commitment_number();
        self.increment_remote_commitment_number();
        network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::ChannelReady(self.get_id(), self.peer_id.clone()),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    pub fn append_remote_commitment_point(&mut self, commitment_point: Pubkey) {
        debug!(
            "Setting remote commitment point #{} (counting from 0)): {:?}",
            self.remote_commitment_points.len(),
            commitment_point
        );
        assert_eq!(
            self.remote_commitment_points.len() as u64,
            self.get_local_commitment_number()
        );
        self.remote_commitment_points.push(commitment_point);
    }

    pub fn handle_revoke_and_ack_message(
        &mut self,
        revoke_and_ack: RevokeAndAck,
    ) -> ProcessingChannelResult {
        let RevokeAndAck {
            channel_id: _,
            per_commitment_secret,
            next_per_commitment_point,
        } = revoke_and_ack;
        let commitment_number = self.get_local_commitment_number();
        debug!(
            "Checking commitment secret and point for revocation #{}",
            commitment_number
        );
        let per_commitment_point = self.get_remote_commitment_point(commitment_number);
        if per_commitment_point != Privkey::from(per_commitment_secret).pubkey() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Per commitment secret and per commitment point mismatch #{}: secret {:?}, point: {:?}",
                commitment_number, per_commitment_secret, per_commitment_point
            )));
        }
        self.update_state_on_raa_msg(true);
        self.append_remote_commitment_point(next_per_commitment_point);
        Ok(())
    }

    fn handle_reestablish_channel_message(
        &mut self,
        reestablish_channel: ReestablishChannel,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        debug!(
            "Handling reestablish channel message: {:?}, our commitment_numbers {:?}",
            reestablish_channel, self.commitment_numbers,
        );
        self.reestablishing = false;
        match self.state {
            ChannelState::NegotiatingFunding(_flags) => {
                // TODO: in current implementation, we don't store the channel when we are in NegotiatingFunding state.
                // This is an unreachable state for reestablish channel message. we may need to handle this case in the future.
            }
            ChannelState::ChannelReady() => {
                let expected_local_commitment_number = self.get_local_commitment_number();
                let acutal_local_commitment_number = reestablish_channel.remote_commitment_number;
                if acutal_local_commitment_number == expected_local_commitment_number {
                    // resend AddTlc, RemoveTlc and CommitmentSigned messages if needed
                    let mut need_resend_commitment_signed = false;
                    for info in self.tlcs.values() {
                        if info.is_offered() {
                            if info.created_at.get_local() >= acutal_local_commitment_number
                                && info.creation_confirmed_at.is_none()
                            {
                                // resend AddTlc message
                                network
                                    .send_message(NetworkActorMessage::new_command(
                                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                                            peer_id: self.peer_id.clone(),
                                            message: CFNMessage::AddTlc(AddTlc {
                                                channel_id: self.get_id(),
                                                tlc_id: info.tlc.get_id(),
                                                amount: info.tlc.amount,
                                                payment_hash: info.tlc.payment_hash,
                                                expiry: info.tlc.lock_time,
                                                hash_algorithm: info.tlc.hash_algorithm,
                                            }),
                                        }),
                                    ))
                                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                                need_resend_commitment_signed = true;
                            }
                        } else if let Some((commitment_number, remove_reason)) = info.removed_at {
                            if commitment_number.get_local() >= acutal_local_commitment_number {
                                // resend RemoveTlc message
                                network
                                    .send_message(NetworkActorMessage::new_command(
                                        NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId {
                                            peer_id: self.peer_id.clone(),
                                            message: CFNMessage::RemoveTlc(RemoveTlc {
                                                channel_id: self.get_id(),
                                                tlc_id: info.tlc.get_id(),
                                                reason: remove_reason,
                                            }),
                                        }),
                                    ))
                                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                                need_resend_commitment_signed = true;
                            }
                        }
                    }
                    if need_resend_commitment_signed {
                        debug!("Resend CommitmentSigned message");
                        network
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::ControlCfnChannel(ChannelCommandWithId {
                                    channel_id: self.get_id(),
                                    command: ChannelCommand::CommitmentSigned(),
                                }),
                            ))
                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    }
                } else if acutal_local_commitment_number == expected_local_commitment_number + 1 {
                    // wait for remote to resend the RevokeAndAck message, do nothing here
                } else {
                    // unreachable state, just log an error for potential bugs
                    error!(
                        "Reestablish channel message with invalid local commitment number: expected {}, actual {}",
                        expected_local_commitment_number, acutal_local_commitment_number
                    );
                }

                let expected_remote_commitment_number = self.get_remote_commitment_number();
                let acutal_remote_commitment_number = reestablish_channel.local_commitment_number;
                if expected_remote_commitment_number == acutal_remote_commitment_number {
                    // synced with remote, do nothing
                } else if expected_remote_commitment_number == acutal_remote_commitment_number + 1 {
                    // Resetting our remote commitment number to the actual remote commitment number
                    // and resend the RevokeAndAck message.
                    self.set_remote_commitment_number(acutal_remote_commitment_number);
                    self.send_revoke_and_ack_message(network);
                } else {
                    // unreachable state, just log an error for potential bugs
                    error!(
                        "Reestablish channel message with invalid remote commitment number: expected {}, actual {}",
                        expected_remote_commitment_number, acutal_remote_commitment_number
                    );
                }
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

    pub fn is_tx_final(&self, tx: &Transaction) -> Result<bool, ProcessingChannelError> {
        // TODO: check if the tx is valid
        let tx = tx.clone().into_view();

        let first_output = tx
            .outputs()
            .get(0)
            .ok_or(ProcessingChannelError::InvalidParameter(
                "Funding transaction should have at least one output".to_string(),
            ))?;

        if first_output.lock() != self.get_funding_lock_script() {
            error!("Checking if transaction final failed as tx's first output's script is not funding lock: tx: {:?}, first output lock script: {:?}, funding lock script: {:?}",
                 &tx, first_output.lock(), self.get_funding_lock_script());
            // TODO: return an error here. We panic because we want to move fast.
            panic!("Invalid funding transation")
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
            return Ok(udt_amount == self.to_remote_amount + self.to_local_amount);
        } else {
            let current_capacity: u64 = first_output.capacity().unpack();
            let is_complete = current_capacity
                == (self.to_local_amount
                    + self.to_remote_amount
                    + self.local_reserved_ckb_amount as u128
                    + self.remote_reserved_ckb_amount as u128) as u64;
            Ok(is_complete)
        }
    }

    pub fn maybe_complete_tx_collaboration(
        &mut self,
        tx: Transaction,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        let is_complete = self.is_tx_final(&tx)?;

        debug!(
            "Checking if funding transaction {:?} is complete: {}",
            &tx, is_complete
        );

        if is_complete {
            // We need to send a SendCFNMessage command here (instead of a ControlCfnChannel),
            // to guarantee that the TxComplete message immediately is sent to the network actor.
            // Otherwise, it is possible that when the network actor is processing ControlCfnChannel,
            // it receives another SendCFNMessage command, and that message (e.g. CommitmentSigned)
            // is processed first, thus breaking the order of messages.
            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendCFNMessage(CFNMessageWithPeerId::new(
                        self.peer_id.clone(),
                        CFNMessage::TxComplete(TxComplete {
                            channel_id: self.get_id(),
                        }),
                    )),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            let old_flags = match self.state {
                ChannelState::CollaboratingFundingTx(flags) => flags,
                _ => {
                    panic!(
                        "Must be in CollaboratingFundingTx state while running update_funding_tx"
                    );
                }
            };
            self.update_state(ChannelState::CollaboratingFundingTx(
                old_flags | CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT,
            ));
        }
        Ok(())
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

    pub fn fill_in_channel_id(&mut self) {
        assert!(
            self.remote_channel_parameters.is_some(),
            "Counterparty pubkeys is required to derive actual channel id"
        );
        let remote_revocation = &self
            .get_remote_channel_parameters()
            .pubkeys
            .revocation_base_key;
        let local_revocation = &self
            .get_local_channel_parameters()
            .pubkeys
            .revocation_base_key;
        let channel_id =
            derive_channel_id_from_revocation_keys(local_revocation, remote_revocation);

        debug!("Channel Id changed from {:?} to {:?}", self.id, channel_id,);

        self.id = channel_id;
    }

    // Whose pubkey should go first in musig2?
    // We define a definitive order for the pubkeys in musig2 to makes it easier
    // to aggregate musig2 signatures.
    fn should_local_go_first_in_musig2(&self) -> bool {
        let local_pubkey = self.get_local_channel_parameters().pubkeys.funding_pubkey;
        let remote_pubkey = self.get_remote_channel_parameters().pubkeys.funding_pubkey;
        local_pubkey <= remote_pubkey
    }

    // Order some items (like pubkey and nonce) from holders and counterparty in musig2.
    fn order_things_for_musig2<T>(&self, holder: T, counterparty: T) -> [T; 2] {
        if self.should_local_go_first_in_musig2() {
            [holder, counterparty]
        } else {
            [counterparty, holder]
        }
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

    fn calculate_shutdown_fee(&self, fee_rate: FeeRate) -> u64 {
        let dummy_script = get_script_by_contract(Contract::Secp256k1Lock, &[0u8; 20]);
        let cell_deps = get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script);

        let (outputs, outputs_data) = if let Some(type_script) = &self.funding_udt_type_script {
            let dummy_output = CellOutput::new_builder()
                .lock(dummy_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(0.pack())
                .build();
            let dummy_output_data = self.to_local_amount.to_le_bytes().pack();

            let outputs = [dummy_output.clone(), dummy_output];
            let outputs_data = [dummy_output_data.clone(), dummy_output_data];
            (outputs, outputs_data.to_vec())
        } else {
            let dummy_output = CellOutput::new_builder()
                .capacity(0.pack())
                .lock(dummy_script)
                .build();
            let outputs = [dummy_output.clone(), dummy_output];
            (outputs, vec![Default::default(), Default::default()])
        };

        let mock_shutdown_tx = TransactionBuilder::default()
            .cell_deps(cell_deps)
            .input(
                CellInput::new_builder()
                    .previous_output(self.get_funding_transaction_outpoint())
                    .build(),
            )
            .set_outputs(outputs.to_vec())
            .set_outputs_data(outputs_data.to_vec())
            .set_witnesses(vec![[0; FUNDING_CELL_WITNESS_LEN].pack()])
            .build();
        let tx_size = mock_shutdown_tx.data().serialized_size_in_block() as u64;
        fee_rate.fee(tx_size).as_u64()
    }

    pub fn build_shutdown_tx(&self) -> Result<(TransactionView, [u8; 32]), ProcessingChannelError> {
        // Don't use get_local_shutdown_script and get_remote_shutdown_script here
        // as they will panic if the scripts are not present.
        // This function may be called in a state where these scripts are not present.
        let (
            local_shutdown_script,
            remote_shutdown_script,
            local_shutdown_fee,
            remote_shutdown_fee,
        ) = match (
            self.local_shutdown_script.clone(),
            self.remote_shutdown_script.clone(),
            self.local_shutdown_fee_rate,
            self.remote_shutdown_fee_rate,
        ) {
            (
                Some(local_shutdown_script),
                Some(remote_shutdown_script),
                Some(local_shutdown_fee_rate),
                Some(remote_shutdown_fee_rate),
            ) => (
                local_shutdown_script,
                remote_shutdown_script,
                self.calculate_shutdown_fee(FeeRate::from_u64(local_shutdown_fee_rate)),
                self.calculate_shutdown_fee(FeeRate::from_u64(remote_shutdown_fee_rate)),
            ),
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Shutdown scripts are not present: local_shutdown_script {:?}, remote_shutdown_script {:?}, local_shutdown_fee_rate {:?}, remote_shutdown_fee_rate {:?}",
                    &self.local_shutdown_script, &self.remote_shutdown_script,
                    &self.local_shutdown_fee_rate, &self.remote_shutdown_fee_rate
                )));
            }
        };

        debug!(
            "build_shutdown_tx local_shutdown_fee: local {}, remote {}",
            local_shutdown_fee, remote_shutdown_fee
        );

        let cell_deps = get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script);
        let tx_builder = TransactionBuilder::default().cell_deps(cell_deps).input(
            CellInput::new_builder()
                .previous_output(self.get_funding_transaction_outpoint())
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
            let local_output = CellOutput::new_builder()
                .lock(local_shutdown_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(local_capacity.pack())
                .build();
            let local_output_data = self.to_local_amount.to_le_bytes().pack();

            let remote_capacity: u64 = self.remote_reserved_ckb_amount - remote_shutdown_fee;
            debug!(
                "shutdown_tx remote_capacity: {} - {} = {}",
                self.remote_reserved_ckb_amount, remote_shutdown_fee, remote_capacity
            );
            let remote_output = CellOutput::new_builder()
                .lock(remote_shutdown_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(remote_capacity.pack())
                .build();
            let remote_output_data = self.to_remote_amount.to_le_bytes().pack();

            let outputs = self.order_things_for_musig2(local_output, remote_output);
            let outputs_data = self.order_things_for_musig2(local_output_data, remote_output_data);
            let tx = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(outputs_data.to_vec())
                .set_witnesses(vec![[0; FUNDING_CELL_WITNESS_LEN].pack()]) // we need to add a dummy witness for calculating shutdown tx size
                .build();
            let message = get_funding_cell_message_to_sign(
                u64::MAX,
                self.get_funding_transaction_outpoint(),
                &tx,
            );
            debug!(
                "Building message to sign for shutdown transaction {:?}",
                hex::encode(message.as_slice())
            );
            Ok((tx, message))
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
            let local_output = CellOutput::new_builder()
                .capacity(local_value.pack())
                .lock(local_shutdown_script)
                .build();
            let remote_output = CellOutput::new_builder()
                .capacity(remote_value.pack())
                .lock(remote_shutdown_script)
                .build();
            let outputs = self.order_things_for_musig2(local_output, remote_output);
            let tx = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(vec![Default::default(), Default::default()])
                .set_witnesses(vec![[0; FUNDING_CELL_WITNESS_LEN].pack()]) // we need to add a dummy witness for calculating shutdown tx size
                .build();

            let message = get_funding_cell_message_to_sign(
                u64::MAX,
                self.get_funding_transaction_outpoint(),
                &tx,
            );
            debug!(
                "Building message to sign for shutdown transaction {:?}",
                hex::encode(message.as_slice())
            );
            Ok((tx, message))
        }
    }

    // The parameter `local` here specifies whether we are building the commitment transaction
    // for the local party or the remote party. If `local` is true, then we are building a
    // commitment transaction which can be broadcasted by ourself (with valid partial
    // signature from the other party), else we are building a commitment transaction
    // for the remote party (we build this commitment transaction
    // normally because we want to send a partial signature to remote).
    // The function returns a tuple, the first element is the commitment transaction itself,
    // and the second element is the message to be signed by the each party,
    // so as to consume the funding cell. The last element is the witnesses for the
    // commitment transaction.
    pub fn build_commitment_tx(&self, local: bool) -> (TransactionView, [u8; 32], Vec<u8>) {
        let version = self.get_current_commitment_number(local);
        debug!(
            "Building {} commitment transaction #{} with local commtiment number {} and remote commitment number {}",
            if local { "local" } else { "remote" }, version,
            self.get_local_commitment_number(), self.get_remote_commitment_number()
        );

        let funding_out_point = self.get_funding_transaction_outpoint();
        let cell_deps = get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script);
        let tx_builder = TransactionBuilder::default().cell_deps(cell_deps).input(
            CellInput::new_builder()
                .previous_output(funding_out_point.clone())
                .build(),
        );

        let (outputs, outputs_data, witnesses) =
            self.build_commitment_transaction_parameters(local);
        debug!(
            "Building {} commitment transaction #{}'s outputs: {:?}",
            if local { "local" } else { "remote" },
            version,
            &outputs
        );
        let tx_builder = tx_builder.set_outputs(outputs);
        let tx_builder = tx_builder.set_outputs_data(outputs_data);
        let tx = tx_builder.build();
        let message = get_funding_cell_message_to_sign(version, funding_out_point, &tx);
        debug!(
            "Built {} commitment transaction #{}: transaction: {:?}, signing message: {:?}, witnesses: {:?}",
            if local { "local" } else { "remote" }, version,
            &tx, hex::encode(message.as_slice()), hex::encode(&witnesses)
        );
        (tx, message, witnesses)
    }

    fn build_current_commitment_transaction_witnesses(&self, local: bool) -> Vec<u8> {
        let local_commitment_number = self.get_local_commitment_number();
        let remote_commitment_number = self.get_remote_commitment_number();
        let commitment_number = if local {
            local_commitment_number
        } else {
            remote_commitment_number
        };
        debug!(
            "Building {} commitment transaction #{}'s witnesses (commitment numbers: local {}, remote {})",
            if local { "local" } else { "remote" },
            commitment_number,
            local_commitment_number,
            remote_commitment_number
        );
        let (delayed_epoch, delayed_payment_key, revocation_key) = {
            let (
                delay,
                // delayed_payment and revocation keys.
                // The two fields below are used to derive a pubkey for the delayed payment.
                // The delayed_payment_commitment_point is held by the broadcaster.
                delayed_payment_commitment_point,
                delayed_payment_base_key,
                // The two fields below are used to derive a pubkey for the revocation.
                // Unlike delayed_payment_commitment_point above, the revocation key is held
                // by the counter-signatory until this commitment transaction is revoked.
                revocation_commitment_point,
                revocation_base_key,
            ) = if local {
                (
                    self.get_local_channel_parameters().selected_contest_delay,
                    self.get_local_commitment_point(remote_commitment_number),
                    self.get_local_channel_parameters()
                        .delayed_payment_base_key(),
                    self.get_remote_commitment_point(local_commitment_number),
                    self.get_local_channel_parameters().revocation_base_key(),
                )
            } else {
                (
                    self.get_remote_channel_parameters().selected_contest_delay,
                    self.get_remote_commitment_point(local_commitment_number),
                    self.get_remote_channel_parameters()
                        .delayed_payment_base_key(),
                    self.get_local_commitment_point(remote_commitment_number),
                    self.get_remote_channel_parameters().revocation_base_key(),
                )
            };
            debug!(
                "Got base witness parameters: delayed_time: {:?}, delayed_payment_key: {:?}, delayed_commitment_point {:?}, revocation_key: {:?}, revocation_commitment point: {:?}",
                delay, delayed_payment_base_key,
                delayed_payment_commitment_point,
                revocation_base_key, revocation_commitment_point
            );
            (
                delay,
                derive_delayed_payment_pubkey(
                    delayed_payment_base_key,
                    &delayed_payment_commitment_point,
                ),
                derive_revocation_pubkey(revocation_base_key, &revocation_commitment_point),
            )
        };

        debug!(
            "Parameters for witnesses: epoch {:?}, payment key: {:?}, revocation key: {:?}",
            delayed_epoch, delayed_payment_key, revocation_key
        );

        // for xudt compatibility issue,
        // refer to: https://github.com/nervosnetwork/cfn-scripts/pull/5
        let empty_witness_args: [u8; 16] = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
        [
            empty_witness_args.to_vec(),
            (Since::from(delayed_epoch).value()).to_le_bytes().to_vec(),
            blake2b_256(delayed_payment_key.serialize())[0..20].to_vec(),
            blake2b_256(revocation_key.serialize())[0..20].to_vec(),
            self.get_witness_args_for_active_tlcs(local),
        ]
        .concat()
    }

    // Build the parameters for the commitment transaction. The first two elements for the
    // returning tuple are commitment outputs and commitment outputs data.
    // The last element is the witnesses for the commitment transaction.
    fn build_commitment_transaction_parameters(
        &self,
        local: bool,
    ) -> (Vec<CellOutput>, Vec<Bytes>, Vec<u8>) {
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(
                self.total_amount,
                self.to_local_amount + self.to_remote_amount
            );
        }

        // The time_locked_value is amount of assets locked by commitment-lock.
        // Our value is always time-locked. Additionally, we need to add the value of
        // all the TLCs that we have received from the counterparty.
        let (time_locked_value, immediately_spendable_value) = if local {
            (
                self.to_local_amount + self.local_reserved_ckb_amount as u128,
                self.to_remote_amount + self.remote_reserved_ckb_amount as u128,
            )
        } else {
            (
                self.to_remote_amount + self.remote_reserved_ckb_amount as u128,
                self.to_local_amount + self.local_reserved_ckb_amount as u128,
            )
        };

        // Only the received tlc value is added here because
        // sent tlc value is already included in the time_locked_value.
        let received_tlc_value = self.get_tlc_value_received_from_remote(local);
        debug!(
            "Got {} commitment transaction #{}'s values: time_locked_value: {}, tlc_value: {}, immediately_spendable_value: {}",
            if local { "local" } else { "remote" },
            self.get_current_commitment_number(local),
            time_locked_value, received_tlc_value, immediately_spendable_value);
        let time_locked_value = time_locked_value + received_tlc_value;
        let immediately_spendable_value = immediately_spendable_value - received_tlc_value;

        debug!("Building commitment transaction with time_locked_value: {}, immediately_spendable_value: {}", time_locked_value, immediately_spendable_value);
        let immediate_payment_key = {
            let (commitment_point, base_payment_key) = if local {
                (
                    // Note that we're building a local commitment transaction, so we need to use
                    // the local commitment number.
                    self.get_remote_commitment_point(self.get_local_commitment_number()),
                    self.get_remote_channel_parameters().payment_base_key(),
                )
            } else {
                (
                    // Note that we're building a remote commitment transaction, so we need to use
                    // the remote commitment number.
                    self.get_local_commitment_point(self.get_remote_commitment_number()),
                    self.get_local_channel_parameters().payment_base_key(),
                )
            };
            derive_payment_pubkey(base_payment_key, &commitment_point)
        };

        let witnesses: Vec<u8> = self.build_current_commitment_transaction_witnesses(local);

        let hash = blake2b_256(&witnesses);
        let script_arg: &[u8] = &hash[..20];
        debug!(
            "Building {} commitment transaction with witnesses {:?} and hash {:?} with local commitment number {} and remote commitment number {}",
            if local { "local" } else {"remote"},
            hex::encode(&witnesses),
            hex::encode(script_arg),
            self.get_local_commitment_number(),
            self.get_remote_commitment_number()
        );

        let immediate_secp256k1_lock_script = get_script_by_contract(
            Contract::Secp256k1Lock,
            &blake2b_256(immediate_payment_key.serialize())[0..20],
        );
        let commitment_lock_script = get_script_by_contract(Contract::CommitmentLock, script_arg);

        if let Some(udt_type_script) = &self.funding_udt_type_script {
            //FIXME(yukang): we need to add logic for transaction fee here
            let (time_locked_ckb_amount, immediately_spendable_ckb_amount) = if local {
                (
                    self.local_reserved_ckb_amount,
                    self.remote_reserved_ckb_amount,
                )
            } else {
                (
                    self.remote_reserved_ckb_amount,
                    self.local_reserved_ckb_amount,
                )
            };

            let immediate_output_data = immediately_spendable_value.to_le_bytes().pack();
            let immediate_output = CellOutput::new_builder()
                .lock(immediate_secp256k1_lock_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity(immediately_spendable_ckb_amount.pack())
                .build();

            let commitment_lock_output_data = time_locked_value.to_le_bytes().pack();
            let commitment_lock_output = CellOutput::new_builder()
                .lock(commitment_lock_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity(time_locked_ckb_amount.pack())
                .build();

            let outputs = vec![immediate_output, commitment_lock_output];
            let outputs_data = vec![immediate_output_data, commitment_lock_output_data];
            (outputs, outputs_data, witnesses)
        } else {
            let outputs = vec![
                CellOutput::new_builder()
                    .capacity((immediately_spendable_value as u64).pack())
                    .lock(immediate_secp256k1_lock_script)
                    .build(),
                CellOutput::new_builder()
                    .capacity((time_locked_value as u64).pack())
                    .lock(commitment_lock_script)
                    .build(),
            ];
            let outputs_data = vec![Bytes::default(); outputs.len()];
            (outputs, outputs_data, witnesses)
        }
    }

    pub fn build_and_verify_commitment_tx(
        &self,
        signature: PartialSignature,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let verify_ctx = Musig2VerifyContext::from(self);

        let (tx, msg, witnesses) = self.build_commitment_tx(false);
        debug!(
            "Verifying partial signature ({:?}) of commitment tx ({:?}) message {:?}",
            &signature,
            &tx,
            hex::encode(msg)
        );
        verify_ctx.verify(signature, msg.as_slice())?;
        Ok(PartiallySignedCommitmentTransaction {
            msg,
            version: self.get_current_commitment_number(false),
            tx,
            signature,
            witnesses,
        })
    }

    pub fn build_and_sign_commitment_tx(
        &self,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let sign_ctx = Musig2SignContext::from(self);

        let (tx, msg, witnesses) = self.build_commitment_tx(true);

        debug!(
            "Signing commitment tx with message {:?}",
            hex::encode(msg.as_slice())
        );
        let signature = sign_ctx.sign(msg.as_slice())?;
        debug!(
            "Signed commitment tx ({:?}) message {:?} with signature {:?}",
            &tx,
            hex::encode(msg),
            &signature,
        );

        Ok(PartiallySignedCommitmentTransaction {
            msg,
            tx,
            signature,
            witnesses,
            version: self.get_current_commitment_number(true),
        })
    }

    /// Verify the partial signature from the peer and create a complete transaction
    /// with valid witnesses.
    pub fn verify_and_complete_tx(
        &self,
        signature: PartialSignature,
    ) -> Result<TransactionView, ProcessingChannelError> {
        let tx = self.build_and_verify_commitment_tx(signature)?;
        debug!(
            "Trying to complete tx with partial remote signature {:?}",
            &tx
        );
        self.sign_tx_to_consume_funding_cell(&tx)
    }
}

pub trait ChannelActorStateStore {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState>;
    fn insert_channel_actor_state(&self, state: ChannelActorState);
    fn delete_channel_actor_state(&self, id: &Hash256);
    fn get_channel_ids_by_peer(&self, peer_id: &PeerId) -> Vec<Hash256>;
    fn get_channel_states(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Hash256, ChannelState)>;
}

/// A wrapper on CommitmentTransaction that has a partial signature along with
/// the ckb transaction.
#[derive(Clone, Debug)]
pub struct PartiallySignedCommitmentTransaction {
    // The message that was signed by the partial signature.
    // This partial signature is going to be verified by the funding lock.
    // We have to follow funding lock's rules to generate this message.
    pub msg: [u8; 32],
    // The version number of the commitment transaction.
    pub version: u64,
    // The commitment transaction.
    pub tx: TransactionView,
    // The witnesses in the commitment transaction.
    pub witnesses: Vec<u8>,
    // The partial signature of the commitment transaction.
    pub signature: PartialSignature,
}

pub fn create_witness_for_funding_cell(
    lock_key_xonly: [u8; 32],
    out_point: OutPoint,
    signature: CompactSignature,
    version: u64,
) -> [u8; FUNDING_CELL_WITNESS_LEN] {
    let mut witness = Vec::with_capacity(FUNDING_CELL_WITNESS_LEN);

    // for xudt compatibility issue,
    // refer to: https://github.com/nervosnetwork/cfn-scripts/pull/5
    let empty_witness_args = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    witness.extend_from_slice(&empty_witness_args);
    for bytes in [
        version.to_le_bytes().as_ref(),
        out_point.as_slice(),
        lock_key_xonly.as_slice(),
        signature.serialize().as_slice(),
    ] {
        debug!(
            "Extending witness with {} bytes: {:?}",
            bytes.len(),
            hex::encode(bytes)
        );
        witness.extend_from_slice(bytes);
    }

    debug!(
        "Building witnesses for transaction to consume funding cell: {:?}",
        hex::encode(&witness)
    );

    witness
        .try_into()
        .expect("Witness length should be correct")
}

pub struct Musig2VerifyContext {
    pub key_agg_ctx: KeyAggContext,
    pub agg_nonce: AggNonce,
    pub pubkey: Pubkey,
    pub pubnonce: PubNonce,
}

impl From<Musig2SignContext> for Musig2VerifyContext {
    fn from(value: Musig2SignContext) -> Self {
        Musig2VerifyContext {
            key_agg_ctx: value.key_agg_ctx,
            agg_nonce: value.agg_nonce,
            pubkey: value.seckey.pubkey(),
            pubnonce: value.secnonce.public_nonce(),
        }
    }
}

impl Musig2VerifyContext {
    pub fn verify(&self, signature: PartialSignature, message: &[u8]) -> ProcessingChannelResult {
        let result = verify_partial(
            &self.key_agg_ctx,
            signature,
            &self.agg_nonce,
            self.pubkey,
            &self.pubnonce,
            message,
        );
        debug!(
            "Verifying partial signature {:?} with message {:?}, nonce {:?}, agg nonce {:?}, result {:?}",
            &signature,
            hex::encode(message),
            &self.pubnonce,
            &self.agg_nonce,
            result
        );
        Ok(result?)
    }
}

#[derive(Debug, Clone)]
pub struct Musig2SignContext {
    key_agg_ctx: KeyAggContext,
    agg_nonce: AggNonce,
    seckey: Privkey,
    secnonce: SecNonce,
}

impl Musig2SignContext {
    pub fn sign(self, message: &[u8]) -> Result<PartialSignature, ProcessingChannelError> {
        debug!(
            "Musig2 signing partial message {:?} with nonce {:?} (public nonce: {:?}), agg nonce {:?}",
            hex::encode(message),
            self.secnonce,
            self.secnonce.public_nonce(),
            &self.agg_nonce
        );
        Ok(sign_partial(
            &self.key_agg_ctx,
            self.seckey,
            self.secnonce,
            &self.agg_nonce,
            message,
        )?)
    }
}

fn get_funding_cell_message_to_sign(
    version: u64,
    funding_out_point: OutPoint,
    tx: &TransactionView,
) -> [u8; 32] {
    let version = version.to_le_bytes();
    let version = version.as_ref();
    let funding_out_point = funding_out_point.as_slice();
    let tx_hash = tx.hash();
    let tx_hash = tx_hash.as_slice();
    blake2b_256([version, funding_out_point, tx_hash].concat())
}

pub fn aggregate_partial_signatures_for_msg(
    message: &[u8],
    verify_ctx: Musig2VerifyContext,
    partial_signatures: [PartialSignature; 2],
) -> Result<CompactSignature, ProcessingChannelError> {
    debug!(
        "Message to aggregate signatures: {:?}",
        hex::encode(message)
    );
    let signature: CompactSignature = aggregate_partial_signatures(
        &verify_ctx.key_agg_ctx,
        &verify_ctx.agg_nonce,
        partial_signatures,
        message,
    )?;
    Ok(signature)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

type ShortHash = [u8; 20];

/// A tlc output.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TLC {
    /// The id of a TLC.
    pub id: TLCId,
    /// The value as it appears in the commitment transaction
    pub amount: u128,
    /// The CLTV lock-time at which this HTLC expires.
    pub lock_time: LockTime,
    /// The hash of the preimage which unlocks this HTLC.
    pub payment_hash: Hash256,
    /// The preimage of the hash to be sent to the counterparty.
    pub payment_preimage: Option<Hash256>,
    /// Which hash algorithm is applied on the preimage
    pub hash_algorithm: HashAlgorithm,
}

impl TLC {
    pub fn is_offered(&self) -> bool {
        self.id.is_offered()
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    // Change this tlc to the opposite side.
    pub fn flip_mut(&mut self) {
        self.id.flip_mut()
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

    fn get_hash(&self) -> ShortHash {
        self.payment_hash.as_ref()[..20].try_into().unwrap()
    }

    fn get_id(&self) -> u64 {
        match self.id {
            TLCId::Offered(id) => id,
            TLCId::Received(id) => id,
        }
    }
}

/// A tlc output in a commitment transaction, including both the tlc output
/// and the commitment_number that it first appeared (will appear) in the
/// commitment transaction.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetailedTLCInfo {
    tlc: TLC,
    // The commitment numbers of both parties when this tlc is created
    // as the offerer sees it.
    // TODO: There is a potential bug here. The commitment number of the
    // receiver may have been updated by the time this tlc is included
    // in a commitment of the offerer. Currently we assume that the commitment
    // number of the receiver when the time this tlc is actually committed by
    // the offerer is just the same as the commitment number of the receiver
    // when the this tlc is created.
    created_at: CommitmentNumbers,
    // The commitment number of the party that received this tlc
    // (also called receiver) when this tlc is first included in
    // the commitment transaction of the receiver.
    creation_confirmed_at: Option<CommitmentNumbers>,
    // The commitment number of the party that removed this tlc
    // (only the receiver is allowed to remove) when the tlc is removed.
    removed_at: Option<(CommitmentNumbers, RemoveTlcReason)>,
    // The initial commitment number of the party (the offerer) that
    // has confirmed the removal of this tlc.
    removal_confirmed_at: Option<CommitmentNumbers>,
}

impl DetailedTLCInfo {
    fn is_offered(&self) -> bool {
        self.tlc.is_offered()
    }

    fn get_commitment_numbers(&self, local: bool) -> CommitmentNumbers {
        let am_i_sending_the_tlc = {
            if self.is_offered() {
                local
            } else {
                !local
            }
        };
        if am_i_sending_the_tlc {
            self.created_at
        } else {
            self.creation_confirmed_at
                .expect("Commitment number is present")
        }
    }
}

pub fn get_tweak_by_commitment_point(commitment_point: &Pubkey) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(&commitment_point.serialize());
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}

fn derive_private_key(secret: &Privkey, commitment_point: &Pubkey) -> Privkey {
    secret.tweak(get_tweak_by_commitment_point(commitment_point))
}

fn derive_public_key(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    base_key.tweak(get_tweak_by_commitment_point(commitment_point))
}

pub fn derive_revocation_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

pub fn derive_payment_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

pub fn derive_delayed_payment_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

pub fn derive_tlc_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

/// A simple implementation of [`WriteableEcdsaChannelSigner`] that just keeps the private keys in memory.
///
/// This implementation performs no policy checks and is insufficient by itself as
/// a secure external signer.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
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
        let revocation_base_key = key_derive(funding_key.as_ref(), b"revocation base key");
        let payment_key = key_derive(revocation_base_key.as_ref(), b"payment key");
        let delayed_payment_base_key =
            key_derive(payment_key.as_ref(), b"delayed payment base key");
        let tlc_base_key = key_derive(delayed_payment_base_key.as_ref(), b"HTLC base key");
        let musig2_base_nonce = key_derive(tlc_base_key.as_ref(), b"musig nocne");

        Self {
            funding_key,
            revocation_base_key,
            payment_key,
            delayed_payment_base_key,
            tlc_base_key,
            musig2_base_nonce,
            commitment_seed,
        }
    }

    fn get_base_public_keys(&self) -> ChannelBasePublicKeys {
        ChannelBasePublicKeys {
            funding_pubkey: self.funding_key.pubkey(),
            revocation_base_key: self.revocation_base_key.pubkey(),
            payment_base_key: self.payment_key.pubkey(),
            delayed_payment_base_key: self.delayed_payment_base_key.pubkey(),
            tlc_base_key: self.tlc_base_key.pubkey(),
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
        debug!(
            "Revocation key: {}",
            hex::encode(
                derive_private_key(&self.revocation_base_key, &per_commitment_point).as_ref()
            )
        );
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

    // TODO: Verify that this is a secure way to derive the nonce.
    pub fn derive_musig2_nonce(&self, commitment_number: u64) -> SecNonce {
        let commitment_point = self.get_commitment_point(commitment_number);
        let seckey = derive_private_key(&self.musig2_base_nonce, &commitment_point);
        debug!(
            "Deriving Musig2 nonce: commitment number: {}, commitment point: {:?}",
            commitment_number, commitment_point
        );
        SecNonce::build(seckey.as_ref()).build()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ckb::{
            network::{AcceptChannelCommand, OpenChannelCommand},
            test_utils::NetworkNode,
            NetworkActorCommand, NetworkActorMessage,
        },
        NetworkServiceEvent,
    };

    use ckb_jsonrpc_types::Status;
    use ractor::call;

    use super::{super::types::Privkey, derive_private_key, derive_tlc_pubkey, InMemorySigner};

    #[test]
    fn test_per_commitment_point_and_secret_consistency() {
        let signer = InMemorySigner::generate_from_seed(&[1; 32]);
        assert_eq!(
            signer.get_commitment_point(0),
            Privkey::from(&signer.get_commitment_secret(0)).pubkey()
        );
    }

    #[test]
    fn test_derive_private_and_public_keys() {
        let privkey = Privkey::from(&[1; 32]);
        let per_commitment_point = Privkey::from(&[2; 32]).pubkey();
        let derived_privkey = derive_private_key(&privkey, &per_commitment_point);
        let derived_pubkey = derive_tlc_pubkey(&privkey.pubkey(), &per_commitment_point);
        assert_eq!(derived_privkey.pubkey(), derived_pubkey);
    }

    #[tokio::test]
    async fn test_open_channel_to_peer() {
        let [node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes(2)
            .await
            .try_into()
            .unwrap();

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: node_b.peer_id.clone(),
                    funding_amount: 100000000000,
                    funding_udt_type_script: None,
                    commitment_fee_rate: None,
                    funding_fee_rate: None,
                },
                rpc_reply,
            ))
        };
        let _open_channel_result = call!(node_a.network_actor, message)
            .expect("node_a alive")
            .expect("open channel success");

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
        use crate::ckb::channel::DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT;

        let [node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes(2)
            .await
            .try_into()
            .unwrap();

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: node_b.peer_id.clone(),
                    funding_amount: 100000000000,
                    funding_udt_type_script: None,
                    commitment_fee_rate: None,
                    funding_fee_rate: None,
                },
                rpc_reply,
            ))
        };
        let open_channel_result = call!(node_a.network_actor, message)
            .expect("node_a alive")
            .expect("open channel success");

        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                    assert_eq!(peer_id, &node_a.peer_id);
                    true
                }
                _ => false,
            })
            .await;

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: open_channel_result.channel_id,
                    funding_amount: DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT as u128,
                },
                rpc_reply,
            ))
        };

        let _accept_channel_result = call!(node_b.network_actor, message)
            .expect("node_b alive")
            .expect("accept channel success");
    }

    #[tokio::test]
    async fn test_create_channel() {
        let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes(2)
            .await
            .try_into()
            .unwrap();

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: node_b.peer_id.clone(),
                    funding_amount: 100000000000,
                    funding_udt_type_script: None,
                    commitment_fee_rate: None,
                    funding_fee_rate: None,
                },
                rpc_reply,
            ))
        };
        let open_channel_result = call!(node_a.network_actor, message)
            .expect("node_a alive")
            .expect("open channel success");

        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                    assert_eq!(peer_id, &node_a.peer_id);
                    true
                }
                _ => false,
            })
            .await;
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: open_channel_result.channel_id,
                    funding_amount: 6200000000,
                },
                rpc_reply,
            ))
        };
        let accept_channel_result = call!(node_b.network_actor, message)
            .expect("node_b alive")
            .expect("accept channel success");
        let new_channel_id = accept_channel_result.new_channel_id;

        let node_a_commitment_tx = node_a
            .expect_to_process_event(|event| match event {
                NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                    println!(
                        "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                        num, &tx, peer_id, channel_id
                    );
                    assert_eq!(peer_id, &node_b.peer_id);
                    assert_eq!(channel_id, &new_channel_id);
                    Some(tx.clone())
                }
                _ => None,
            })
            .await;

        let node_b_commitment_tx = node_b
            .expect_to_process_event(|event| match event {
                NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                    println!(
                        "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                        num, &tx, peer_id, channel_id
                    );
                    assert_eq!(peer_id, &node_a.peer_id);
                    assert_eq!(channel_id, &new_channel_id);
                    Some(tx.clone())
                }
                _ => None,
            })
            .await;

        node_a
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelReady(peer_id, channel_id) => {
                    println!(
                        "A channel ({:?}) to {:?} is now ready",
                        &channel_id, &peer_id
                    );
                    assert_eq!(peer_id, &node_b.peer_id);
                    assert_eq!(channel_id, &new_channel_id);
                    true
                }
                _ => false,
            })
            .await;

        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelReady(peer_id, channel_id) => {
                    println!(
                        "A channel ({:?}) to {:?} is now ready",
                        &channel_id, &peer_id
                    );
                    assert_eq!(peer_id, &node_a.peer_id);
                    assert_eq!(channel_id, &new_channel_id);
                    true
                }
                _ => false,
            })
            .await;

        // We can submit the commitment txs to the chain now.
        assert_eq!(
            node_a.submit_tx(node_a_commitment_tx.clone()).await,
            Status::Committed
        );
        assert_eq!(
            node_b.submit_tx(node_b_commitment_tx.clone()).await,
            Status::Committed
        );
    }

    #[tokio::test]
    async fn test_reestablish_channel() {
        let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes(2)
            .await
            .try_into()
            .unwrap();

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: node_b.peer_id.clone(),
                    funding_amount: 100000000000,
                    funding_udt_type_script: None,
                    commitment_fee_rate: None,
                    funding_fee_rate: None,
                },
                rpc_reply,
            ))
        };
        let open_channel_result = call!(node_a.network_actor, message)
            .expect("node_a alive")
            .expect("open channel success");

        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                    println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                    assert_eq!(peer_id, &node_a.peer_id);
                    true
                }
                _ => false,
            })
            .await;

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: open_channel_result.channel_id,
                    funding_amount: 6200000000,
                },
                rpc_reply,
            ))
        };
        let _accept_channel_result = call!(node_b.network_actor, message)
            .expect("node_b alive")
            .expect("accept channel success");

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

        node_a
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::DisconnectPeer(node_b.peer_id.clone()),
            ))
            .expect("node_a alive");

        node_a
            .expect_event(|event| match event {
                NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                    assert_eq!(peer_id, &node_b.peer_id);
                    true
                }
                _ => false,
            })
            .await;

        node_b
            .expect_event(|event| match event {
                NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                    assert_eq!(peer_id, &node_a.peer_id);
                    true
                }
                _ => false,
            })
            .await;

        node_a.connect_to(&node_b).await;

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
