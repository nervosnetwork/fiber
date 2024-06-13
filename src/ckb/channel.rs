use bitflags::bitflags;
use ckb_hash::{blake2b_256, new_blake2b};
use ckb_sdk::Since;
use ckb_types::{
    core::{TransactionBuilder, TransactionView},
    packed::{Bytes, CellInput, CellOutput, OutPoint, Script, Transaction},
    prelude::{AsTransactionBuilder, IntoTransactionView, Pack, Unpack},
};

use log::{debug, error, info, warn};
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
    collections::{hash_map, HashMap},
    fmt::Debug,
};

use crate::{
    ckb::types::Shutdown,
    ckb_chain::{
        contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract},
        FundingRequest, FundingUdtInfo,
    },
    NetworkServiceEvent,
};

use super::{
    key::blake2b_hash_with_salt,
    network::PCNMessageWithPeerId,
    serde_utils::EntityHex,
    types::{
        AcceptChannel, AddTlc, ChannelReady, ClosingSigned, CommitmentSigned, Hash256, LockTime,
        OpenChannel, PCNMessage, Privkey, Pubkey, RemoveTlc, RemoveTlcReason, RevokeAndAck,
        TxCollaborationMsg, TxComplete, TxUpdate,
    },
    NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
};

// - `version`: 8 bytes, u64 in little-endian
// - `funding_out_point`: 36 bytes, out point of the funding transaction
// - `pubkey`: 32 bytes, x only aggregated public key
// - `signature`: 64 bytes, aggregated signature
const FUNDING_CELL_WITNESS_LEN: usize = 8 + 36 + 32 + 64;
// Some part of the code liberally gets previous commitment number, which is
// the current commitment number minus 1. We deliberately set initial commitment number to 1,
// so that we can get previous commitment point/number without checking if the channel
// is funded or not.
pub const INITIAL_COMMITMENT_NUMBER: u64 = 0;

pub enum ChannelActorMessage {
    /// Command are the messages that are sent to the channel actor to perform some action.
    /// It is normally generated from a user request.
    Command(ChannelCommand),
    /// Some system events associated to a channel, such as the funding transaction confirmed.
    Event(ChannelEvent),
    /// PeerMessage are the messages sent from the peer.
    PeerMessage(PCNMessage),
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
    Shutdown(ShutdownCommand),
}

#[derive(Debug)]
pub enum TxCollaborationCommand {
    TxUpdate(TxUpdateCommand),
    TxComplete(TxCompleteCommand),
}

#[derive(Debug)]
pub struct AddTlcCommand {
    pub amount: u128,
    pub preimage: Option<Hash256>,
    pub payment_hash: Option<Hash256>,
    pub expiry: LockTime,
}

#[derive(Debug)]
pub struct RemoveTlcCommand {
    pub id: u64,
    pub reason: RemoveTlcReason,
}

#[derive(Debug)]
pub struct ShutdownCommand {
    pub close_script: Script,
    pub fee: u128,
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

pub const DEFAULT_FEE_RATE: u64 = 0;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 0;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u128 = u128::MAX;
pub const DEFAULT_MAX_ACCEPT_TLCS: u64 = u64::MAX;
pub const DEFAULT_MIN_TLC_VALUE: u128 = 0;
pub const DEFAULT_TO_LOCAL_DELAY_BLOCKS: u64 = 10;
pub const DEFAULT_UDT_MINIMAL_CKB_AMOUNT: u64 = 200 * 100_000_000; // 200 CKB

#[derive(Debug)]
pub struct TxUpdateCommand {
    pub transaction: Transaction,
}

#[derive(Debug)]
pub struct TxCompleteCommand {}

pub enum ChannelInitializationParameter {
    /// To open a new channel to another peer, the funding amount,
    /// the temporary channel id a unique channel seed to generate
    /// channel secrets must be given.
    OpenChannel(u128, [u8; 32], Option<Script>, oneshot::Sender<Hash256>),
    /// To accept a new channel from another peer, the funding amount,
    /// a unique channel seed to generate unique channel id,
    /// original OpenChannel message and an oneshot
    /// channel to receive the new channel ID must be given.
    AcceptChannel(
        u128,
        [u8; 32],
        OpenChannel,
        Option<oneshot::Sender<Hash256>>,
    ),
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
        message: PCNMessage,
    ) -> Result<(), ProcessingChannelError> {
        match message {
            PCNMessage::OpenChannel(_) => {
                panic!("OpenChannel message should be processed while prestarting")
            }
            PCNMessage::AcceptChannel(accept_channel) => {
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
                            state.local_ckb_amount,
                            state.remote_ckb_amount,
                        ),
                    ))
                    .expect("network actor alive");
                Ok(())
            }
            PCNMessage::TxUpdate(tx) => {
                state.handle_tx_collaboration_msg(TxCollaborationMsg::TxUpdate(tx), &self.network)
            }
            PCNMessage::TxComplete(tx) => {
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
            PCNMessage::CommitmentSigned(commitment_signed) => {
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
                            .expect("myself alive");
                    }
                }
                Ok(())
            }
            PCNMessage::TxSignatures(tx_signatures) => {
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
                        .expect("network actor alive");

                    state.state =
                        ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::empty());
                    return Ok(());
                };

                state.handle_tx_signatures(&self.network, Some(tx_signatures.witnesses))?;
                Ok(())
            }
            PCNMessage::RevokeAndAck(revoke_and_ack) => {
                state.handle_revoke_and_ack_message(revoke_and_ack)?;
                if let ChannelState::ChannelReady(flags) = state.state {
                    if flags.is_empty() {
                        self.handle_commitment_signed_command(state)?;
                    }
                }
                state.update_state(ChannelState::ChannelReady(ChannelReadyFlags::empty()));
                Ok(())
            }
            PCNMessage::ChannelReady(channel_ready) => {
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
            PCNMessage::AddTlc(add_tlc) => {
                state.check_state_for_tlc_update()?;

                let tlc = state.create_inbounding_tlc(add_tlc);
                let id = tlc.id;

                match state.pending_received_tlcs.get(&id) {
                    Some(current) if current == &tlc => {
                        debug!(
                            "Repeated processing of AddTlcCommand with id {:?}: current tlc {:?}",
                            id, current,
                        );
                        return Ok(());
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
                        debug!("Adding tlc {:?} to channel {:?}", &tlc, state.get_id());
                    }
                }

                state.pending_received_tlcs.insert(tlc.id, tlc);
                state.to_remote_amount -= tlc.amount;

                // TODO: here we didn't send any ack message to the peer.
                // The peer may falsely believe that we have already processed this message,
                // while we have crashed. We need a way to make sure that the peer will resend
                // this message, and our processing of this message is idempotent.
                Ok(())
            }
            PCNMessage::RemoveTlc(remove_tlc) => {
                state.check_state_for_tlc_update()?;

                match state.pending_offered_tlcs.entry(remove_tlc.tlc_id) {
                    hash_map::Entry::Occupied(entry) => {
                        let current = entry.get();
                        match remove_tlc.reason {
                            RemoveTlcReason::RemoveTlcFail(_fail) => {
                                state.to_local_amount += current.amount;
                            }
                            RemoveTlcReason::RemoveTlcFulfill(fulfill) => {
                                let preimage = fulfill.payment_preimage;
                                if current.payment_hash != blake2b_256(preimage).into() {
                                    return Err(ProcessingChannelError::InvalidParameter(
                                        "Payment preimage does not match the hash".to_string(),
                                    ));
                                }
                                state.to_remote_amount += current.amount;
                            }
                        }
                        entry.remove();
                        if state.pending_offered_tlcs.is_empty() {
                            state.maybe_transition_to_shutdown(&self.network)?;
                        }
                        Ok(())
                    }
                    hash_map::Entry::Vacant(_) => {
                        Err(ProcessingChannelError::InvalidParameter(format!(
                            "TLC with id {:?} not found in pending_received_tlcs",
                            remove_tlc.tlc_id
                        )))
                    }
                }
            }
            PCNMessage::Shutdown(shutdown) => {
                let flags = match state.state {
                    ChannelState::ChannelReady(_) => ShuttingDownFlags::empty(),
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
                state.remote_shutdown_fee = Some(shutdown.fee);

                let flags = flags | ShuttingDownFlags::THEIR_SHUTDOWN_SENT;
                state.update_state(ChannelState::ShuttingDown(flags));
                state.maybe_transition_to_shutdown(&self.network)?;

                Ok(())
            }
            PCNMessage::ClosingSigned(closing) => {
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

            _ => {
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
            ChannelState::ChannelReady(flags)
                if flags.contains(ChannelReadyFlags::AWAITING_REMOTE_REVOKE) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, which requires the remote to send a revoke message first.",
                    &state.state
                )));
            }
            ChannelState::ChannelReady(flags) => CommitmentSignedFlags::ChannelReady(flags),
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &state.state
                )));
            }
        };

        let PartiallySignedCommitmentTransaction {
            tx,
            signature,
            msg: _,
            version: _,
        } = state.build_and_sign_commitment_tx()?;
        debug!(
            "Built and signed a commitment tx ({:?}) with partial signature {:?}",
            &tx, &signature
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
                NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                    peer_id: state.peer_id.clone(),
                    message: PCNMessage::CommitmentSigned(commitment_signed),
                }),
            ))
            .expect("network actor alive");

        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT;
                state.update_state(ChannelState::SigningCommitment(flags));
                state.maybe_transition_to_tx_signatures(flags, &self.network)?;
            }
            CommitmentSignedFlags::ChannelReady(flags) => {
                let flags = flags | ChannelReadyFlags::AWAITING_REMOTE_REVOKE;
                state.update_state(ChannelState::ChannelReady(flags));
            }
        }
        Ok(())
    }

    pub fn handle_add_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: AddTlcCommand,
    ) -> Result<u64, ProcessingChannelError> {
        state.check_state_for_tlc_update()?;

        let tlc = state.create_outbounding_tlc(command);

        // TODO: Note that since message sending is async,
        // we can't guarantee anything about the order of message sending
        // and state updating. And any of these may fail while the other succeeds.
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
        state.pending_offered_tlcs.insert(tlc.id, tlc);
        state.to_local_amount -= tlc.amount;
        state.next_offering_tlc_id += 1;
        debug!(
            "Added tlc with id {:?} to pending offered tlcs: {:?}",
            tlc.id, &tlc
        );
        debug!(
            "Current pending offered tlcs: {:?}",
            &state.pending_offered_tlcs
        );
        debug!(
            "Current pending received tlcs: {:?}",
            &state.pending_received_tlcs
        );
        debug!(
            "Balance after addtlccommand: to_local_amount: {} to_remote_amount: {}",
            state.to_local_amount, state.to_remote_amount
        );
        Ok(tlc.id)
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
                if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = command.reason {
                    let filled_payment_hash: Hash256 = blake2b_256(fulfill.payment_preimage).into();
                    if tlc.payment_hash != filled_payment_hash {
                        state.pending_received_tlcs.insert(tlc.id, tlc);
                        return Err(ProcessingChannelError::InvalidParameter(format!(
                            "Preimage {:?} is hashed to {}, which does not match payment hash {:?}",
                            fulfill.payment_preimage, filled_payment_hash, tlc.payment_hash,
                        )));
                    }
                }
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(msg),
                    ))
                    .expect("network actor alive");
                match command.reason {
                    RemoveTlcReason::RemoveTlcFail(_) => {
                        state.to_remote_amount += tlc.amount;
                    }
                    RemoveTlcReason::RemoveTlcFulfill(_) => {
                        state.to_local_amount += tlc.amount;
                    }
                }
            }
            None => {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Trying to remove tlc with id {:?} that is not in pending received tlcs",
                    command.id
                )));
            }
        }
        debug!(
            "Balance after removetlccommand: to_local_amount: {} to_remote_amount: {}",
            state.to_local_amount, state.to_remote_amount
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
            ChannelState::ChannelReady(_) => {
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
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                    peer_id: self.peer_id.clone(),
                    message: PCNMessage::Shutdown(Shutdown {
                        channel_id: state.get_id(),
                        close_script: command.close_script.clone(),
                        fee: command.fee,
                    }),
                }),
            ))
            .expect("network actor alive");
        state.local_shutdown_script = Some(command.close_script.clone());
        state.local_shutdown_fee = Some(command.fee);
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
                let pcn_msg = PCNMessage::TxUpdate(TxUpdate {
                    channel_id: state.get_id(),
                    tx: tx_update.transaction.clone(),
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId::new(
                            self.peer_id.clone(),
                            pcn_msg,
                        )),
                    ))
                    .expect("network actor alive");

                state.update_state(ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG,
                ));
                state.funding_tx = Some(tx_update.transaction.clone());
                state.maybe_complete_tx_collaboration(
                    tx_update.transaction.clone(),
                    &self.network,
                )?;
            }
            TxCollaborationCommand::TxComplete(_) => {
                state.check_tx_complete_preconditions()?;
                let pcn_msg = PCNMessage::TxComplete(TxComplete {
                    channel_id: state.get_id(),
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId::new(
                            self.peer_id.clone(),
                            pcn_msg,
                        )),
                    ))
                    .expect("network actor alive");
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
            ChannelCommand::Shutdown(command) => self.handle_shutdown_command(state, command),
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
            ChannelInitializationParameter::AcceptChannel(
                my_funding_amount,
                seed,
                open_channel,
                oneshot_channel,
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
                    funding_udt_type_script,
                    funding_amount,
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
                    funding_udt_type_script.clone(),
                    &seed,
                    peer_id.clone(),
                    *funding_amount,
                    *to_local_delay,
                    counterpart_pubkeys,
                    next_local_nonce.clone(),
                    *first_per_commitment_point,
                    *second_per_commitment_point,
                );

                let commitment_number = 0;

                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: my_funding_amount,
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
                state.update_state(ChannelState::NegotiatingFunding(
                    NegotiatingFundingFlags::INIT_SENT,
                ));
                if let Some(sender) = oneshot_channel {
                    sender.send(state.get_id()).expect("Receive not dropped");
                }
                Ok(state)
            }
            ChannelInitializationParameter::OpenChannel(
                funding_amount,
                seed,
                funding_udt_type_script,
                tx,
            ) => {
                let peer_id = self.peer_id.clone();
                info!("Trying to open a channel to {:?}", &peer_id);

                let mut channel = ChannelActorState::new_outbound_channel(
                    &seed,
                    self.peer_id.clone(),
                    funding_amount,
                    funding_udt_type_script.clone(),
                    LockTime::new(DEFAULT_TO_LOCAL_DELAY_BLOCKS),
                );

                let commitment_number = 0;
                let message = PCNMessage::OpenChannel(OpenChannel {
                    chain_hash: Hash256::default(),
                    channel_id: channel.get_id(),
                    funding_udt_type_script,
                    funding_amount: channel.to_local_amount,
                    funding_fee_rate: DEFAULT_FEE_RATE,
                    commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
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
                        // TODO: The channel id here is a temporary channel id,
                        // while the ChannelCreated event emitted by the counterpart
                        // is a real channel id. This may cause confusion.
                        NetworkActorEvent::ChannelCreated(
                            channel.get_id(),
                            self.peer_id.clone(),
                            myself,
                        ),
                    ))
                    .expect("network actor alive");

                tx.send(channel.get_id()).expect("Receive not dropped");
                Ok(channel)
            }
            ChannelInitializationParameter::ReestablishChannel(channel_id) => {
                let channel = self
                    .store
                    .get_channel_actor_state(&channel_id)
                    .expect("channel should exist");

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
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ChannelActorMessage::PeerMessage(message) => {
                if let Err(error) = self.handle_peer_message(state, message) {
                    error!("Error while processing channel message: {:?}", error);
                }
            }
            ChannelActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(state, command) {
                    error!("Error while processing channel command: {:?}", err);
                }
            }
            ChannelActorMessage::Event(e) => match e {
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
                            NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                                peer_id: state.peer_id.clone(),
                                message: PCNMessage::ChannelReady(ChannelReady {
                                    channel_id: state.get_id(),
                                }),
                            }),
                        ))
                        .expect("network actor alive");
                    let flags = flags | AwaitingChannelReadyFlags::OUR_CHANNEL_READY;
                    state.update_state(ChannelState::AwaitingChannelReady(flags));
                    if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                        state.on_channel_ready(&self.network);
                    }
                }
                ChannelEvent::PeerDisconnected => {
                    myself.stop(Some("PeerDisconnected".to_string()));
                }
            },
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

    // Is this channel initially inbound?
    // An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,

    pub to_local_amount: u128,
    pub to_remote_amount: u128,

    // only used for UDT scenario:
    // `to_local_amount` and `to_remote_amount` are the amount of UDT
    // we need to keep track of the CKB amount from partners in the channel.
    pub local_ckb_amount: u64,
    pub remote_ckb_amount: u64,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    // Cached channel parameter for easier of access.
    pub local_channel_parameters: ChannelParametersOneParty,
    // The holder has set a shutdown script.
    #[serde_as(as = "Option<EntityHex>")]
    pub local_shutdown_script: Option<Script>,

    // Commitment numbers that are used to derive keys.
    // This value is guaranteed to be 1 when channel is just created.
    pub local_commitment_number: u64,
    pub remote_commitment_number: u64,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).

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

    // The counterparty has already sent a shutdown message with this script.
    #[serde_as(as = "Option<EntityHex>")]
    pub remote_shutdown_script: Option<Script>,

    pub remote_nonce: Option<PubNonce>,

    // All the commitment point that are sent from the counterparty.
    // We need to save all these points to derive the keys for the commitment transactions.
    pub remote_commitment_points: Vec<Pubkey>,
    pub remote_channel_parameters: Option<ChannelParametersOneParty>,
    pub local_shutdown_signature: Option<PartialSignature>,
    pub local_shutdown_fee: Option<u128>,
    pub remote_shutdown_signature: Option<PartialSignature>,
    pub remote_shutdown_fee: Option<u128>,
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
    #[error("Invalid state: ")]
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
    pub struct ChannelReadyFlags: u32 {
        /// Indicates that we have sent a `commitment_signed` but are awaiting the responding
        ///	`revoke_and_ack` message. During this period, we can't generate new messages as
        /// we'd be unable to determine which TLCs they included in their `revoke_and_ack`
        ///	implicit ACK, so instead we have to hold them away temporarily to be sent later.
        const AWAITING_REMOTE_REVOKE = 1;
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
enum CommitmentSignedFlags {
    SigningCommitment(SigningCommitmentFlags),
    ChannelReady(ChannelReadyFlags),
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
    ChannelReady(ChannelReadyFlags),
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
impl ChannelActorState {
    pub fn new_inbound_channel<'a>(
        temp_channel_id: Hash256,
        local_value: u128,
        funding_udt_type_script: Option<Script>,
        seed: &[u8],
        peer_id: PeerId,
        remote_value: u128,
        remote_delay: LockTime,
        remote_pubkeys: ChannelBasePublicKeys,
        remote_nonce: PubNonce,
        first_commitment_point: Pubkey,
        second_commitment_point: Pubkey,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_pubkeys = signer.to_channel_public_keys(INITIAL_COMMITMENT_NUMBER);

        let channel_id = derive_channel_id_from_revocation_keys(
            &local_pubkeys.revocation_base_key,
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
            id: channel_id,
            next_offering_tlc_id: 0,
            next_receiving_tlc_id: 0,
            pending_offered_tlcs: Default::default(),
            pending_received_tlcs: Default::default(),
            to_remote_amount: remote_value,
            local_shutdown_script: None,
            local_channel_parameters: ChannelParametersOneParty {
                pubkeys: local_pubkeys,
                selected_contest_delay: remote_delay,
            },
            signer,
            remote_channel_parameters: Some(ChannelParametersOneParty {
                pubkeys: remote_pubkeys,
                selected_contest_delay: remote_delay,
            }),
            local_commitment_number: INITIAL_COMMITMENT_NUMBER,
            remote_commitment_number: INITIAL_COMMITMENT_NUMBER,
            remote_shutdown_script: None,
            remote_nonce: Some(remote_nonce),
            remote_commitment_points: vec![first_commitment_point, second_commitment_point],
            local_shutdown_signature: None,
            local_shutdown_fee: None,
            remote_shutdown_signature: None,
            remote_shutdown_fee: None,
            local_ckb_amount: DEFAULT_UDT_MINIMAL_CKB_AMOUNT,
            remote_ckb_amount: DEFAULT_UDT_MINIMAL_CKB_AMOUNT,
        }
    }

    pub fn new_outbound_channel(
        seed: &[u8],
        peer_id: PeerId,
        value: u128,
        funding_udt_type_script: Option<Script>,
        to_local_delay: LockTime,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_pubkeys = signer.to_channel_public_keys(INITIAL_COMMITMENT_NUMBER);
        let temp_channel_id =
            derive_temp_channel_id_from_revocation_key(&local_pubkeys.revocation_base_key);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            peer_id,
            funding_tx: None,
            funding_udt_type_script,
            is_acceptor: false,
            to_local_amount: value,
            id: temp_channel_id,
            next_offering_tlc_id: 0,
            next_receiving_tlc_id: 0,
            pending_offered_tlcs: Default::default(),
            pending_received_tlcs: Default::default(),
            to_remote_amount: 0,
            signer,
            local_channel_parameters: ChannelParametersOneParty {
                pubkeys: local_pubkeys,
                selected_contest_delay: to_local_delay,
            },
            remote_channel_parameters: None,
            local_commitment_number: INITIAL_COMMITMENT_NUMBER,
            remote_nonce: None,
            remote_commitment_number: INITIAL_COMMITMENT_NUMBER,
            remote_commitment_points: vec![],
            local_shutdown_script: None,
            local_shutdown_fee: None,
            remote_shutdown_script: None,
            remote_shutdown_fee: None,
            local_shutdown_signature: None,
            remote_shutdown_signature: None,
            local_ckb_amount: DEFAULT_UDT_MINIMAL_CKB_AMOUNT,
            remote_ckb_amount: DEFAULT_UDT_MINIMAL_CKB_AMOUNT,
        }
    }

    fn update_state(&mut self, new_state: ChannelState) {
        debug!(
            "Updating channel state from {:?} to {:?}",
            &self.state, &new_state
        );
        self.state = new_state;
    }
}

// Properties for the channel actor state.
impl ChannelActorState {
    pub fn get_id(&self) -> Hash256 {
        self.id
    }

    pub fn get_local_nonce(&self) -> impl Borrow<PubNonce> {
        self.get_next_local_secnonce().public_nonce()
    }

    pub fn get_next_local_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_next_commitment_number(true))
    }

    pub fn get_next_local_nonce(&self) -> PubNonce {
        self.signer
            .derive_musig2_nonce(self.get_next_commitment_number(true))
            .public_nonce()
    }

    pub fn get_remote_nonce(&self) -> &PubNonce {
        self.remote_nonce.as_ref().unwrap()
    }

    pub fn get_channel_parameters(&self, local: bool) -> &ChannelParametersOneParty {
        if local {
            self.get_local_channel_parameters()
        } else {
            self.get_remote_channel_parameters()
        }
    }

    pub fn get_local_channel_parameters(&self) -> &ChannelParametersOneParty {
        &self.local_channel_parameters
    }

    pub fn get_remote_channel_parameters(&self) -> &ChannelParametersOneParty {
        self.remote_channel_parameters.as_ref().unwrap()
    }

    pub fn get_current_commitment_number(&self, local: bool) -> u64 {
        if local {
            self.local_commitment_number
        } else {
            self.remote_commitment_number
        }
    }

    pub fn get_next_commitment_number(&self, local: bool) -> u64 {
        self.get_current_commitment_number(local) + 1
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

    pub fn get_current_local_commitment_point(&self) -> Pubkey {
        self.get_local_commitment_point(self.local_commitment_number)
    }

    // Get the commitment secret for previous commitment transaction.
    // This is used to revoke previous commitment transaction.
    pub fn get_previous_local_commitment_secret(&self) -> [u8; 32] {
        let prev_commitment_number = if self.local_commitment_number == INITIAL_COMMITMENT_NUMBER {
            INITIAL_COMMITMENT_NUMBER
        } else {
            self.local_commitment_number - 1
        };
        self.signer.get_commitment_secret(prev_commitment_number)
    }

    // Get the commitment point for previous commitment transaction.
    // This is used to verify counterparty's revocation of previous
    // commitment transaction.
    pub fn get_previous_remote_commitment_point(&self) -> Pubkey {
        let prev_commitment_number = if self.remote_commitment_number == INITIAL_COMMITMENT_NUMBER {
            INITIAL_COMMITMENT_NUMBER
        } else {
            self.remote_commitment_number - 1
        };
        self.get_remote_commitment_point(prev_commitment_number)
    }

    pub fn get_current_remote_commitment_point(&self) -> Pubkey {
        self.get_remote_commitment_point(self.remote_commitment_number)
    }

    /// Get the counterparty commitment point for the given commitment number.
    fn get_remote_commitment_point(&self, commitment_number: u64) -> Pubkey {
        let index = commitment_number as usize;
        let commitment_point = self.remote_commitment_points[index];
        debug!(
            "Obtained remote commitment point #{} (counting from 1) of {}: {:?}",
            index + 1,
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
            hex::encode(&args),
            hex::encode(&args[..20])
        );
        get_script_by_contract(Contract::FundingLock, args.as_slice())
    }

    pub fn get_funding_request(&self, fee_rate: u64) -> FundingRequest {
        FundingRequest {
            udt_info: self.funding_udt_type_script.as_ref().map(|script| {
                FundingUdtInfo::new(script, self.local_ckb_amount, self.remote_ckb_amount)
            }),
            script: self.get_funding_lock_script(),
            local_amount: self.to_local_amount as u64,
            local_fee_rate: fee_rate,
            remote_amount: self.to_remote_amount as u64,
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
            .derive_musig2_nonce(self.local_commitment_number)
    }

    pub fn get_local_musig2_pubnonce(&self) -> PubNonce {
        self.get_local_musig2_secnonce().public_nonce()
    }

    pub fn get_musig2_agg_pubnonce(&self) -> AggNonce {
        let local_nonce = self.get_local_nonce();
        let local_nonce = local_nonce.borrow();
        let remote_nonce = self.get_remote_nonce();
        let nonces = self.order_things_for_musig2(local_nonce, remote_nonce);
        AggNonce::sum(nonces)
    }

    pub fn get_all_tlcs(&self) -> Vec<TLC> {
        debug!("Getting tlcs for commitment tx");
        debug!(
            "Current pending offered tlcs: {:?}",
            self.pending_offered_tlcs
        );
        debug!(
            "Current pending received tlcs: {:?}",
            self.pending_received_tlcs
        );
        self.pending_offered_tlcs
            .values()
            .chain(self.pending_received_tlcs.values())
            .cloned()
            .collect()
    }

    fn any_tlc_pending(&self) -> bool {
        !self.pending_offered_tlcs.is_empty() || !self.pending_received_tlcs.is_empty()
    }

    pub fn get_remote_funding_pubkey(&self) -> &Pubkey {
        &self.get_remote_channel_parameters().pubkeys.funding_pubkey
    }

    pub fn check_state_for_tlc_update(&self) -> ProcessingChannelResult {
        match self.state {
            ChannelState::ChannelReady(flags) if flags.is_empty() => Ok(()),
            ChannelState::ShuttingDown(_) => Ok(()),
            _ => Err(ProcessingChannelError::InvalidState(format!(
                "Invalid state {:?} for adding tlc",
                self.state
            ))),
        }
    }

    pub fn create_outbounding_tlc(&mut self, command: AddTlcCommand) -> TLC {
        // TODO: we are filling the user command with a new id here.
        // The advantage of this is that we don't need to burden the users to
        // provide a next id for each tlc. The disadvantage is that users may
        // inadvertently click the same button twice, and we will process the same
        // twice, the frontend needs to prevent this kind of behaviour.
        // Is this what we want?
        let id = self.next_offering_tlc_id;
        assert!(
            self.pending_offered_tlcs.get(&id).is_none(),
            "Must not have the same id in pending offered tlcs"
        );

        let preimage = command.preimage.unwrap_or(get_random_preimage());
        let payment_hash = command
            .payment_hash
            .unwrap_or(blake2b_256(&preimage).into());

        TLC {
            id,
            amount: command.amount,
            payment_hash,
            lock_time: command.expiry,
            is_offered: true,
            payment_preimage: Some(preimage),
            local_commitment_number: self.local_commitment_number,
            remote_commitment_number: self.remote_commitment_number,
            local_payment_key_hash: None,
            remote_payment_key_hash: None,
        }
    }

    pub fn create_inbounding_tlc(&self, message: AddTlc) -> TLC {
        let local_commitment_number = self.local_commitment_number;
        let remote_commitment_number = self.remote_commitment_number;
        TLC {
            id: message.tlc_id,
            amount: message.amount,
            payment_hash: message.payment_hash,
            lock_time: message.expiry,
            is_offered: false,
            payment_preimage: None,
            local_commitment_number,
            remote_commitment_number,
            local_payment_key_hash: None,
            remote_payment_key_hash: None,
        }
    }
}

impl From<&ChannelActorState> for Musig2Context {
    fn from(value: &ChannelActorState) -> Self {
        Musig2Context {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            local_seckey: value.signer.funding_key,
            local_secnonce: value.get_local_musig2_secnonce(),
            remote_pubkey: *value.get_remote_funding_pubkey(),
            remote_pubnonce: value.get_remote_nonce().clone(),
        }
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
                &self.get_all_tlcs()
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
        let local_shutdown_signature = self.local_shutdown_signature.unwrap_or({
            let signature = sign_ctx.clone().sign(message.as_slice())?;
            self.local_shutdown_signature = Some(signature);
            debug!(
                "We have signed shutdown tx ({:?}) message {:?} with signature {:?}",
                &shutdown_tx, &message, &signature,
            );

            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                        peer_id: self.peer_id.clone(),
                        message: PCNMessage::ClosingSigned(ClosingSigned {
                            partial_signature: signature,
                            channel_id: self.get_id(),
                        }),
                    }),
                ))
                .expect("network actor alive");
            signature
        });

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
                    .expect("network actor alive");
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

        self.update_state(ChannelState::NegotiatingFunding(
            NegotiatingFundingFlags::INIT_SENT,
        ));
        self.to_remote_amount = accept_channel.funding_amount;
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
                                msg.tx.clone(),
                                // TODO: use fee rate set by the user.
                                self.get_funding_request(20000),
                            ),
                        ))
                        .expect("network alive");
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
                        .expect("myself alive");
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
            // TODO: There is a chance that both parties unknowingly send commitment_signed messages,
            // and both of they thought they are the first one to send the message and fail the other
            // party's message. What to do in this case?
            ChannelState::ChannelReady(flags)
                if flags.contains(ChannelReadyFlags::AWAITING_REMOTE_REVOKE) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed message in state {:?}, as we are have already sent a commitment_signed, the remote should have sent a revoke message first.",
                    &self.state
                )));
            }
            ChannelState::ChannelReady(flags) => {
                debug!(
                    "Processing commitment_signed message in state {:?}",
                    &self.state
                );
                CommitmentSignedFlags::ChannelReady(flags)
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
                        tx.clone(),
                    ),
                ),
            ))
            .expect("myself alive");

        debug!("Updating peer next local nonce");
        self.remote_nonce = Some(commitment_signed.next_local_nonce);
        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT;
                self.update_state(ChannelState::SigningCommitment(flags));
                self.maybe_transition_to_tx_signatures(flags, network)?;
            }
            CommitmentSignedFlags::ChannelReady(_) => {
                // Now we should revoke previous transation by revealing preimage.
                let revocation_preimage = self.get_previous_local_commitment_secret();
                debug!(
                    "Revealing preimage for revocation: {:?}",
                    &revocation_preimage
                );
                self.local_commitment_number = self.get_next_commitment_number(true);
                let next_commitment_point = self.get_current_local_commitment_point();
                network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId {
                            peer_id: self.peer_id.clone(),
                            message: PCNMessage::RevokeAndAck(RevokeAndAck {
                                channel_id: self.get_id(),
                                per_commitment_secret: revocation_preimage.into(),
                                next_per_commitment_point: next_commitment_point,
                            }),
                        }),
                    ))
                    .expect("network actor alive");
                self.update_state(ChannelState::ChannelReady(ChannelReadyFlags::empty()));
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
            .expect("network alive");
        let flags = flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT;
        self.update_state(ChannelState::AwaitingTxSignatures(flags));

        Ok(())
    }

    pub fn on_channel_ready(&mut self, network: &ActorRef<NetworkActorMessage>) {
        self.update_state(ChannelState::ChannelReady(ChannelReadyFlags::empty()));
        self.local_commitment_number = self.get_next_commitment_number(true);
        self.remote_commitment_number = self.get_next_commitment_number(false);
        debug!(
            "Channel ready commitment numbers {} {}",
            self.local_commitment_number, self.remote_commitment_number
        );
        network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::ChannelReady(self.get_id(), self.peer_id.clone()),
            ))
            .expect("network actor alive");
    }

    pub fn append_remote_commitment_point(&mut self, commitment_point: Pubkey) {
        debug!(
            "Appending remote commitment point #{}: {:?}",
            self.remote_commitment_points.len() + 1,
            commitment_point
        );
        debug_assert_eq!(
            self.remote_commitment_points.len() as u64,
            self.get_current_commitment_number(false) + 1
        );
        self.remote_commitment_points.push(commitment_point);
        self.remote_commitment_number = self.get_next_commitment_number(false);
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
        debug!("Checking commitment secret and point for revocation");
        let per_commitment_point = self.get_previous_remote_commitment_point();
        if per_commitment_point != Privkey::from(per_commitment_secret).pubkey() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Per commitment secret and per commitment point mismatch: secret {:?}, point: {:?}",
                per_commitment_secret, per_commitment_point,
            )));
        }
        self.append_remote_commitment_point(next_per_commitment_point);
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
            error!("Checking if transaction final failed as tx's first output's script is not funding lock: tx: {:?}, first output lock script: {:?}, funding lock script: {:?}", &tx, first_output.lock(), self.get_funding_lock_script());
            // TODO: return an error here. We panic because we want to move fast.
            panic!("Invalid funding transation")
        }

        if self.funding_udt_type_script.is_some() {
            let (_output, data) =
                tx.output_with_data(0)
                    .ok_or(ProcessingChannelError::InvalidParameter(
                        "Funding transaction should have at least one output".to_string(),
                    ))?;
            if data.as_ref().len() >= 16 {
                let mut amount_bytes = [0u8; 16];
                amount_bytes.copy_from_slice(&data.as_ref()[0..16]);
                let udt_amount = u128::from_le_bytes(amount_bytes);
                debug!(
                    "udt_amount: {}, to_remote_amount: {}, to_local_amount: {}",
                    udt_amount, self.to_remote_amount, self.to_local_amount
                );
                return Ok(udt_amount == self.to_remote_amount + self.to_local_amount);
            }
            debug!(
                "is_tx_final: output data length {} is less than 16",
                data.as_ref().len()
            );
            return Ok(false);
        }
        let current_capacity: u64 = first_output.capacity().unpack();
        let is_complete = current_capacity == (self.to_local_amount + self.to_remote_amount) as u64;
        Ok(is_complete)
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
            // We need to send a SendPcnMessage command here (instead of a ControlPcnChannel),
            // to guarantee that the TxComplete message immediately is sent to the network actor.
            // Otherwise, it is possible that when the network actor is processing ControlPcnChannel,
            // it receives another SendPcnMessage command, and that message (e.g. CommitmentSigned)
            // is processed first, thus breaking the order of messages.
            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId::new(
                        self.peer_id.clone(),
                        PCNMessage::TxComplete(TxComplete {
                            channel_id: self.get_id(),
                        }),
                    )),
                ))
                .expect("network actor alive");
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
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Received TxComplete message, but funding tx is not final"
                    )));
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

    // Should we (also called holder) send tx_signatures first?
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
            self.local_shutdown_fee,
            self.remote_shutdown_fee,
        ) {
            (
                Some(local_shutdown_script),
                Some(remote_shutdown_script),
                Some(local_shutdown_fee),
                Some(remote_shutdown_fee),
            ) => (
                local_shutdown_script,
                remote_shutdown_script,
                local_shutdown_fee,
                remote_shutdown_fee,
            ),
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Shutdown scripts are not present: local_shutdown_script {:?}, remote_shutdown_script {:?}, local_shutdown_fee {:?}, remote_shutdown_fee {:?}",
                    &self.local_shutdown_script, &self.remote_shutdown_script,
                    &self.local_shutdown_fee, &self.remote_shutdown_fee
                )));
            }
        };

        let mut contracts = vec![Contract::CommitmentLock];
        if self.funding_udt_type_script.is_some() {
            // TODO(yukang): we need to find corresponding dep cells for UDT
            contracts.push(Contract::SimpleUDT);
        }
        let cell_deps = get_cell_deps_by_contracts(contracts);
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

            let local_capacity: u64 = self.local_ckb_amount - local_shutdown_fee as u64;
            debug!(
                "shutdown_tx local_capacity: {} - {} = {}",
                self.local_ckb_amount, local_shutdown_fee, local_capacity
            );
            let local_output = CellOutput::new_builder()
                .lock(local_shutdown_script.clone())
                .type_(Some(type_script.clone()).pack())
                .capacity(local_capacity.pack())
                .build();
            let local_output_data = self.to_local_amount.to_le_bytes().pack();

            let remote_capacity: u64 = self.remote_ckb_amount - remote_shutdown_fee as u64;
            debug!(
                "shutdown_tx remote_capacity: {} - {} = {}",
                self.remote_ckb_amount, remote_shutdown_fee, remote_capacity
            );
            let remote_output = CellOutput::new_builder()
                .lock(remote_shutdown_script.clone())
                .type_(Some(type_script.clone()).pack())
                .capacity(remote_capacity.pack())
                .build();
            let remote_output_data = self.to_remote_amount.to_le_bytes().pack();

            let outputs = self.order_things_for_musig2(local_output, remote_output);
            let outputs_data = self.order_things_for_musig2(local_output_data, remote_output_data);
            let tx_builder = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(outputs_data.to_vec());
            let tx = tx_builder.build();
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
            let local_value = (self.to_local_amount - local_shutdown_fee) as u64;
            let remote_value = (self.to_remote_amount - remote_shutdown_fee) as u64;
            debug!(
                "Building shutdown transaction with values: {} {}",
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
            let tx_builder = tx_builder.set_outputs(outputs.to_vec());
            let tx_builder =
                tx_builder.set_outputs_data(vec![Default::default(), Default::default()]);
            let tx = tx_builder.build();
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
    // so as to consume the funding cell.
    pub fn build_commitment_tx(&self, local: bool) -> (TransactionView, [u8; 32]) {
        let funding_out_point = self.get_funding_transaction_outpoint();
        let mut contracts = vec![Contract::CommitmentLock];
        if self.funding_udt_type_script.is_some() {
            // TODO(yukang): we need to find corresponding dep cells for UDT
            contracts.push(Contract::SimpleUDT);
        }
        let tx_builder = TransactionBuilder::default()
            .cell_deps(get_cell_deps_by_contracts(contracts))
            .input(
                CellInput::new_builder()
                    .previous_output(funding_out_point.clone())
                    .build(),
            );

        let (outputs, outputs_data) = self.build_commitment_transaction_outputs(local);
        debug!("Built outputs for commitment transaction: {:?}", &outputs);
        let tx_builder = tx_builder.set_outputs(outputs);
        let tx_builder = tx_builder.set_outputs_data(outputs_data);
        let tx = tx_builder.build();
        let version = self.get_current_commitment_number(local);
        let message = get_funding_cell_message_to_sign(version, funding_out_point, &tx);
        debug!(
            "Building {} commitment transaction message to sign {:?} (version {})",
            if local { "local" } else { "remote" },
            hex::encode(message.as_slice()),
            version
        );
        (tx, message)
    }

    fn build_current_commitment_transaction_witnesses(&self, local: bool) -> Vec<u8> {
        let commitment_number = self.get_current_commitment_number(local);
        debug!(
            "Building current commitment transaction witnesses for {} party, commitment number: {}",
            if local { "local" } else { "remote" },
            commitment_number
        );
        self.build_commitment_transaction_witnesses(local, commitment_number)
    }

    // We need this function both for building new commitment transaction and revoking old commitment transaction.
    fn build_commitment_transaction_witnesses(
        &self,
        local: bool,
        commitment_number: u64,
    ) -> Vec<u8> {
        debug!(
            "Building commitment transaction #{}'s witnesses for {} party",
            commitment_number,
            if local { "local" } else { "remote" }
        );
        let (delayed_epoch, delayed_payment_key, revocation_key) = {
            let (delay, commitment_point, base_delayed_payment_key, base_revocation_key) = if local
            {
                (
                    self.get_local_channel_parameters().selected_contest_delay,
                    self.get_local_commitment_point(commitment_number),
                    self.get_local_channel_parameters()
                        .delayed_payment_base_key(),
                    self.get_local_channel_parameters().revocation_base_key(),
                )
            } else {
                (
                    self.get_remote_channel_parameters().selected_contest_delay,
                    self.get_remote_commitment_point(commitment_number),
                    self.get_remote_channel_parameters()
                        .delayed_payment_base_key(),
                    self.get_remote_channel_parameters().revocation_base_key(),
                )
            };
            debug!("Get base witness parameters: delayed time: {:?}, delayed_payment_key: {:?}, revocation_key: {:?}", delay, base_delayed_payment_key, base_revocation_key);
            (
                delay,
                derive_delayed_payment_pubkey(base_delayed_payment_key, &commitment_point),
                derive_revocation_pubkey(base_revocation_key, &commitment_point),
            )
        };

        // Build a sorted array of TLC so that both party can generate the same commitment transaction.
        // TODO: we actually need a historical snapshot of tlcs for the commitment number.
        // This currently only works for the current commitment number.
        let tlcs = {
            let (mut received_tlcs, mut offered_tlcs) = (
                self.pending_received_tlcs
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
                self.pending_offered_tlcs
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            for tlc in received_tlcs.iter_mut().chain(offered_tlcs.iter_mut()) {
                tlc.fill_in_pubkeys(local, &self);
            }
            let (mut a, mut b) = if local {
                (received_tlcs, offered_tlcs)
            } else {
                for tlc in received_tlcs.iter_mut().chain(offered_tlcs.iter_mut()) {
                    // Need to flip these fields for the counterparty.
                    tlc.is_offered = !tlc.is_offered;
                    (tlc.local_payment_key_hash, tlc.remote_payment_key_hash) =
                        (tlc.remote_payment_key_hash, tlc.local_payment_key_hash);
                }
                (offered_tlcs, received_tlcs)
            };
            a.sort_by(|x, y| x.id.cmp(&y.id));
            b.sort_by(|x, y| x.id.cmp(&y.id));
            [a, b].concat()
        };

        debug!(
            "Get all tlcs for commitment tx #{} for {} party: {:?}",
            commitment_number,
            if local { "local" } else { "remote" },
            &tlcs
        );
        let witnesses: Vec<u8> = [
            (Since::from(delayed_epoch).value()).to_le_bytes().to_vec(),
            blake2b_256(delayed_payment_key.serialize())[0..20].to_vec(),
            blake2b_256(revocation_key.serialize())[0..20].to_vec(),
            tlcs.iter()
                .map(|tlc| tlc.serialize_to_lock_args())
                .flatten()
                .collect(),
        ]
        .map(|x| {
            debug!(
                "Witness element for commitment transaction #{} of {} party: {:?}",
                commitment_number,
                if local { "local" } else { "remote" },
                hex::encode(&x)
            );
            x
        })
        .concat();
        debug!(
            "Built commitment transaction #{}'s witnesses: {:?}",
            commitment_number,
            hex::encode(&witnesses)
        );
        witnesses
    }

    fn build_commitment_transaction_outputs(&self, local: bool) -> (Vec<CellOutput>, Vec<Bytes>) {
        let (time_locked_value, immediately_spendable_value) = if local {
            (self.to_local_amount, self.to_remote_amount)
        } else {
            (self.to_remote_amount, self.to_local_amount)
        };

        // The time_locked_value is amount of assets locked by commitment-lock.
        // We need also to include the total balance of all pending TLCs.
        let time_locked_value = time_locked_value as u64
            + self
                .get_all_tlcs()
                .iter()
                .map(|tlc| tlc.amount)
                .sum::<u128>() as u64;

        let immediate_payment_key = {
            let (commitment_point, base_payment_key) = if local {
                (
                    self.get_current_remote_commitment_point(),
                    self.get_remote_channel_parameters().payment_base_key(),
                )
            } else {
                (
                    self.get_current_local_commitment_point(),
                    self.get_local_channel_parameters().payment_base_key(),
                )
            };
            derive_payment_pubkey(base_payment_key, &commitment_point)
        };

        let witnesses: Vec<u8> = self.build_current_commitment_transaction_witnesses(local);

        let hash = blake2b_256(&witnesses);
        debug!(
            "Building {} commitment transaction with witnesses {:?} and hash {:?} with local commitment number {} and remote commitment number {}",
            if local { "local" } else {"remote"},
            hex::encode(&witnesses),
            hex::encode(&hash[..20]),
            self.local_commitment_number,
            self.remote_commitment_number
        );

        let immediate_secp256k1_lock_script = get_script_by_contract(
            Contract::Secp256k1Lock,
            &blake2b_256(immediate_payment_key.serialize())[0..20],
        );
        let commitment_lock_script =
            get_script_by_contract(Contract::CommitmentLock, &blake2b_256(witnesses)[0..20]);

        if let Some(udt_type_script) = &self.funding_udt_type_script {
            //FIXME(yukang): we need to add logic for transaction fee here
            let (time_locked_ckb_amount, immediately_spendable_ckb_amount) = if local {
                (self.local_ckb_amount, self.remote_ckb_amount)
            } else {
                (self.remote_ckb_amount, self.local_ckb_amount)
            };

            let immediate_output_data = immediately_spendable_value.to_le_bytes().pack();
            let immediate_output = CellOutput::new_builder()
                .lock(immediate_secp256k1_lock_script.clone())
                .type_(Some(udt_type_script.clone()).pack())
                .capacity(immediately_spendable_ckb_amount.pack())
                .build();

            let commitment_lock_output_data = time_locked_value.to_le_bytes().pack();
            let commitment_lock_output = CellOutput::new_builder()
                .lock(commitment_lock_script.clone())
                .type_(Some(udt_type_script.clone()).pack())
                .capacity(time_locked_ckb_amount.pack())
                .build();

            let outputs = vec![immediate_output, commitment_lock_output];
            let outputs_data = vec![immediate_output_data, commitment_lock_output_data];
            (outputs, outputs_data)
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
            (outputs, outputs_data)
        }
    }

    pub fn build_and_verify_commitment_tx(
        &self,
        signature: PartialSignature,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let verify_ctx = Musig2VerifyContext::from(self);

        let (tx, msg) = self.build_commitment_tx(false);
        debug!(
            "Verifying partial signature ({:?}) of commitment tx ({:?}) message {:?}",
            &signature,
            &tx,
            hex::encode(&msg)
        );
        verify_ctx.verify(signature, msg.as_slice())?;
        Ok(PartiallySignedCommitmentTransaction {
            msg,
            version: self.get_current_commitment_number(false),
            tx,
            signature,
        })
    }

    pub fn build_and_sign_commitment_tx(
        &self,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let sign_ctx = Musig2SignContext::from(self);

        let (tx, msg) = self.build_commitment_tx(true);

        debug!(
            "Signing commitment tx with message {:?}",
            hex::encode(msg.as_slice())
        );
        let signature = sign_ctx.sign(msg.as_slice())?;
        debug!(
            "Signed commitment tx ({:?}) message {:?} with signature {:?}",
            &tx,
            hex::encode(&msg),
            &signature,
        );

        Ok(PartiallySignedCommitmentTransaction {
            msg,
            tx,
            signature,
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

pub struct Musig2Context {
    pub key_agg_ctx: KeyAggContext,
    pub agg_nonce: AggNonce,
    pub local_seckey: Privkey,
    pub local_secnonce: SecNonce,
    pub remote_pubkey: Pubkey,
    pub remote_pubnonce: PubNonce,
}

impl Musig2Context {
    pub fn split(self) -> (Musig2SignContext, Musig2VerifyContext) {
        let Musig2Context {
            key_agg_ctx,
            agg_nonce,
            local_seckey,
            local_secnonce,
            remote_pubkey,
            remote_pubnonce,
        } = self;
        (
            Musig2SignContext {
                key_agg_ctx: key_agg_ctx.clone(),
                agg_nonce: agg_nonce.clone(),
                seckey: local_seckey,
                secnonce: local_secnonce,
            },
            Musig2VerifyContext {
                key_agg_ctx,
                agg_nonce,
                pubkey: remote_pubkey,
                pubnonce: remote_pubnonce,
            },
        )
    }
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

#[derive(Debug, Clone)]
pub struct Musig2SignContext {
    key_agg_ctx: KeyAggContext,
    agg_nonce: AggNonce,
    seckey: Privkey,
    secnonce: SecNonce,
}

impl Musig2SignContext {
    pub fn sign(self, message: &[u8]) -> Result<PartialSignature, ProcessingChannelError> {
        debug!("Musig2 signing partial message {:?}", hex::encode(&message));
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
        hex::encode(&message)
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
    /// The id of a received TLC. Must be empty if this is an offered HTLC.
    /// We will fill in the id when we send this tlc to the counterparty.
    /// Otherwise must be the next sequence number of the counterparty.
    pub id: u64,
    /// Is this HTLC being received by us or offered by us?
    pub is_offered: bool,
    /// The value as it appears in the commitment transaction
    pub amount: u128,
    /// The CLTV lock-time at which this HTLC expires.
    pub lock_time: LockTime,
    /// The hash of the preimage which unlocks this HTLC.
    pub payment_hash: Hash256,
    /// The preimage of the hash to be sent to the counterparty.
    pub payment_preimage: Option<Hash256>,
    /// The commitment number is kept here because we need to derive
    /// the local and remote payment pubkey. Note that commitment number
    /// is different for initiator and acceptor. So we keep both
    /// a local_commitment_number and a remote_commitment_number.
    pub local_commitment_number: u64,
    pub remote_commitment_number: u64,
    // Below are the actual public keys used in the commitment transaction.
    // These are derived from the channel base keys and per-commitment data.
    pub local_payment_key_hash: Option<ShortHash>,
    pub remote_payment_key_hash: Option<ShortHash>,
}

impl TLC {
    fn get_hash(&self) -> ShortHash {
        self.payment_hash.as_ref()[..20].try_into().unwrap()
    }

    fn fill_in_pubkeys(&mut self, local: bool, channel: &ChannelActorState) {
        debug!("Filling in pubkeys for tlc {:?}", self);
        self.local_payment_key_hash = Some(self.get_local_pubkey_hash(local, channel));
        self.remote_payment_key_hash = Some(self.get_remote_pubkey_hash(local, channel));
    }

    fn get_local_pubkey_hash(&self, local: bool, channel: &ChannelActorState) -> ShortHash {
        let pubkey = self.get_pubkey(true, local, channel);
        blake2b_256(pubkey.serialize())[..20].try_into().unwrap()
    }

    fn get_remote_pubkey_hash(&self, local: bool, channel: &ChannelActorState) -> ShortHash {
        let pubkey = self.get_pubkey(false, local, channel);
        blake2b_256(pubkey.serialize())[..20].try_into().unwrap()
    }

    fn get_pubkey(
        &self,
        local_pubkey: bool,
        local_commitment: bool,
        channel: &ChannelActorState,
    ) -> Pubkey {
        let commitment_number = if local_commitment {
            self.local_commitment_number
        } else {
            self.remote_commitment_number
        };
        let (base_key, commitment_point) = if local_pubkey {
            (
                channel.get_local_channel_parameters().pubkeys.tlc_base_key,
                channel.get_local_commitment_point(commitment_number),
            )
        } else {
            (
                channel.get_remote_channel_parameters().pubkeys.tlc_base_key,
                channel.get_remote_commitment_point(commitment_number),
            )
        };
        derive_tlc_pubkey(&base_key, &commitment_point)
    }

    // Serialize the tlc output to lock args.
    // Must be called after fill_in_pubkeys.
    pub fn serialize_to_lock_args(&self) -> Vec<u8> {
        [
            (if self.is_offered { [0] } else { [1] }).to_vec(),
            self.amount.to_le_bytes().to_vec(),
            self.get_hash().to_vec(),
            self.remote_payment_key_hash.unwrap().to_vec(),
            self.local_payment_key_hash.unwrap().to_vec(),
            Since::from(self.lock_time).value().to_le_bytes().to_vec(),
        ]
        .concat()
    }
}

/// A tlc output in a commitment transaction, including both the tlc output
/// and the commitment_number that it first appeared in the commitment transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TLCOutputInCommitment {
    pub output: TLC,
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

    pub fn derive_musig2_nonce(&self, _new_commitment_number: u64) -> SecNonce {
        // TODO: generate new musig nonce here
        self.musig2_base_nonce.clone()
    }
}

#[cfg(test)]
mod tests {

    use ckb_jsonrpc_types::Status;
    use ractor::call;

    use crate::{
        ckb::{
            network::{AcceptChannelCommand, OpenChannelCommand},
            test_utils::NetworkNode,
            NetworkActorCommand, NetworkActorMessage,
        },
        NetworkServiceEvent,
    };

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
                    funding_amount: 6100000000,
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
        let _ = env_logger::try_init();

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
                    funding_amount: 1000,
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
                    funding_amount: 6100000000,
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
