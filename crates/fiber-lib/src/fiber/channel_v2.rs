//! V2 commitment sessions, exact recovery replay and durable downstream effects.
use super::{
    blake2b_256, checked_calculate_commitment_tx_fee, checked_sub_u64, compute_tx_message,
    create_witness_for_funding_cell, is_tlc_key_derivation_safe, ChannelActor, ChannelActorMessage,
    ChannelActorState, ChannelActorStateStore, ProcessingChannelError, ProcessingChannelResult,
};
use crate::fiber::network::{
    FiberMessageWithTarget, NetworkActorCommand, NetworkActorMessage, NetworkServiceEvent,
};
use crate::fiber::session_v2::{allocate_nonce, bind_context, guarded_sign, SESSION_MARKER};
use crate::fiber::types::{
    ChannelReady, ChannelReadyV2, ClosingSignedV2, CommitmentSignedV2, FiberChannelMessage,
    FiberMessage, ReestablishChannelV2, RevokeAndAckV2, ShutdownV2, TxComplete, TxCompleteV2,
};
use crate::fiber::ASSUME_NETWORK_ACTOR_ALIVE;
use crate::invoice::{InvoiceStore, PreimageStore};
use ckb_types::{
    core::TransactionView,
    packed::{Bytes, CellOutput},
    prelude::{Builder, Entity, IntoTransactionView, Pack},
};
use fiber_types::{
    ChannelSessionV2, ChannelState, CollaboratingFundingTxFlags, IncomingCommitmentV2,
    NegotiatingFundingFlags, NoncePurposeV2, OutgoingCommitmentV2, RevocationData, SettlementData,
    SigningCommitmentFlags, INITIAL_COMMITMENT_NUMBER,
};
use musig2::{secp::Point, AggNonce, CompactSignature, PubNonce};
use ractor::ActorRef;

fn invalid(message: impl Into<String>) -> ProcessingChannelError {
    ProcessingChannelError::InvalidState(message.into())
}

impl ChannelActorState {
    pub(super) fn ack_peer_wait_v2(&self) -> Option<u64> {
        // Draining TLCs can still owe a real peer RAA. Only terminal local
        // publication/on-chain waits end this obligation, not shutdown ads.
        let can_wait = match self.state {
            ChannelState::ChannelReady => true,
            ChannelState::ShuttingDown(flags) => {
                !flags.contains(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
            }
            _ => false,
        };
        if !self.tlc_state.waiting_ack || !can_wait {
            return None;
        }
        Some(
            self.session_v2
                .as_ref()?
                .outgoing
                .as_ref()?
                .peer_wait_started_at,
        )
    }

    pub(super) fn peer_response_wait_v2(&self) -> Option<u64> {
        self.close_peer_wait_v2()
            .into_iter()
            .chain(self.ack_peer_wait_v2())
            .min()
    }

    // The retained records explicitly distinguish awaiting Shutdown, awaiting
    // ClosingSigned, local TLC draining, and awaiting on-chain confirmation.
    pub(super) fn close_peer_wait_v2(&self) -> Option<u64> {
        if !matches!(self.state, ChannelState::ShuttingDown(_)) {
            return None;
        }
        let close = self.session_v2.as_ref()?.closing.as_ref()?;
        if !close.superseded_by_force_close
            && (close.remote_shutdown.is_none()
                || (close.response.is_some() && close.remote_response.is_none()))
        {
            Some(close.peer_wait_started_at)
        } else {
            None
        }
    }

    pub(crate) fn ensure_v2_not_quarantined(&self) -> ProcessingChannelResult {
        if self.channel_features.is_v2() && self.state == ChannelState::Stale {
            return Err(invalid(
                "Backup-restored V2 channel is quarantined: freshness is unknown",
            ));
        }
        Ok(())
    }

    pub(super) fn send_reestablish_v2(&self) {
        self.send_v2_message(FiberMessage::reestablish_channel_v2(
            self.reestablish_message_v2(),
        ));
    }

    fn reestablish_message_v2(&self) -> ReestablishChannelV2 {
        let session = self.session_v2.as_ref().expect("V2 session validated");
        ReestablishChannelV2 {
            channel_id: self.id,
            next_commitment_number: session.own.number,
            next_ack_number: self.get_local_commitment_number(),
            next_local_commitment_nonce: session.own.public_nonce.clone(),
        }
    }

    fn validate_recovery_history_v2(&self) -> ProcessingChannelResult {
        if !matches!(self.state, ChannelState::ChannelReady | ChannelState::Stale) {
            return Ok(());
        }
        let session = self
            .session_v2
            .as_ref()
            .ok_or_else(|| invalid("Missing V2 recovery session"))?;
        let local = self.get_local_commitment_number();
        let remote = self.get_remote_commitment_number();
        if session.own.number != remote || session.remote_number != local {
            return Err(invalid("V2 nonce/counter history mismatch"));
        }
        let outgoing = session
            .outgoing
            .as_ref()
            .ok_or_else(|| invalid("Missing retained V2 outgoing history"))?;
        let sent = CommitmentSignedV2::try_from(outgoing.request.clone())
            .map_err(|e| invalid(e.to_string()))?;
        if sent.channel_id != self.id
            || if self.tlc_state.waiting_ack {
                sent.commitment_number != local
            } else {
                sent.commitment_number.checked_add(1) != Some(local)
            }
        {
            return Err(invalid("V2 outgoing/counter history mismatch"));
        }
        if local > INITIAL_COMMITMENT_NUMBER + 2 {
            let ack = session
                .last_ack
                .clone()
                .ok_or_else(|| invalid("Missing retained V2 accepted ACK"))?;
            let ack = RevokeAndAckV2::try_from(ack).map_err(|e| invalid(e.to_string()))?;
            if ack.channel_id != self.id || ack.commitment_number.checked_add(1) != Some(local) {
                return Err(invalid("V2 accepted ACK/counter history mismatch"));
            }
        }
        let incoming = session
            .incoming
            .as_ref()
            .ok_or_else(|| invalid("Missing usable V2 incoming history"))?;
        let accepted = CommitmentSignedV2::try_from(incoming.request.clone())
            .map_err(|e| invalid(e.to_string()))?;
        if accepted.channel_id != self.id
            || accepted.commitment_number.checked_add(1) != Some(remote)
        {
            return Err(invalid("V2 incoming/counter history mismatch"));
        }
        if remote > INITIAL_COMMITMENT_NUMBER + 2 {
            let ack = incoming
                .response
                .clone()
                .ok_or_else(|| invalid("Missing retained V2 response"))?;
            let ack = RevokeAndAckV2::try_from(ack).map_err(|e| invalid(e.to_string()))?;
            if ack.commitment_number != accepted.commitment_number
                || ack.next_commitment_nonce != session.own.public_nonce
            {
                return Err(invalid("V2 cached ACK/nonce history mismatch"));
            }
        }
        Ok(())
    }

    fn finish_recovery_v2(&mut self, myself: &ActorRef<ChannelActorMessage>) {
        if self.reestablishing
            && self.recovery_peer_v2.is_some()
            && !self.tlc_state.waiting_ack
            && matches!(self.state, ChannelState::ShuttingDown(_))
        {
            self.reestablishing = false;
            self.connectivity_state = fiber_types::ChannelConnectivityState::Online;
            self.reestablish_started_at = None;
            self.schedule_next_retry_task(myself);
            return;
        }
        if self.reestablishing
            && self.recovery_peer_v2.is_some()
            && !self.tlc_state.waiting_ack
            && self.state == ChannelState::ChannelReady
        {
            self.clear_waiting_peer_response();
            self.on_reestablished_channel_ready(myself);
            super::debug_event!(self.network(), "Reestablished channel in ChannelReady");
            self.schedule_next_retry_task(myself);
        }
    }

    pub(super) fn validate_session_v2(&self) -> ProcessingChannelResult {
        fiber_types::channel_v2_validation::validate_channel_v2(&self.core).map_err(invalid)
    }

    pub(super) fn initialize_session_v2(&mut self, remote_nonce: Option<PubNonce>) {
        if !self.channel_features.is_v2() {
            return;
        }
        self.session_v2 = Some(ChannelSessionV2 {
            marker: SESSION_MARKER,
            own: allocate_nonce(
                self.id,
                self.signer.funding_key.pubkey(),
                INITIAL_COMMITMENT_NUMBER + 1,
                NoncePurposeV2::Commitment,
                &self.signer.funding_key,
            ),
            bootstrap_remote_nonce: remote_nonce.clone(),
            remote_ready: None,
            remote_nonce,
            remote_number: INITIAL_COMMITMENT_NUMBER + 1,
            outgoing: None,
            incoming: None,
            pending_incoming: None,
            pending_ack: None,
            last_ack: None,
            revocation_effect: None,
            bootstrap_settlement: None,
            remove_effects: Vec::new(),
            closing: None,
        });
        self.last_committed_remote_nonce = None;
        self.remote_revocation_nonce_for_send = None;
        self.remote_revocation_nonce_for_verify = None;
        self.remote_revocation_nonce_for_next = None;
    }

    pub(super) fn opening_wire_message(&self, message: FiberMessage) -> FiberMessage {
        let Some(session) = &self.session_v2 else {
            return message;
        };
        match message {
            FiberMessage::ChannelInitialization(open) => {
                crate::fiber::session_v2::opening_message(open, session.own.public_nonce.clone())
            }
            FiberMessage::ChannelNormalOperation(FiberChannelMessage::AcceptChannel(accept)) => {
                crate::fiber::session_v2::accepting_message(
                    accept,
                    session.own.public_nonce.clone(),
                )
            }
            other => other,
        }
    }

    pub(super) fn tx_complete_wire_message(&self) -> FiberMessage {
        if let Some(session) = &self.session_v2 {
            FiberMessage::tx_complete_v2(TxCompleteV2 {
                channel_id: self.id,
                initial_commitment_nonce: session.own.public_nonce.clone(),
            })
        } else {
            FiberMessage::tx_complete(TxComplete {
                channel_id: self.id,
                next_commitment_nonce: self.get_next_commitment_nonce(),
            })
        }
    }

    pub(super) fn ready_wire_message(&mut self) -> ProcessingChannelResult<FiberMessage> {
        if self.session_v2.is_none() {
            return Ok(FiberMessage::channel_ready(ChannelReady {
                channel_id: self.id,
            }));
        }
        let mut session = self
            .session_v2
            .clone()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        if session.own.number == INITIAL_COMMITMENT_NUMBER + 1 {
            if session.incoming.is_none() {
                return Err(invalid("V2 Ready requires usable initial commitment"));
            }
            session.own = allocate_nonce(
                self.id,
                self.signer.funding_key.pubkey(),
                INITIAL_COMMITMENT_NUMBER + 2,
                NoncePurposeV2::Commitment,
                &self.signer.funding_key,
            );
        }
        let message = FiberMessage::channel_ready_v2(ChannelReadyV2 {
            channel_id: self.id,
            next_commitment_number: session.own.number,
            next_commitment_nonce: session.own.public_nonce.clone(),
        });
        self.session_v2 = Some(session);
        Ok(message)
    }

    fn revocation_message_v2(
        &self,
        for_remote: bool,
        new_number: u64,
    ) -> ProcessingChannelResult<([u8; 32], CellOutput, Bytes)> {
        let agg = self.get_musig2_agg_context(for_remote);
        let xonly = agg.aggregated_pubkey::<Point>().serialize_xonly();
        let fee = checked_calculate_commitment_tx_fee(
            self.commitment_fee_rate,
            &self.funding_udt_type_script,
            self.channel_features,
        )?;
        let lock = if for_remote {
            self.get_local_shutdown_script()
        } else {
            self.get_remote_shutdown_script()
        };
        let (output, data) = if let Some(udt) = &self.funding_udt_type_script {
            (
                CellOutput::new_builder()
                    .lock(lock)
                    .type_(Some(udt.clone()).pack())
                    .capacity(checked_sub_u64(
                        self.get_total_reserved_ckb_amount()?,
                        fee,
                        "Reserved CKB",
                    )?)
                    .build(),
                self.checked_liquid_capacity()?.to_le_bytes().pack(),
            )
        } else {
            (
                CellOutput::new_builder()
                    .lock(lock)
                    .capacity(checked_sub_u64(
                        self.get_total_ckb_amount()?,
                        fee,
                        "Total CKB",
                    )?)
                    .build(),
                Bytes::default(),
            )
        };
        let old = new_number
            .checked_sub(1)
            .ok_or_else(|| invalid("V2 revocation number underflow"))?;
        let args = [
            &blake2b_256(xonly)[..20],
            &self.get_delay_epoch_as_lock_args_bytes(),
            &old.to_be_bytes(),
        ]
        .concat();
        Ok((
            blake2b_256([output.as_slice(), data.as_slice(), &args].concat()),
            output,
            data,
        ))
    }

    fn revocation_context_v2(
        &self,
        for_remote: bool,
        number: u64,
    ) -> ProcessingChannelResult<fiber_types::RevocationContextV2> {
        let (message, output, output_data) = self.revocation_message_v2(for_remote, number)?;
        Ok(fiber_types::RevocationContextV2 {
            message,
            output,
            output_data,
        })
    }

    fn prepare_outgoing_v2(
        &mut self,
        initial: bool,
    ) -> ProcessingChannelResult<(CommitmentSignedV2, SettlementData)> {
        self.ensure_v2_not_quarantined()?;
        let mut session = self
            .session_v2
            .clone()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        let number = self.get_local_commitment_number();
        number
            .checked_add(1)
            .ok_or_else(|| invalid("V2 commitment number exhausted"))?;
        if session.remote_number != number {
            return Err(invalid("Remote V2 nonce number mismatch"));
        }
        if let Some(previous) = &session.outgoing {
            let old = CommitmentSignedV2::try_from(previous.request.clone())
                .map_err(|e| invalid(e.to_string()))?;
            if old.commitment_number == number {
                return Err(invalid("V2 CS already signed; replay exact cached request"));
            }
        }
        let remote = session
            .remote_nonce
            .clone()
            .ok_or_else(|| invalid("Missing remote V2 commitment nonce"))?;
        let (transaction, settlement) = self.build_commitment_tx_and_settlement_data(true)?;
        let mut funding = allocate_nonce(
            self.id,
            *self.get_remote_funding_pubkey(),
            number,
            NoncePurposeV2::Commitment,
            &self.signer.funding_key,
        );
        let keys = self.order_things_for_musig2(
            *self.get_local_funding_pubkey(),
            *self.get_remote_funding_pubkey(),
        );
        let agg = self.get_deterministic_musig2_agg_context();
        let nonce = AggNonce::sum([funding.public_nonce.clone(), remote]);
        let signature = guarded_sign(
            &mut funding,
            &self.signer.funding_key,
            keys,
            &agg,
            &nonce,
            &compute_tx_message(&transaction),
        )
        .map_err(invalid)?;
        let revocation = (!initial).then(|| {
            allocate_nonce(
                self.id,
                *self.get_remote_funding_pubkey(),
                number,
                NoncePurposeV2::Revocation,
                &self.signer.funding_key,
            )
        });
        let request = CommitmentSignedV2 {
            channel_id: self.id,
            commitment_number: number,
            funding_tx_partial_signature: signature,
            funding_nonce: funding.public_nonce.clone(),
            revocation_nonce: revocation.as_ref().map(|n| n.public_nonce.clone()),
        };
        session.outgoing = Some(OutgoingCommitmentV2 {
            peer_wait_started_at: crate::now_timestamp_as_millis_u64(),
            request: request.clone().into(),
            transaction: transaction.data(),
            settlement: settlement.clone(),
            updates: self.collect_pending_tlc_updates(),
            funding,
            revocation,
            revocation_context: if initial {
                None
            } else {
                Some(self.revocation_context_v2(true, number)?)
            },
        });
        self.session_v2 = Some(session);
        Ok((request, settlement))
    }

    fn stage_incoming_v2(
        &mut self,
        request: &CommitmentSignedV2,
        initial: bool,
    ) -> ProcessingChannelResult {
        self.ensure_v2_not_quarantined()?;
        let mut session = self
            .session_v2
            .clone()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        let packed: fiber_types::gen::fiber::CommitmentSignedV2 = request.clone().into();
        if let Some(pending) = &session.pending_incoming {
            if pending.request.as_slice() != packed.as_slice() {
                return Err(invalid("Conflicting pending incoming V2 CS"));
            }
            return Ok(());
        }
        request
            .commitment_number
            .checked_add(1)
            .ok_or_else(|| invalid("V2 commitment number exhausted"))?;
        if request.channel_id != self.id
            || request.commitment_number != self.get_remote_commitment_number()
            || request.commitment_number != session.own.number
            || request.revocation_nonce.is_none() != initial
        {
            return Err(invalid("Unexpected V2 incoming CS session/phase"));
        }
        let (transaction, settlement) = self.build_commitment_tx_and_settlement_data(false)?;
        let message = compute_tx_message(&transaction);
        let keys = self.order_things_for_musig2(
            *self.get_local_funding_pubkey(),
            *self.get_remote_funding_pubkey(),
        );
        let agg = self.get_deterministic_musig2_agg_context();
        let nonce = AggNonce::sum([
            session.own.public_nonce.clone(),
            request.funding_nonce.clone(),
        ]);
        musig2::verify_partial(
            &agg,
            request.funding_tx_partial_signature,
            &nonce,
            *self.get_remote_funding_pubkey(),
            &request.funding_nonce,
            message,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let mut funding = session.own.clone();
        bind_context(&mut funding, keys, &nonce, &message).map_err(invalid)?;
        if initial && session.bootstrap_settlement.is_none() {
            session.bootstrap_settlement = Some(settlement.clone());
        }
        session.pending_incoming = Some(IncomingCommitmentV2 {
            request: packed,
            funding,
            revocation: None,
            revocation_context: if initial {
                None
            } else {
                Some(self.revocation_context_v2(false, request.commitment_number)?)
            },
            transaction: transaction.data(),
            settlement,
            response: None,
        });
        self.session_v2 = Some(session);
        Ok(())
    }

    fn accept_commitment_v2(
        &mut self,
        request: &CommitmentSignedV2,
        initial: bool,
    ) -> ProcessingChannelResult<(TransactionView, SettlementData, Option<RevokeAndAckV2>)> {
        self.ensure_v2_not_quarantined()?;
        let mut session = self
            .session_v2
            .clone()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        let next_number = request
            .commitment_number
            .checked_add(1)
            .ok_or_else(|| invalid("V2 commitment number exhausted"))?;
        if request.channel_id != self.id
            || request.commitment_number != self.get_remote_commitment_number()
            || request.commitment_number != session.own.number
        {
            return Err(invalid("Unexpected incoming V2 commitment number/channel"));
        }
        if request.revocation_nonce.is_none() != initial {
            return Err(invalid(
                "V2 CS revocation nonce does not match opening/normal phase",
            ));
        }
        let pending = session
            .pending_incoming
            .as_ref()
            .ok_or_else(|| invalid("V2 incoming CS was not durably staged"))?;
        let packed: fiber_types::gen::fiber::CommitmentSignedV2 = request.clone().into();
        if pending.request.as_slice() != packed.as_slice() {
            return Err(invalid("Conflicting staged V2 CS"));
        }
        let tx = pending.transaction.clone().into_view();
        let settlement = pending.settlement.clone();
        let revocation_context = pending.revocation_context.clone();
        session.own = pending.funding.clone();
        let message = compute_tx_message(&tx);
        let keys = self.order_things_for_musig2(
            *self.get_local_funding_pubkey(),
            *self.get_remote_funding_pubkey(),
        );
        let agg = self.get_deterministic_musig2_agg_context();
        let nonce = AggNonce::sum([
            session.own.public_nonce.clone(),
            request.funding_nonce.clone(),
        ]);
        musig2::verify_partial(
            &agg,
            request.funding_tx_partial_signature,
            &nonce,
            *self.get_remote_funding_pubkey(),
            &request.funding_nonce,
            message,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let signature = guarded_sign(
            &mut session.own,
            &self.signer.funding_key,
            keys,
            &agg,
            &nonce,
            &message,
        )
        .map_err(invalid)?;
        let signature: CompactSignature = musig2::aggregate_partial_signatures(
            &agg,
            &nonce,
            [signature, request.funding_tx_partial_signature],
            message,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let witness =
            create_witness_for_funding_cell(self.get_funding_lock_script_xonly(), signature);
        let tx = tx
            .as_advanced_builder()
            .set_witnesses(vec![witness.pack()])
            .build();
        // Install usable commitment before creating any predecessor revocation.
        self.latest_commitment_transaction = Some(tx.data());
        let funding = session.own.clone();
        let mut revocation = None;
        let response = if let Some(remote_nonce) = &request.revocation_nonce {
            let mut local = allocate_nonce(
                self.id,
                *self.get_local_funding_pubkey(),
                request.commitment_number,
                NoncePurposeV2::Revocation,
                &self.signer.funding_key,
            );
            let keys = [
                *self.get_remote_funding_pubkey(),
                *self.get_local_funding_pubkey(),
            ];
            let agg = self.get_musig2_agg_context(false);
            let nonce = AggNonce::sum([local.public_nonce.clone(), remote_nonce.clone()]);
            let message = revocation_context
                .as_ref()
                .ok_or_else(|| invalid("Missing staged V2 revocation context"))?
                .message;
            let signature = guarded_sign(
                &mut local,
                &self.signer.funding_key,
                keys,
                &agg,
                &nonce,
                &message,
            )
            .map_err(invalid)?;
            self.increment_remote_commitment_number();
            session.own = allocate_nonce(
                self.id,
                *self.get_local_funding_pubkey(),
                next_number,
                NoncePurposeV2::Commitment,
                &self.signer.funding_key,
            );
            let response = RevokeAndAckV2 {
                channel_id: self.id,
                commitment_number: request.commitment_number,
                revocation_partial_signature: signature,
                revocation_nonce: local.public_nonce.clone(),
                next_per_commitment_point: self.get_current_local_commitment_point(),
                next_commitment_nonce: session.own.public_nonce.clone(),
            };
            revocation = Some(local);
            Some(response)
        } else {
            None
        };
        session.incoming = Some(IncomingCommitmentV2 {
            request: request.clone().into(),
            funding,
            revocation,
            revocation_context,
            transaction: tx.data(),
            settlement: settlement.clone(),
            response: response.clone().map(Into::into),
        });
        session.pending_incoming = None;
        self.session_v2 = Some(session);
        Ok((tx, settlement, response))
    }

    fn stage_ack_v2(&mut self, ack: &RevokeAndAckV2) -> ProcessingChannelResult {
        self.ensure_v2_not_quarantined()?;
        let mut session = self
            .session_v2
            .clone()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        let packed: fiber_types::gen::fiber::RevokeAndAckV2 = ack.clone().into();
        if let Some(last) = &session.last_ack {
            let old = RevokeAndAckV2::try_from(last.clone()).map_err(|e| invalid(e.to_string()))?;
            if old.commitment_number == ack.commitment_number {
                if last.as_slice() != packed.as_slice() {
                    return Err(invalid("Conflicting duplicate V2 ACK"));
                }
                return Ok(());
            }
        }
        if let Some(pending) = &session.pending_ack {
            if pending.as_slice() != packed.as_slice() {
                return Err(invalid("Conflicting pending V2 ACK"));
            }
            return Ok(());
        }
        if !self.tlc_state.waiting_ack
            || ack.channel_id != self.id
            || ack.commitment_number != self.get_local_commitment_number()
        {
            return Err(invalid("Unexpected V2 ACK number/channel"));
        }
        ack.commitment_number
            .checked_add(1)
            .ok_or_else(|| invalid("V2 commitment number exhausted"))?;
        if !is_tlc_key_derivation_safe(
            &self.get_remote_channel_public_keys().tlc_base_key,
            &ack.next_per_commitment_point,
        ) {
            return Err(invalid("Invalid V2 ACK commitment point"));
        }
        let outgoing = session
            .outgoing
            .as_mut()
            .ok_or_else(|| invalid("Missing persisted outgoing V2 CS"))?;
        let local = outgoing
            .revocation
            .as_mut()
            .ok_or_else(|| invalid("Initial V2 CS cannot receive ACK"))?;
        if local.number != ack.commitment_number {
            return Err(invalid("V2 revocation session mismatch"));
        }
        let keys = [
            *self.get_local_funding_pubkey(),
            *self.get_remote_funding_pubkey(),
        ];
        let agg = self.get_musig2_agg_context(true);
        let nonce = AggNonce::sum([local.public_nonce.clone(), ack.revocation_nonce.clone()]);
        let message = outgoing
            .revocation_context
            .as_ref()
            .ok_or_else(|| invalid("Missing original V2 ACK context"))?
            .message;
        musig2::verify_partial(
            &agg,
            ack.revocation_partial_signature,
            &nonce,
            *self.get_remote_funding_pubkey(),
            &ack.revocation_nonce,
            message,
        )
        .map_err(|e| invalid(e.to_string()))?;
        bind_context(local, keys, &nonce, &message).map_err(invalid)?;
        session.pending_ack = Some(packed);
        self.session_v2 = Some(session);
        Ok(())
    }

    fn accept_ack_v2(
        &mut self,
        myself: &ActorRef<ChannelActorMessage>,
        ack: &RevokeAndAckV2,
    ) -> ProcessingChannelResult<Option<(RevocationData, SettlementData)>> {
        self.ensure_v2_not_quarantined()?;
        let mut session = self
            .session_v2
            .clone()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        let packed: fiber_types::gen::fiber::RevokeAndAckV2 = ack.clone().into();
        if let Some(previous) = &session.last_ack {
            let old =
                RevokeAndAckV2::try_from(previous.clone()).map_err(|e| invalid(e.to_string()))?;
            if old.commitment_number == ack.commitment_number {
                if previous.as_slice() != packed.as_slice() {
                    return Err(invalid("Conflicting duplicate V2 ACK"));
                }
                return Ok(None);
            }
        }
        if !self.tlc_state.waiting_ack
            || ack.channel_id != self.id
            || ack.commitment_number != self.get_local_commitment_number()
        {
            return Err(invalid("Unexpected V2 ACK number/channel"));
        }
        if session.pending_ack.as_ref().map(Entity::as_slice) != Some(packed.as_slice()) {
            return Err(invalid("V2 ACK was not durably staged"));
        }
        if !is_tlc_key_derivation_safe(
            &self.get_remote_channel_public_keys().tlc_base_key,
            &ack.next_per_commitment_point,
        ) {
            return Err(invalid("Invalid V2 ACK commitment point"));
        }
        let outgoing = session
            .outgoing
            .as_mut()
            .ok_or_else(|| invalid("V2 ACK has no persisted outgoing CS"))?;
        let mut local = outgoing
            .revocation
            .clone()
            .ok_or_else(|| invalid("Initial V2 CS cannot receive RAA"))?;
        if local.number != ack.commitment_number {
            return Err(invalid("V2 revocation session mismatch"));
        }
        let keys = [
            *self.get_local_funding_pubkey(),
            *self.get_remote_funding_pubkey(),
        ];
        let agg = self.get_musig2_agg_context(true);
        let nonce = AggNonce::sum([local.public_nonce.clone(), ack.revocation_nonce.clone()]);
        let context = outgoing
            .revocation_context
            .clone()
            .ok_or_else(|| invalid("Missing original V2 ACK context"))?;
        let fiber_types::RevocationContextV2 {
            message,
            output,
            output_data,
        } = context;
        musig2::verify_partial(
            &agg,
            ack.revocation_partial_signature,
            &nonce,
            *self.get_remote_funding_pubkey(),
            &ack.revocation_nonce,
            message,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let signature = guarded_sign(
            &mut local,
            &self.signer.funding_key,
            keys,
            &agg,
            &nonce,
            &message,
        )
        .map_err(invalid)?;
        let aggregated_signature = musig2::aggregate_partial_signatures(
            &agg,
            &nonce,
            [signature, ack.revocation_partial_signature],
            message,
        )
        .map_err(|e| invalid(e.to_string()))?;
        outgoing.revocation = Some(local);
        let settlement = outgoing.settlement.clone();
        self.increment_local_commitment_number();
        self.append_remote_commitment_point(ack.next_per_commitment_point);
        let numbers = self.commitment_numbers;
        self.tlc_state.update_for_revoke_and_ack(numbers);
        self.set_waiting_ack(myself, false);
        session.remote_nonce = Some(ack.next_commitment_nonce.clone());
        session.remote_number = ack.commitment_number + 1;
        session.last_ack = Some(packed);
        session.pending_ack = None;
        let revocation = RevocationData {
            commitment_number: ack.commitment_number - 1,
            aggregated_signature,
            output,
            output_data,
        };
        session.revocation_effect = Some((revocation.clone(), settlement.clone()));
        self.session_v2 = Some(session);
        Ok(Some((revocation, settlement)))
    }
}

impl<S> ChannelActor<S>
where
    S: ChannelActorStateStore + InvoiceStore + PreimageStore,
{
    fn advertise_close_v2(
        &self,
        state: &mut ChannelActorState,
        script: ckb_types::packed::Script,
        fee_rate: u64,
        remote_shutdown: Option<fiber_types::gen::fiber::ShutdownV2>,
    ) -> ProcessingChannelResult {
        let own = allocate_nonce(
            state.id,
            state.signer.funding_key.pubkey(),
            0,
            NoncePurposeV2::Closing,
            &state.signer.funding_key,
        );
        let shutdown = ShutdownV2 {
            channel_id: state.id,
            close_script: script.clone(),
            fee_rate,
            closing_nonce: own.public_nonce.clone(),
        };
        state.local_shutdown_info = Some(fiber_types::ShutdownInfo {
            close_script: script,
            fee_rate,
            signature: None,
        });
        state.session_v2.as_mut().expect("V2").closing = Some(fiber_types::ClosingSessionV2 {
            peer_wait_started_at: crate::now_timestamp_as_millis_u64(),
            superseded_by_force_close: false,
            own,
            shutdown: shutdown.clone().into(),
            remote_shutdown: remote_shutdown.clone(),
            transaction: None,
            response: None,
            remote_response: None,
        });
        let flags = match state.state {
            ChannelState::ShuttingDown(flags) => flags,
            _ => fiber_types::ShuttingDownFlags::empty(),
        };
        state.update_state(ChannelState::ShuttingDown(
            flags
                | fiber_types::ShuttingDownFlags::OUR_SHUTDOWN_SENT
                | if remote_shutdown.is_some() {
                    fiber_types::ShuttingDownFlags::THEIR_SHUTDOWN_SENT
                } else {
                    fiber_types::ShuttingDownFlags::empty()
                },
        ));
        self.arm_peer_response_v2(state);
        self.store.insert_channel_actor_state(state.clone());
        state.send_v2_message(FiberMessage::shutdown_v2(shutdown));
        Ok(())
    }

    pub(super) fn arm_peer_response_v2(&self, state: &mut ChannelActorState) {
        if state.ensure_v2_not_quarantined().is_err() {
            return;
        }
        let Some(started) = state.peer_response_wait_v2() else {
            return;
        };
        state.waiting_peer_response = Some(
            state
                .waiting_peer_response
                .map_or(started, |old| old.min(started)),
        );
        let remaining = super::PEER_CHANNEL_RESPONSE_TIMEOUT
            .saturating_sub(crate::now_timestamp_as_millis_u64().saturating_sub(started));
        let channel_id = state.id;
        self.network
            .send_after(std::time::Duration::from_millis(remaining + 1), move || {
                NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
                    super::ChannelCommandWithId {
                        channel_id,
                        command: super::ChannelCommand::NotifyEvent(
                            super::ChannelEvent::CheckActiveChannel,
                        ),
                    },
                ))
            });
    }

    pub(super) async fn shutdown_v2(
        &self,
        state: &mut ChannelActorState,
        command: super::ShutdownCommand,
    ) -> ProcessingChannelResult {
        if state.connectivity_state != fiber_types::ChannelConnectivityState::Online {
            return Err(invalid(format!(
                "Cannot cooperatively shutdown channel {} while peer is offline",
                state.get_id()
            )));
        }
        if state.state != ChannelState::ChannelReady {
            return Err(invalid("V2 cooperative shutdown requires a ready channel"));
        }
        if state.tlc_state.all_tlcs().any(|tlc| {
            matches!(
                tlc.status,
                fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::LocalAnnounced)
            )
        }) {
            return Err(invalid("Pending outbound TLCs prevent shutdown"));
        }
        let script = command
            .close_script
            .unwrap_or_else(|| state.get_local_shutdown_script());
        let fee = command
            .fee_rate
            .unwrap_or(ckb_types::core::FeeRate::from_u64(
                state.commitment_fee_rate,
            ));
        state.check_shutdown_fee_rate(fee, &script)?;
        self.advertise_close_v2(state, script, fee.as_u64(), None)
    }

    pub(super) async fn receive_shutdown_v2(
        &self,
        state: &mut ChannelActorState,
        message: ShutdownV2,
    ) -> ProcessingChannelResult {
        state.ensure_v2_not_quarantined()?;
        if message.channel_id != state.id {
            return Err(invalid("V2 shutdown channel mismatch"));
        }
        let packed: fiber_types::gen::fiber::ShutdownV2 = message.clone().into();
        if let Some(close) = state.session_v2.as_ref().and_then(|s| s.closing.as_ref()) {
            if let Some(old) = &close.remote_shutdown {
                if old.as_slice() != packed.as_slice() {
                    return Err(invalid("Conflicting V2 shutdown"));
                }
                if let Some(response) = &close.response {
                    state.send_v2_message(FiberMessage::closing_signed_v2(
                        ClosingSignedV2::try_from(response.clone())
                            .map_err(|e| invalid(e.to_string()))?,
                    ));
                }
                return Ok(());
            }
        }
        if !matches!(
            state.state,
            ChannelState::ChannelReady | ChannelState::ShuttingDown(_)
        ) || !state.check_shutdown_fee_valid(&message.close_script, message.fee_rate)
        {
            return Err(invalid("Invalid V2 shutdown state or fee"));
        }
        if state.tlc_state.all_tlcs().any(|tlc| {
            matches!(
                tlc.status,
                fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::RemoteAnnounced)
            )
        }) {
            return Err(invalid("Pending inbound TLCs prevent shutdown"));
        }
        state.remote_shutdown_info = Some(fiber_types::ShutdownInfo {
            close_script: message.close_script,
            fee_rate: message.fee_rate,
            signature: None,
        });
        if state.session_v2.as_ref().expect("V2").closing.is_none() {
            if !state.check_valid_to_auto_accept_shutdown() {
                return Err(invalid(
                    "V2 shutdown fee does not allow automatic acceptance",
                ));
            }
            self.advertise_close_v2(
                state,
                state.get_local_shutdown_script(),
                0,
                Some(packed.clone()),
            )?;
        }
        state
            .session_v2
            .as_mut()
            .expect("V2")
            .closing
            .as_mut()
            .expect("closing")
            .remote_shutdown = Some(packed);
        state.update_state(ChannelState::ShuttingDown(
            fiber_types::ShuttingDownFlags::AWAITING_PENDING_TLCS,
        ));
        self.store.insert_channel_actor_state(state.clone());
        self.progress_close_v2(state).await
    }

    pub(super) async fn progress_close_v2(
        &self,
        state: &mut ChannelActorState,
    ) -> ProcessingChannelResult {
        state.ensure_v2_not_quarantined()?;
        if !matches!(state.state, ChannelState::ShuttingDown(flags) if flags.contains(fiber_types::ShuttingDownFlags::AWAITING_PENDING_TLCS) || (flags == fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION && state.session_v2.as_ref().and_then(|s| s.closing.as_ref()).is_some_and(|c| c.remote_response.is_some())))
            || state.any_tlc_pending()
            || state.reestablishing
        {
            return Ok(());
        }
        let Some(close) = state.session_v2.as_ref().and_then(|s| s.closing.as_ref()) else {
            return Ok(());
        };
        if close.superseded_by_force_close
            || state.tlc_state.waiting_ack
            || state.tlc_state.all_tlcs().any(|tlc| {
                !tlc.applied_flags
                    .contains(fiber_types::AppliedFlags::REMOVE)
            })
        {
            return Ok(());
        }
        let Some(remote) = close.remote_shutdown.clone() else {
            return Ok(());
        };
        let remote = ShutdownV2::try_from(remote).map_err(|e| invalid(e.to_string()))?;
        let tx = match &close.transaction {
            Some(tx) => tx.clone().into_view(),
            None => state.build_shutdown_tx().await?,
        };
        let keys = state.order_things_for_musig2(
            state.signer.funding_key.pubkey(),
            *state.get_remote_funding_pubkey(),
        );
        let aggregate = musig2::KeyAggContext::new(keys).map_err(|e| invalid(e.to_string()))?;
        let nonce = AggNonce::sum([close.own.public_nonce.clone(), remote.closing_nonce]);
        let message = compute_tx_message(&tx);
        let key = state.signer.funding_key.clone();
        let channel_id = state.id;
        state.update_state(ChannelState::ShuttingDown(
            fiber_types::ShuttingDownFlags::AWAITING_PENDING_TLCS
                | fiber_types::ShuttingDownFlags::DROPPING_PENDING,
        ));
        let close = state
            .session_v2
            .as_mut()
            .expect("V2")
            .closing
            .as_mut()
            .expect("closing");
        close.transaction = Some(tx.data());
        bind_context(&mut close.own, keys, &nonce, &message).map_err(invalid)?;
        self.store.insert_channel_actor_state(state.clone());
        let close = state
            .session_v2
            .as_mut()
            .expect("V2")
            .closing
            .as_mut()
            .expect("closing");
        let signature = guarded_sign(&mut close.own, &key, keys, &aggregate, &nonce, &message)
            .map_err(invalid)?;
        let first = close.response.is_none();
        let response = ClosingSignedV2 {
            channel_id,
            partial_signature: signature,
        };
        close.response = Some(response.clone().into());
        let remote_response = close.remote_response.clone();
        state
            .local_shutdown_info
            .as_mut()
            .expect("shutdown")
            .signature = Some(signature);
        self.store.insert_channel_actor_state(state.clone());
        if first {
            self.arm_peer_response_v2(state);
            state.send_v2_message(FiberMessage::closing_signed_v2(response));
        }
        if let Some(remote_response) = remote_response {
            let remote =
                ClosingSignedV2::try_from(remote_response).map_err(|e| invalid(e.to_string()))?;
            let sig: CompactSignature = musig2::aggregate_partial_signatures(
                &aggregate,
                &nonce,
                [signature, remote.partial_signature],
                message,
            )
            .map_err(|e| invalid(e.to_string()))?;
            let witness =
                create_witness_for_funding_cell(state.get_funding_lock_script_xonly(), sig);
            let tx = tx
                .as_advanced_builder()
                .set_witnesses(vec![witness.pack()])
                .build();
            state.update_state(ChannelState::ShuttingDown(
                fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
            ));
            state.clear_waiting_peer_response();
            self.store.insert_channel_actor_state(state.clone());
            state
                .network()
                .send_message(NetworkActorMessage::new_event(
                    crate::fiber::network::NetworkActorEvent::ClosingTransactionPending(
                        state.id,
                        state.remote_pubkey,
                        tx,
                        false,
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
        Ok(())
    }

    pub(super) async fn receive_closing_v2(
        &self,
        state: &mut ChannelActorState,
        message: ClosingSignedV2,
    ) -> ProcessingChannelResult {
        state.ensure_v2_not_quarantined()?;
        if message.channel_id != state.id {
            return Err(invalid("V2 closing channel mismatch"));
        }
        let close = state
            .session_v2
            .as_ref()
            .and_then(|s| s.closing.as_ref())
            .ok_or_else(|| invalid("V2 closing without shutdown"))?;
        let packed: fiber_types::gen::fiber::ClosingSignedV2 = message.clone().into();
        if let Some(old) = &close.remote_response {
            if old.as_slice() != packed.as_slice() {
                return Err(invalid("Conflicting V2 closing signature"));
            }
            return Ok(());
        }
        let tx = close
            .transaction
            .as_ref()
            .ok_or_else(|| invalid("V2 closing before fixed transaction"))?
            .clone()
            .into_view();
        let remote = ShutdownV2::try_from(
            close
                .remote_shutdown
                .clone()
                .ok_or_else(|| invalid("Missing remote closing nonce"))?,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let keys = state.order_things_for_musig2(
            state.signer.funding_key.pubkey(),
            *state.get_remote_funding_pubkey(),
        );
        let aggregate = musig2::KeyAggContext::new(keys).map_err(|e| invalid(e.to_string()))?;
        let nonce = AggNonce::sum([close.own.public_nonce.clone(), remote.closing_nonce.clone()]);
        musig2::verify_partial(
            &aggregate,
            message.partial_signature,
            &nonce,
            *state.get_remote_funding_pubkey(),
            &remote.closing_nonce,
            compute_tx_message(&tx),
        )
        .map_err(|e| invalid(e.to_string()))?;
        state
            .session_v2
            .as_mut()
            .expect("V2")
            .closing
            .as_mut()
            .expect("closing")
            .remote_response = Some(packed);
        state
            .remote_shutdown_info
            .as_mut()
            .expect("shutdown")
            .signature = Some(message.partial_signature);
        self.store.insert_channel_actor_state(state.clone());
        self.progress_close_v2(state).await
    }

    pub(super) async fn reestablish_v2(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        peer: ReestablishChannelV2,
    ) -> ProcessingChannelResult {
        // A valid old image cannot reveal post-backup nonce use or revocations.
        // Matching peer counters/nonces are untrusted assertions, not freshness proof.
        state.ensure_v2_not_quarantined()?;
        if !state.reestablishing {
            // A delayed identical handshake must not rewind a live session.
            return Ok(());
        }
        if let Some(previous) = &state.recovery_peer_v2 {
            if previous != &peer {
                return Err(invalid(
                    "Conflicting V2 handshake for the active connection",
                ));
            }
            return Ok(());
        }
        state.on_peer_reconnected();
        myself.send_after(
            std::time::Duration::from_millis(super::REESTABLISH_TIMEOUT + 1),
            || ChannelActorMessage::Event(super::ChannelEvent::CheckActiveChannel),
        );
        state.validate_recovery_history_v2()?;
        state.notify_funding_tx(&self.network);
        let session = state
            .session_v2
            .as_ref()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        if peer.next_commitment_number == session.remote_number
            && session.remote_nonce.as_ref() != Some(&peer.next_local_commitment_nonce)
        {
            return Err(invalid("Peer changed persisted V2 verification nonce"));
        }
        if matches!(
            state.state,
            ChannelState::NegotiatingFunding(_)
                | ChannelState::CollaboratingFundingTx(_)
                | ChannelState::SigningCommitment(_)
        ) {
            let external = state.ephemeral_config.external_funding.enabled;
            let has_signing_session = session.outgoing.is_some()
                || session.incoming.is_some()
                || session.pending_incoming.is_some();
            if !external && !has_signing_session {
                myself
                    .send_message(ChannelActorMessage::Event(super::ChannelEvent::Stop(
                        super::StopReason::AbortFunding,
                    )))
                    .expect("myself alive");
                return Ok(());
            }
            if !(INITIAL_COMMITMENT_NUMBER + 1..=INITIAL_COMMITMENT_NUMBER + 2)
                .contains(&peer.next_commitment_number)
                || !(INITIAL_COMMITMENT_NUMBER + 1..=INITIAL_COMMITMENT_NUMBER + 2)
                    .contains(&peer.next_ack_number)
            {
                return Err(invalid(
                    "Incompatible V2 external funding bootstrap history",
                ));
            }
            state.recovery_peer_v2 = Some(peer);
            // Bootstrap funding flow is unlocked only for this validated handshake.
            // Keep the recovery watchdog active until actual ChannelReady.
            self.replay_effects_v2(state);
            if external && !state.ephemeral_config.external_funding.signed_submitted {
                // Waiting for local funding submission owes no peer response.
                state.reestablishing = false;
                return Ok(());
            }
            if let (true, Some(tx)) = (external, state.funding_tx.clone()) {
                state.send_v2_message(FiberMessage::tx_update(crate::fiber::types::TxUpdate {
                    channel_id: state.id,
                    tx,
                }));
            }
            if let Some(pending) = state
                .session_v2
                .as_ref()
                .and_then(|s| s.pending_incoming.clone())
            {
                self.receive_commitment_v2(
                    myself,
                    state,
                    CommitmentSignedV2::try_from(pending.request)
                        .map_err(|e| invalid(e.to_string()))?,
                )
                .await?;
            }
            if let Some(outgoing) = state.session_v2.as_ref().and_then(|s| s.outgoing.clone()) {
                state.send_v2_message(FiberMessage::commitment_signed_v2(
                    CommitmentSignedV2::try_from(outgoing.request)
                        .map_err(|e| invalid(e.to_string()))?,
                ));
                if let ChannelState::SigningCommitment(flags) = state.state {
                    state.maybe_transfer_to_tx_signatures(flags)?;
                }
            } else {
                // No partial signature exists: allocating the first session is
                // permitted. Existing signed sessions above are replayed verbatim.
                myself
                    .send_message(ChannelActorMessage::Command(
                        super::ChannelCommand::CommitmentSigned(None),
                    ))
                    .expect("myself alive");
            }
            return Ok(());
        }
        if matches!(
            state.state,
            ChannelState::AwaitingTxSignatures(_) | ChannelState::AwaitingChannelReady(_)
        ) {
            if !(INITIAL_COMMITMENT_NUMBER + 1..=INITIAL_COMMITMENT_NUMBER + 2)
                .contains(&peer.next_commitment_number)
                || !(INITIAL_COMMITMENT_NUMBER + 1..=INITIAL_COMMITMENT_NUMBER + 2)
                    .contains(&peer.next_ack_number)
            {
                return Err(invalid("Incompatible V2 funding Ready history"));
            }
            let replay_initial = peer.next_commitment_number == INITIAL_COMMITMENT_NUMBER + 1;
            state.recovery_peer_v2 = Some(peer);
            self.replay_effects_v2(state);
            if replay_initial {
                let outgoing = state
                    .session_v2
                    .as_ref()
                    .and_then(|s| s.outgoing.clone())
                    .ok_or_else(|| invalid("Missing V2 initial CS replay"))?;
                state.send_v2_message(FiberMessage::commitment_signed_v2(
                    CommitmentSignedV2::try_from(outgoing.request)
                        .map_err(|e| invalid(e.to_string()))?,
                ));
            }
            state.resume_funding(myself);
            return Ok(());
        }
        let local = state.get_local_commitment_number();
        let remote = state.get_remote_commitment_number();
        let session = state
            .session_v2
            .as_ref()
            .ok_or_else(|| invalid("Missing V2 session"))?;
        // Ready advances opening commitment 1 to normal commitment 2 without
        // RAA. The one-step bootstrap gap must replay Ready, never an initial ACK.
        if state.state == ChannelState::ChannelReady
            && local == INITIAL_COMMITMENT_NUMBER + 2
            && remote == local
            && peer.next_commitment_number == local
            && peer.next_ack_number == INITIAL_COMMITMENT_NUMBER + 1
            && !state.tlc_state.waiting_ack
            && state.tlc_state.all_tlcs().next().is_none()
            && state.funding_tx_confirmed_at.is_some()
        {
            if session.remote_nonce.as_ref() != Some(&peer.next_local_commitment_nonce) {
                return Err(invalid("Peer changed V2 bootstrap Ready nonce"));
            }
            state.recovery_peer_v2 = Some(peer);
            self.replay_effects_v2(state);
            let ready = state.ready_wire_message()?;
            self.store.insert_channel_actor_state(state.clone());
            state.send_v2_message(ready);
            state.finish_recovery_v2(myself);
            super::debug_event!(self.network, "Replayed ChannelReady after reestablishment");
            return Ok(());
        }
        let replay_cs = if state.tlc_state.waiting_ack {
            if peer.next_commitment_number == local {
                true
            } else if local.checked_add(1) == Some(peer.next_commitment_number) {
                false
            } else {
                return Err(invalid("Incompatible V2 incoming CS history"));
            }
        } else {
            if peer.next_commitment_number != local {
                return Err(invalid("Incompatible V2 completed outgoing history"));
            }
            false
        };
        if peer.next_commitment_number == session.remote_number
            && session.remote_nonce.as_ref() != Some(&peer.next_local_commitment_nonce)
        {
            return Err(invalid("Peer changed persisted V2 verification nonce"));
        }
        let replay_ack = if peer.next_ack_number == remote {
            false
        } else if peer.next_ack_number.checked_add(1) == Some(remote) {
            true
        } else {
            return Err(invalid("Incompatible V2 incoming ACK history"));
        };
        let outgoing = session.outgoing.clone();
        let incoming = session.incoming.clone();
        if state.tlc_state.waiting_ack {
            let outgoing = outgoing
                .as_ref()
                .ok_or_else(|| invalid("Missing outstanding V2 CS"))?;
            let request = CommitmentSignedV2::try_from(outgoing.request.clone())
                .map_err(|e| invalid(e.to_string()))?;
            if request.commitment_number != local {
                return Err(invalid("Outstanding V2 CS counter mismatch"));
            }
        }
        let ack = if replay_ack {
            let incoming = incoming
                .as_ref()
                .ok_or_else(|| invalid("Missing accepted V2 CS"))?;
            let response = incoming
                .response
                .clone()
                .ok_or_else(|| invalid("Missing cached V2 ACK"))?;
            let ack = RevokeAndAckV2::try_from(response).map_err(|e| invalid(e.to_string()))?;
            if ack.commitment_number != peer.next_ack_number {
                return Err(invalid("Cached V2 ACK counter mismatch"));
            }
            Some(ack)
        } else {
            None
        };
        state.recovery_peer_v2 = Some(peer);
        self.replay_effects_v2(state);
        self.update_tlc_status_on_ack(myself, state).await;
        self.apply_settled_remove_tlcs(state, true).await;
        self.replay_remove_effects_v2(state);
        if !matches!(state.state, ChannelState::ShuttingDown(_)) {
            state.set_waiting_peer_response();
        }
        self.arm_peer_response_v2(state);
        // Replay the two independent directions in their original relative order.
        let send_cs = || -> ProcessingChannelResult {
            if replay_cs {
                let outgoing = outgoing
                    .as_ref()
                    .ok_or_else(|| invalid("Missing outgoing replay"))?;
                for update in &outgoing.updates {
                    state.send_v2_message(match update {
                        fiber_types::TlcReplayUpdate::Add(add) => {
                            FiberMessage::add_tlc(add.clone())
                        }
                        fiber_types::TlcReplayUpdate::Remove(remove) => {
                            FiberMessage::remove_tlc(remove.clone())
                        }
                    });
                    match update {
                        fiber_types::TlcReplayUpdate::Add(_) => {
                            super::debug_event!(self.network, "resend add tlc");
                        }
                        fiber_types::TlcReplayUpdate::Remove(_) => {
                            super::debug_event!(self.network, "resend remove tlc");
                        }
                    }
                }
                state.send_v2_message(FiberMessage::commitment_signed_v2(
                    CommitmentSignedV2::try_from(outgoing.request.clone())
                        .map_err(|e| invalid(e.to_string()))?,
                ));
            }
            Ok(())
        };
        if state.last_was_revoke {
            send_cs()?;
        }
        if let Some(ack) = ack {
            state.send_v2_message(FiberMessage::revoke_and_ack_v2(ack));
        }
        if !state.last_was_revoke {
            send_cs()?;
        }
        let pending_incoming = state
            .session_v2
            .as_ref()
            .and_then(|s| s.pending_incoming.clone());
        let pending_ack = state
            .session_v2
            .as_ref()
            .and_then(|s| s.pending_ack.clone());
        if let Some(pending) = pending_incoming {
            self.receive_commitment_v2(
                myself,
                state,
                CommitmentSignedV2::try_from(pending.request)
                    .map_err(|e| invalid(e.to_string()))?,
            )
            .await?;
        }
        if let Some(pending) = pending_ack {
            self.receive_ack_v2(
                myself,
                state,
                RevokeAndAckV2::try_from(pending).map_err(|e| invalid(e.to_string()))?,
            )
            .await?;
        }
        state.finish_recovery_v2(myself);
        if let Some(close) = state.session_v2.as_ref().and_then(|s| s.closing.as_ref()) {
            if !close.superseded_by_force_close {
                state.send_v2_message(FiberMessage::shutdown_v2(
                    ShutdownV2::try_from(close.shutdown.clone())
                        .map_err(|e| invalid(e.to_string()))?,
                ));
                if let Some(response) = &close.response {
                    state.send_v2_message(FiberMessage::closing_signed_v2(
                        ClosingSignedV2::try_from(response.clone())
                            .map_err(|e| invalid(e.to_string()))?,
                    ));
                }
            }
        }
        if !state.reestablishing {
            state.resend_tlcs_on_reestablish(true)?;
        }
        self.progress_close_v2(state).await
    }

    pub(super) fn replay_effects_v2(&self, state: &ChannelActorState) {
        let session = state.session_v2.as_ref().expect("validated V2 session");
        if let Some(settlement) = &session.bootstrap_settlement {
            let (local_key, remote_key) = state.get_settlement_keys();
            self.network
                .send_message(NetworkActorMessage::new_notification(
                    NetworkServiceEvent::RemoteTxComplete(
                        state.remote_pubkey,
                        state.id,
                        state.funding_udt_type_script.clone(),
                        local_key,
                        remote_key,
                        *state.get_local_funding_pubkey(),
                        *state.get_remote_funding_pubkey(),
                        settlement.clone(),
                        state.channel_features,
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
        // Each slot is a latest-value snapshot, not an edge-triggered effect. Do
        // not erase a slot after enqueue: NetworkActor's observer queue is volatile.
        // Replay accepted revocation before the newer pending remote settlement.
        if let Some((revocation, settlement)) = &session.revocation_effect {
            self.network
                .send_message(NetworkActorMessage::new_notification(
                    NetworkServiceEvent::RevokeAndAckReceived(
                        state.remote_pubkey,
                        state.id,
                        revocation.clone(),
                        settlement.clone(),
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
        if let Some(outgoing) = &session.outgoing {
            self.network
                .send_message(NetworkActorMessage::new_notification(
                    NetworkServiceEvent::LocalCommitmentSigned(
                        state.id,
                        outgoing.settlement.clone(),
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
        if let Some(incoming) = &session.incoming {
            self.network
                .send_message(NetworkActorMessage::new_notification(
                    NetworkServiceEvent::RemoteCommitmentSigned(
                        state.remote_pubkey,
                        state.id,
                        incoming.transaction.clone().into_view(),
                        incoming.settlement.clone(),
                    ),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
    }

    pub(super) fn replay_remove_effects_v2(&self, state: &ChannelActorState) {
        if let Some(session) = &state.session_v2 {
            for (tlc, execution) in &session.remove_effects {
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::RelayV2RemoveEffect {
                            channel_id: state.id,
                            tlc: Box::new(tlc.clone()),
                            execution: *execution,
                        },
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            }
        }
    }

    pub(super) async fn send_commitment_v2(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
    ) -> ProcessingChannelResult {
        state.ensure_v2_not_quarantined()?;
        let recovering_bootstrap = state.recovery_peer_v2.is_some()
            && matches!(
                state.state,
                ChannelState::CollaboratingFundingTx(_) | ChannelState::SigningCommitment(_)
            );
        if state.reestablishing && !recovering_bootstrap {
            return Err(invalid("V2 signing waits for reestablishment obligations"));
        }
        if state.tlc_state.waiting_ack {
            return Ok(());
        }
        let flags = match state.state {
            ChannelState::CollaboratingFundingTx(flags)
                if flags.contains(CollaboratingFundingTxFlags::COLLABORATION_COMPLETED) =>
            {
                Some(SigningCommitmentFlags::empty())
            }
            ChannelState::SigningCommitment(flags)
                if !flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT) =>
            {
                Some(flags)
            }
            ChannelState::ChannelReady => None,
            ChannelState::ShuttingDown(flags) if flags.is_ok_for_commitment_operation() => None,
            _ => return Err(invalid("V2 CS command in invalid phase")),
        };
        state.clean_up_failed_tlcs();
        let (request, settlement) = state.prepare_outgoing_v2(flags.is_some())?;
        if let Some(flags) = flags {
            state.update_state(ChannelState::SigningCommitment(
                flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT,
            ));
        } else {
            state.set_waiting_ack(myself, true);
        }
        state.last_was_revoke = false;
        self.store.insert_channel_actor_state(state.clone());
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::LocalCommitmentSigned(state.id, settlement),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        state.send_v2_message(FiberMessage::commitment_signed_v2(request));
        if let ChannelState::SigningCommitment(flags) = state.state {
            state.maybe_transfer_to_tx_signatures(flags)?;
        }
        Ok(())
    }

    pub(super) async fn receive_commitment_v2(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        request: CommitmentSignedV2,
    ) -> ProcessingChannelResult {
        state.ensure_v2_not_quarantined()?;
        let session = state
            .session_v2
            .as_ref()
            .ok_or_else(|| invalid("V2 CS on legacy channel"))?;
        if let Some(previous) = &session.incoming {
            let old = CommitmentSignedV2::try_from(previous.request.clone())
                .map_err(|e| invalid(e.to_string()))?;
            if old.commitment_number == request.commitment_number {
                let packed: fiber_types::gen::fiber::CommitmentSignedV2 = request.clone().into();
                if previous.request.as_slice() != packed.as_slice() {
                    return Err(invalid("Conflicting duplicate V2 CS"));
                }
                if let Some(response) = &previous.response {
                    state.send_v2_message(FiberMessage::revoke_and_ack_v2(
                        RevokeAndAckV2::try_from(response.clone())
                            .map_err(|e| invalid(e.to_string()))?,
                    ));
                }
                return Ok(());
            }
        }
        let flags = match state.state {
            ChannelState::CollaboratingFundingTx(flags)
                if flags.contains(CollaboratingFundingTxFlags::COLLABORATION_COMPLETED) =>
            {
                Some(SigningCommitmentFlags::empty())
            }
            ChannelState::SigningCommitment(flags)
                if !flags.contains(SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT) =>
            {
                Some(flags)
            }
            ChannelState::NegotiatingFunding(flags)
                if flags.contains(NegotiatingFundingFlags::AWAITING_EXTERNAL_FUNDING) =>
            {
                Some(SigningCommitmentFlags::empty())
            }
            ChannelState::ChannelReady => None,
            ChannelState::ShuttingDown(flags) if flags.is_ok_for_commitment_operation() => None,
            _ => return Err(invalid("V2 CS received in invalid phase")),
        };
        if state.defer_peer_tlc_updates {
            state.stop_defer_peer_tlc_updates();
            self.flush_deferred_peer_tlc_updates(state)?;
        }
        state.stage_incoming_v2(&request, flags.is_some())?;
        self.store.insert_channel_actor_state(state.clone());
        let (transaction, settlement, response) =
            state.accept_commitment_v2(&request, flags.is_some())?;
        if let Some(flags) = flags {
            state.update_state(ChannelState::SigningCommitment(
                flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT,
            ));
        }
        let needs_cs = state.tlc_state.update_for_commitment_signed();
        state.last_was_revoke = response.is_some();
        self.store.insert_channel_actor_state(state.clone());
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::RemoteCommitmentSigned(
                    state.remote_pubkey,
                    state.id,
                    transaction,
                    settlement,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        if let Some(response) = response {
            state.send_v2_message(FiberMessage::revoke_and_ack_v2(response));
        }
        if let ChannelState::SigningCommitment(flags) = state.state {
            state.maybe_transfer_to_tx_signatures(flags)?;
        }
        self.apply_settled_remove_tlcs(state, true).await;
        let external_reply = state.ephemeral_config.external_funding.signed_submitted
            && matches!(state.state, ChannelState::SigningCommitment(flags) if !flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT));
        state.finish_recovery_v2(myself);
        if (needs_cs || external_reply)
            && !state.tlc_state.waiting_ack
            && (!state.reestablishing || (flags.is_some() && state.recovery_peer_v2.is_some()))
        {
            self.send_commitment_v2(myself, state).await?;
        }
        Ok(())
    }

    pub(super) async fn receive_ack_v2(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        ack: RevokeAndAckV2,
    ) -> ProcessingChannelResult {
        state.ensure_v2_not_quarantined()?;
        if state.reestablishing
            && state.recovery_peer_v2.as_ref().is_some_and(|peer| {
                ack.commitment_number.checked_add(1) == Some(peer.next_commitment_number)
                    && ack.next_commitment_nonce != peer.next_local_commitment_nonce
            })
        {
            return Err(invalid(
                "V2 ACK contradicts persisted peer nonce advertisement",
            ));
        }
        state.stage_ack_v2(&ack)?;
        self.store.insert_channel_actor_state(state.clone());
        let Some((revocation, settlement)) = state.accept_ack_v2(myself, &ack)? else {
            return Ok(());
        };
        self.store.insert_channel_actor_state(state.clone());
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::RevokeAndAckReceived(
                    state.remote_pubkey,
                    state.id,
                    revocation,
                    settlement,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        self.update_tlc_status_on_ack(myself, state).await;
        state.finish_recovery_v2(myself);
        if state.tlc_state.need_another_commitment_signed() {
            self.send_commitment_v2(myself, state).await?;
        }
        if !state.is_waiting_tlc_ack() {
            self.apply_retryable_tlc_operations(myself, state, false)
                .await;
        }
        Ok(())
    }
}

impl ChannelActorState {
    fn send_v2_message(&self, message: FiberMessage) {
        self.network()
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithTarget::new(
                    self.remote_pubkey,
                    message,
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fiber::graph::NetworkGraphStateStore;
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
                super::super::ChannelCommandWithId {
                    command: super::super::ChannelCommand::TestBarrier(reply),
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
                super::super::ChannelCommandWithId {
                    channel_id: id,
                    command: super::super::ChannelCommand::TestBarrier(reply),
                }
            )),
            5000
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_v2_storage_codec_defers_corrupt_intent_to_preflight_and_restore() {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut channel = a.get_channel_actor_state(id);
        channel
            .validate_session_v2()
            .expect("genuine funded channel");
        a.stop().await;
        b.stop().await;

        // Complete, correctly versioned bytes can contain a corrupt signing
        // intent. Reading them must not panic before the owning boundary checks it.
        channel.session_v2.as_mut().unwrap().own.seed[0] ^= 1;
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
        assert!(restored.validate_session_v2().is_err());

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
            super::super::ChannelInitializationParameter {
                operation: super::super::ChannelInitializationOperation::RestoreOfflineChannel(id),
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
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut local = a.get_channel_actor_state(id);
        let mut peer = b.get_channel_actor_state(id);
        let old_peer = peer.clone();
        let backed_up_nonce = local.session_v2.as_ref().unwrap().own.public_nonce.clone();
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
            let (_, _, ack) = local.accept_commitment_v2(&request, false).unwrap();
            local.tlc_state.update_for_commitment_signed();
            assert_eq!(
                local
                    .session_v2
                    .as_ref()
                    .unwrap()
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
            peer.accept_ack_v2(&myself, &ack.unwrap()).unwrap();
            assert!(peer
                .session_v2
                .as_ref()
                .unwrap()
                .revocation_effect
                .is_some());
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
        assert_eq!(
            stale.session_v2.as_ref().unwrap().own.public_nonce,
            backed_up_nonce
        );
        assert!(stale.session_v2.as_ref().unwrap().own.context.is_none());
        let before = bincode::serialize(&stale.session_v2).unwrap();
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
                    super::super::ShutdownCommand {
                        force,
                        close_script: None,
                        fee_rate: None
                    }
                )
                .await
                .is_err());
        }
        stale.reestablish_started_at = Some(0);
        stale.waiting_peer_response = Some(0);
        actor
            .handle_event(
                &myself,
                &mut stale,
                super::super::ChannelEvent::CheckActiveChannel,
            )
            .await
            .unwrap();
        assert!(stale.get_latest_commitment_transaction().await.is_err());
        assert_eq!(bincode::serialize(&stale.session_v2).unwrap(), before);
        capture_barrier(&network, id).await;
        network.stop(None);
        task.await.unwrap();
        assert!(!captured.lock().unwrap().iter().any(|m| matches!(
            m,
            NetworkActorMessage::Event(
                crate::fiber::network::NetworkActorEvent::ClosingTransactionPending(..)
            ) | NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(_))
                | NetworkActorMessage::Event(
                    crate::fiber::network::NetworkActorEvent::ChannelReady(..)
                )
                | NetworkActorMessage::Notification(
                    NetworkServiceEvent::ChannelOnline(..) | NetworkServiceEvent::ChannelReady(..)
                )
        )));
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
                let (_, _, ack) = state.accept_commitment_v2(&request, false).unwrap();
                state.tlc_state.update_for_commitment_signed();
                peer.stage_ack_v2(&ack.clone().unwrap()).unwrap();
                peer.accept_ack_v2(&myself, &ack.unwrap()).unwrap();
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
                        super::super::ShutdownCommand {
                            force: false,
                            close_script: None,
                            fee_rate: None,
                        },
                    )
                    .await
                    .unwrap();
                if closing {
                    let shutdown = ShutdownV2::try_from(
                        state
                            .session_v2
                            .as_ref()
                            .unwrap()
                            .closing
                            .as_ref()
                            .unwrap()
                            .shutdown
                            .clone(),
                    )
                    .unwrap();
                    remote
                        .receive_shutdown_v2(&mut peer, shutdown)
                        .await
                        .unwrap();
                    let reply = ShutdownV2::try_from(
                        peer.session_v2
                            .as_ref()
                            .unwrap()
                            .closing
                            .as_ref()
                            .unwrap()
                            .shutdown
                            .clone(),
                    )
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
                state
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .closing
                    .as_mut()
                    .unwrap()
                    .peer_wait_started_at = 0;
                if reconnect {
                    let encoded = bincode::serialize(&state.core).unwrap();
                    state.core = bincode::deserialize(&encoded).unwrap();
                    state.waiting_peer_response = None;
                    state.mark_reestablishing_offline();
                    actor
                        .reestablish_v2(&myself, &mut state, peer.reestablish_message_v2())
                        .await
                        .unwrap();
                    assert!(!state.reestablishing);
                    assert_eq!(
                        state.waiting_peer_response,
                        Some(0),
                        "reconnect must not extend the deadline"
                    );
                }
                assert!(
                    state.peer_does_not_reply_ack_in_time(),
                    "unanswered close must have an effective deadline"
                );
                actor
                    .handle_event(
                        &myself,
                        &mut state,
                        super::super::ChannelEvent::CheckActiveChannel,
                    )
                    .await
                    .unwrap();
                capture_barrier(&network, id).await;
                let shutdown = {
                    let mut messages = captured.lock().unwrap();
                    let index = messages.iter().position(|m| matches!(m,
                        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                            super::super::ChannelCommandWithId { channel_id, command: super::super::ChannelCommand::Shutdown(command, _) }
                        )) if *channel_id == id && command.force
                    )).expect("CheckActiveChannel must request force close, not merely schedule another check");
                    let NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                        super::super::ChannelCommandWithId {
                            command: super::super::ChannelCommand::Shutdown(command, _),
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

    async fn assert_offline_close_restart_rearms(
        closing: bool,
        expectation: RestartCloseExpectation,
    ) {
        use crate::fiber::network::TestFiberMessageKind;
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let predecessor = a
            .get_channel_actor_state(id)
            .latest_commitment_transaction
            .clone()
            .unwrap();
        let payment = a.send_payment_keysend(&b, 1000, false).await.unwrap();
        a.wait_until_success(payment.payment_hash).await;
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
            super::super::ShutdownCommand {
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
            .session_v2
            .as_ref()
            .and_then(|s| s.closing.as_ref())
            .and_then(|c| c.response.clone());
        let mut retained = a.get_channel_actor_state(id);
        let close = retained
            .session_v2
            .as_mut()
            .unwrap()
            .closing
            .as_mut()
            .unwrap();
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
                let close = retained
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .closing
                    .as_mut()
                    .unwrap();
                close.response = None;
                close.own.signature = None;
                retained.local_shutdown_info.as_mut().unwrap().signature = None;
                assert!(retained.close_peer_wait_v2().is_none());
                assert!(retained.ack_peer_wait_v2().is_none());
                fiber_types::channel_v2_validation::validate_channel_v2(&retained.core).unwrap();
            }
            RestartCloseExpectation::Publish => {}
        }
        let intent = bincode::serialize(&retained.session_v2.as_ref().unwrap().closing).unwrap();
        // Store the original intent after stopping so handler-end persistence
        // cannot overwrite the expired durable deadline. No old actor/timer survives.
        a.store.insert_channel_actor_state(retained);
        a.start().await;
        if matches!(expectation, RestartCloseExpectation::Publish) {
            assert_timer_publishes_usable(&mut a, id, &usable, std::time::Duration::from_secs(5))
                .await;
        } else {
            tokio::time::sleep(
                crate::fiber::network::CHECK_CHANNELS_INTERVAL
                    + std::time::Duration::from_millis(200),
            )
            .await;
            assert!(a
                .get_transaction_view_from_hash(usable.hash().into())
                .await
                .is_none());
        }
        let mut after = a
            .get_channel_actor_state(id)
            .session_v2
            .as_ref()
            .unwrap()
            .closing
            .clone()
            .unwrap();
        assert_eq!(
            after.superseded_by_force_close,
            matches!(expectation, RestartCloseExpectation::Publish)
        );
        after.superseded_by_force_close = false;
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
        a.wait_until_success(payment.payment_hash).await;
        let usable = a
            .get_channel_actor_state(id)
            .get_latest_commitment_transaction()
            .await
            .unwrap();
        b.hold_next_fiber_messages(a.pubkey, id, TestFiberMessageKind::RevokeAndAck, 1)
            .await;
        a.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ControlFiberChannel(super::super::ChannelCommandWithId {
                    channel_id: id,
                    command: super::super::ChannelCommand::CommitmentSigned(None),
                }),
            ))
            .unwrap();
        b.wait_for_held_fiber_messages(1).await;
        b.hold_next_fiber_messages(a.pubkey, id, TestFiberMessageKind::ClosingSigned, 1)
            .await;
        assert!(a.get_channel_actor_state(id).tlc_state.waiting_ack);
        a.send_shutdown_command_to_channel(
            id,
            super::super::ShutdownCommand {
                force: false,
                close_script: None,
                fee_rate: None,
            },
        )
        .await;
        b.wait_for_held_fiber_messages(2).await;
        let mut retained = a.get_channel_actor_state(id);
        let close = retained
            .session_v2
            .as_ref()
            .unwrap()
            .closing
            .as_ref()
            .unwrap();
        assert!(close.remote_shutdown.is_some());
        assert!(close.response.is_none());
        assert!(retained.tlc_state.waiting_ack);
        assert!(
            retained.close_peer_wait_v2().is_none(),
            "this is an ACK obligation, not a closing-signature wait"
        );
        let outgoing = bincode::serialize(&retained.session_v2.as_ref().unwrap().outgoing).unwrap();
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
            |reply| ChannelActorMessage::Command(super::super::ChannelCommand::TestBarrier(reply)),
            5000
        )
        .unwrap();
        assert_eq!(
            bincode::serialize(
                &a.get_channel_actor_state(id)
                    .session_v2
                    .as_ref()
                    .unwrap()
                    .outgoing
            )
            .unwrap(),
            outgoing
        );
        if restart {
            b.stop().await;
            a.stop().await;
            retained.reestablishing = true;
            a.store.insert_channel_actor_state(retained);
            a.start().await;
        }
        assert_timer_publishes_usable(
            &mut a,
            id,
            &usable,
            std::time::Duration::from_millis(super::super::PEER_CHANNEL_RESPONSE_TIMEOUT + 5000),
        )
        .await;
        assert_eq!(
            bincode::serialize(
                &a.get_channel_actor_state(id)
                    .session_v2
                    .as_ref()
                    .unwrap()
                    .outgoing
            )
            .unwrap(),
            outgoing
        );
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
            super::super::ShutdownCommand {
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
            .session_v2
            .as_ref()
            .unwrap()
            .closing
            .clone()
            .unwrap();
        let before_b = b
            .get_channel_actor_state(id)
            .session_v2
            .as_ref()
            .unwrap()
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
            let after = state.session_v2.as_ref().unwrap().closing.as_ref().unwrap();
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
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sa = a.get_channel_actor_state(id);
        let mut sb = b.get_channel_actor_state(id);
        let myself = a.get_channel_actor(id).await.unwrap();
        let (request, _) = sa.prepare_outgoing_v2(false).unwrap();
        sa.tlc_state.waiting_ack = true;
        sb.stage_incoming_v2(&request, false).unwrap();
        let (_, _, ack) = sb.accept_commitment_v2(&request, false).unwrap();
        sb.tlc_state.update_for_commitment_signed();
        let ack = ack.unwrap();
        sa.stage_ack_v2(&ack).unwrap();
        sa.accept_ack_v2(&myself, &ack).unwrap();
        let usable = sa.latest_commitment_transaction.clone().unwrap();
        let old_nonce = sa.session_v2.as_ref().unwrap().own.public_nonce.clone();
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
                super::super::ShutdownCommand {
                    force: false,
                    close_script: None,
                    fee_rate: None,
                },
            )
            .await
            .unwrap();
        let shutdown = ShutdownV2::try_from(
            sa.session_v2
                .as_ref()
                .unwrap()
                .closing
                .as_ref()
                .unwrap()
                .shutdown
                .clone(),
        )
        .unwrap();
        assert_ne!(shutdown.closing_nonce, old_nonce);
        assert_ne!(
            shutdown.closing_nonce,
            sa.session_v2
                .as_ref()
                .unwrap()
                .outgoing
                .as_ref()
                .unwrap()
                .funding
                .public_nonce
        );
        actor_b
            .receive_shutdown_v2(&mut sb, shutdown.clone())
            .await
            .unwrap();
        let peer_shutdown = ShutdownV2::try_from(
            sb.session_v2
                .as_ref()
                .unwrap()
                .closing
                .as_ref()
                .unwrap()
                .shutdown
                .clone(),
        )
        .unwrap();
        actor_a
            .receive_shutdown_v2(&mut sa, peer_shutdown.clone())
            .await
            .unwrap();
        let original = sa.session_v2.as_ref().unwrap().closing.clone().unwrap();
        let bytes = bincode::serialize(&sa.core).unwrap();
        sa.core = bincode::deserialize(&bytes).unwrap();
        fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();
        // A crash after the durable binding but before signing is legitimate.
        // Recovery must complete that same context, producing the original response.
        {
            let pending = sa.session_v2.as_mut().unwrap().closing.as_mut().unwrap();
            pending.own.signature = None;
            pending.response = None;
        }
        sa.local_shutdown_info.as_mut().unwrap().signature = None;
        fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();
        actor_a.progress_close_v2(&mut sa).await.unwrap();
        assert_eq!(
            sa.session_v2
                .as_ref()
                .unwrap()
                .closing
                .as_ref()
                .unwrap()
                .own
                .signature,
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
            sa.session_v2
                .as_ref()
                .unwrap()
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
            sb.session_v2
                .as_ref()
                .unwrap()
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
        sa.session_v2
            .as_mut()
            .unwrap()
            .closing
            .as_mut()
            .unwrap()
            .peer_wait_started_at = 0;
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
            let session = bad.session_v2.as_mut().unwrap();
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
    async fn test_v2_recovery_handshake_precedes_replay_and_watchdog_bounds_wait() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let myself = a.get_channel_actor(id).await.unwrap();
        let own = state.session_v2.as_ref().unwrap().own.public_nonce.clone();
        let (request, _) = state.prepare_outgoing_v2(false).unwrap();
        let ack_deadline_start = state
            .session_v2
            .as_ref()
            .unwrap()
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
        assert_eq!(state.session_v2.as_ref().unwrap().own.public_nonce, own);
        actor
            .handle_peer_message(
                &myself,
                &mut state,
                FiberChannelMessage::ReestablishChannelV2(incoming.reestablish_message_v2()),
            )
            .await
            .unwrap();
        assert!(state.reestablishing);
        assert!(state.waiting_peer_response.is_some());
        assert_eq!(state.ack_peer_wait_v2(), Some(ack_deadline_start));
        actor
            .handle_peer_message(
                &myself,
                &mut state,
                FiberChannelMessage::ReestablishChannelV2(incoming.reestablish_message_v2()),
            )
            .await
            .unwrap();
        assert_eq!(
            state.ack_peer_wait_v2(),
            Some(ack_deadline_start),
            "duplicate reconciliation must not replace the outstanding ACK deadline"
        );
        assert_eq!(state.session_v2.as_ref().unwrap().own.public_nonce, own);
        state.reestablish_started_at = Some(
            crate::now_timestamp_as_millis_u64()
                .saturating_sub(super::super::REESTABLISH_TIMEOUT + 1),
        );
        actor
            .handle_event(
                &myself,
                &mut state,
                super::super::ChannelEvent::CheckActiveChannel,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if captured.lock().unwrap().iter().any(|m| {
                    matches!(
                        m,
                        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(_))
                    )
                }) {
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
            assert!(matches!(
                wire[0],
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ReestablishChannelV2(_))
            ));
            let FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                replay,
            )) = wire[1]
            else {
                panic!("expected exact CS after handshake");
            };
            let packed: fiber_types::PackedCommitmentSignedV2 = replay.clone().into();
            let original: fiber_types::PackedCommitmentSignedV2 = request.into();
            assert_eq!(packed.as_slice(), original.as_slice());
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
            state.accept_commitment_v2(&incoming, false).unwrap();
            if !cs_first {
                state.prepare_outgoing_v2(false).unwrap();
            }
            state.tlc_state.waiting_ack = true;
            state.last_was_revoke = cs_first;
            let original_cs = state
                .session_v2
                .as_ref()
                .unwrap()
                .outgoing
                .as_ref()
                .unwrap()
                .request
                .clone();
            let original_ack = state
                .session_v2
                .as_ref()
                .unwrap()
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
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
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
                                NetworkActorMessage::Command(
                                    NetworkActorCommand::SendFiberMessage(_)
                                )
                            )
                        })
                        .count()
                        == 3
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
                let cs_index = if cs_first { 1 } else { 2 };
                let ack_index = if cs_first { 2 } else { 1 };
                let FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                    cs,
                )) = wire[cs_index]
                else {
                    panic!("CS replay order changed");
                };
                let FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(ack)) =
                    wire[ack_index]
                else {
                    panic!("RAA replay order changed");
                };
                let cs: fiber_types::PackedCommitmentSignedV2 = cs.clone().into();
                let ack: fiber_types::PackedRevokeAndAckV2 = ack.clone().into();
                assert_eq!(cs.as_slice(), original_cs.as_slice());
                assert_eq!(ack.as_slice(), original_ack.as_slice());
            }
            assert!(state.reestablishing && state.tlc_state.waiting_ack);
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
            .session_v2
            .as_mut()
            .unwrap()
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
                        .session_v2
                        .as_ref()
                        .unwrap()
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
        let (_, _, ack) = owner.accept_commitment_v2(&request, false).unwrap();
        owner.tlc_state.update_for_commitment_signed();
        let ack = ack.unwrap();
        sender.stage_ack_v2(&ack).unwrap();
        let (revocation, _) = sender.accept_ack_v2(&myself, &ack).unwrap().unwrap();
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
        let (_, _, ack) = owner.accept_commitment_v2(&request, false).unwrap();
        assert!(!owner.tlc_state.update_for_commitment_signed());
        let original_ack: fiber_types::PackedRevokeAndAckV2 = ack.unwrap().into();
        let own = owner.session_v2.as_ref().unwrap().own.public_nonce.clone();
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
            sender
                .session_v2
                .as_ref()
                .unwrap()
                .last_ack
                .as_ref()
                .unwrap()
                .as_slice(),
            original_ack.as_slice()
        );
        assert_eq!(owner.session_v2.as_ref().unwrap().own.public_nonce, own);
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
    async fn test_v2_recovery_crash_with_unsent_cs() {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        b.stop().await;
        a.stop().await;
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.tlc_state.waiting_ack = true;
        let original = sender
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .clone()
            .unwrap();
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
        let replayed = state
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .as_ref()
            .unwrap();
        assert_eq!(replayed.request.as_slice(), original.request.as_slice());
        assert_eq!(
            replayed.transaction.as_slice(),
            original.transaction.as_slice()
        );
        assert_eq!(replayed.funding.context, original.funding.context);
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
                .session_v2
                .as_ref()
                .unwrap()
                .pending_incoming
                .as_ref()
                .unwrap()
                .transaction
                .clone();
            if crash_at_ack {
                let (_, _, ack) = owner.accept_commitment_v2(&request, false).unwrap();
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
            assert!(a_state.session_v2.as_ref().unwrap().pending_ack.is_none());
            assert!(b_state
                .session_v2
                .as_ref()
                .unwrap()
                .pending_incoming
                .is_none());
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
    async fn test_v2_pending_aggregation_context_survives_restart() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut owner = b.get_channel_actor_state(id);
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        owner.stage_incoming_v2(&request, false).unwrap();
        let bytes = bincode::serialize(&owner).unwrap();
        let mut restored: ChannelActorState = bincode::deserialize(&bytes).unwrap();
        let pending = restored
            .session_v2
            .as_ref()
            .unwrap()
            .pending_incoming
            .as_ref()
            .unwrap();
        assert!(pending.funding.context.is_some());
        assert!(pending.funding.signature.is_none());
        let mut conflicting = request.clone();
        conflicting.revocation_nonce = Some(request.funding_nonce.clone());
        assert!(restored.stage_incoming_v2(&conflicting, false).is_err());
        let (_, _, response) = restored.accept_commitment_v2(&request, false).unwrap();
        let response = response.unwrap();
        sender.tlc_state.waiting_ack = true;
        sender.stage_ack_v2(&response).unwrap();
        let bytes = bincode::serialize(&sender).unwrap();
        let mut restored_sender: ChannelActorState = bincode::deserialize(&bytes).unwrap();
        let record = restored_sender
            .session_v2
            .as_ref()
            .unwrap()
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
        restored_sender.validate_session_v2().unwrap();
        restored_sender.session_v2.as_mut().unwrap().marker = 0;
        assert!(restored_sender.validate_session_v2().is_err());
    }
}
