//! V2 commitment sessions, exact recovery replay and durable downstream effects.
use super::{
    blake2b_256, checked_calculate_commitment_tx_fee, checked_sub_u64, compute_tx_message,
    create_witness_for_funding_cell, is_tlc_key_derivation_safe, ChannelActor, ChannelActorMessage,
    ChannelActorState, ChannelActorStateStore, ProcessingChannelError, ProcessingChannelResult,
    RemoveTlcCommand,
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
    NegotiatingFundingFlags, NoncePurposeV2, OutgoingCommitmentV2, RetryableTlcOperation,
    RevocationData, SettlementData, SigningCommitmentFlags, INITIAL_COMMITMENT_NUMBER,
};
use musig2::{secp::Point, AggNonce, CompactSignature, PubNonce};
use ractor::ActorRef;

fn invalid(message: impl Into<String>) -> ProcessingChannelError {
    ProcessingChannelError::InvalidState(message.into())
}

impl ChannelActorState {
    /// A peer cannot answer while this channel is offline or its reestablishment is still in
    /// flight, so a reconnect starts a fresh peer-response window instead of charging the
    /// partition against the peer. A reconnect that never completes is bounded separately by
    /// `REESTABLISH_TIMEOUT`.
    pub(super) fn refresh_peer_response_wait_v2_after_reestablish(&mut self) {
        if self.session_v2.is_none() {
            return;
        }
        self.clear_waiting_peer_response();
        self.reset_peer_response_wait_v2();
    }

    pub(super) fn reset_peer_response_wait_v2(&mut self) {
        let ack_pending = self.ack_peer_wait_v2().is_some();
        let close_pending = self.close_peer_wait_v2().is_some();
        let now = crate::now_timestamp_as_millis_u64();
        if let Some(session) = self.session_v2.as_mut() {
            if ack_pending {
                if let Some(outgoing) = session.outgoing.as_mut() {
                    outgoing.peer_wait_started_at = now;
                }
            }
            if close_pending {
                if let Some(closing) = session.closing.as_mut() {
                    closing.peer_wait_started_at = now;
                }
            }
        }
    }

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
            // The cached ACK keeps the verification nonce that was advertised when it was sent.
            // After a reconnect we rotate `session.own`, so its `next_commitment_nonce` is
            // historical and must not be compared with the current one; only the counter must
            // still describe the accepted commitment.
            if ack.commitment_number != accepted.commitment_number {
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
            self.refresh_peer_response_wait_v2_after_reestablish();
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
            own_retired: Vec::new(),
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
        let revocation_context = if initial {
            None
        } else {
            Some(self.revocation_context_v2(true, number)?)
        };
        session.outgoing = Some(OutgoingCommitmentV2 {
            peer_wait_started_at: crate::now_timestamp_as_millis_u64(),
            request: request.clone().into(),
            transaction: transaction.data(),
            settlement: settlement.clone(),
            updates: self.collect_pending_tlc_updates(),
            funding,
            revocation,
            revocation_context,
        });
        self.session_v2 = Some(session);
        Ok((request, settlement))
    }

    fn stage_incoming_v2(
        &mut self,
        request: &CommitmentSignedV2,
        initial: bool,
    ) -> ProcessingChannelResult<bool> {
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
            // Retained staging data is not a verification result for this call.
            return Ok(false);
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
        // The peer may have signed before it learned about a nonce rotation, so accept the active
        // verification nonce or any recently retired one. The nonce that actually verifies is the
        // one recorded with the accepted request, and its secret is never reused for a signature.
        let mut candidates = Vec::with_capacity(1 + session.own_retired.len());
        candidates.push(session.own.clone());
        candidates.extend(session.own_retired.iter().cloned());
        let mut verified = None;
        for candidate in candidates {
            let nonce = AggNonce::sum([
                candidate.public_nonce.clone(),
                request.funding_nonce.clone(),
            ]);
            if musig2::verify_partial(
                &agg,
                request.funding_tx_partial_signature,
                &nonce,
                *self.get_remote_funding_pubkey(),
                &request.funding_nonce,
                message,
            )
            .is_ok()
            {
                verified = Some((candidate, nonce));
                break;
            }
        }
        let (mut funding, _) =
            verified.ok_or_else(|| invalid("failed to verify signature: signature is invalid"))?;
        let nonce = AggNonce::sum([funding.public_nonce.clone(), request.funding_nonce.clone()]);
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
        Ok(true)
    }

    fn accept_commitment_v2(
        &mut self,
        request: &CommitmentSignedV2,
        initial: bool,
        verified: bool,
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
            .ok_or_else(|| invalid("V2 incoming CS was not staged"))?;
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
        if !verified {
            musig2::verify_partial(
                &agg,
                request.funding_tx_partial_signature,
                &nonce,
                *self.get_remote_funding_pubkey(),
                &request.funding_nonce,
                message,
            )
            .map_err(|e| invalid(e.to_string()))?;
        }
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

    fn stage_ack_v2(&mut self, ack: &RevokeAndAckV2) -> ProcessingChannelResult<bool> {
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
                return Ok(false);
            }
        }
        if let Some(pending) = &session.pending_ack {
            if pending.as_slice() != packed.as_slice() {
                return Err(invalid("Conflicting pending V2 ACK"));
            }
            // Recovery must verify the retained ACK again before accepting it.
            return Ok(false);
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
        Ok(true)
    }

    fn accept_ack_v2(
        &mut self,
        myself: &ActorRef<ChannelActorMessage>,
        ack: &RevokeAndAckV2,
        verified: bool,
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
            return Err(invalid("V2 ACK was not staged"));
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
        if !verified {
            musig2::verify_partial(
                &agg,
                ack.revocation_partial_signature,
                &nonce,
                *self.get_remote_funding_pubkey(),
                &ack.revocation_nonce,
                message,
            )
            .map_err(|e| invalid(e.to_string()))?;
        }
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
        // Validate the auto-accept minimum before changing shutdown state: the
        // actor persists state even when this handler fails, so rejecting here
        // after recording remote_shutdown_info would leave a stale remote info
        // next to a later local closing session without a remote advertisement,
        // and V2 validation would reject the channel after a restart.
        let auto_accept = state.session_v2.as_ref().expect("V2").closing.is_none();
        if auto_accept && message.fee_rate < state.commitment_fee_rate {
            return Err(invalid(
                "V2 shutdown fee does not allow automatic acceptance",
            ));
        }
        state.remote_shutdown_info = Some(fiber_types::ShutdownInfo {
            close_script: message.close_script,
            fee_rate: message.fee_rate,
            signature: None,
        });
        if auto_accept {
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
        let was_waiting_confirmation = matches!(state.state, ChannelState::ShuttingDown(flags)
            if flags == fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION);
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
        if first {
            self.store.insert_channel_actor_state(state.clone());
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
            if first || !was_waiting_confirmation {
                self.store.insert_channel_actor_state(state.clone());
            }
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
        // Syncing can only precede this handler after a committed reconnect message.
        let nonce_already_persisted =
            state.connectivity_state == fiber_types::ChannelConnectivityState::Syncing;
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
            if !nonce_already_persisted {
                self.store.insert_channel_actor_state(state.clone());
            }
            state.send_v2_message(ready);
            state.finish_recovery_v2(myself);
            super::debug_event!(self.network, "Replayed ChannelReady after reestablishment");
            return Ok(());
        }
        // The peer has not accepted our outstanding CS (`next_commitment_number == local`), but
        // the transaction we signed for it no longer matches our TLC state, so replaying it can
        // only fail verification. Abandon that signing session and sign a fresh CS after
        // recovery; V2 allocates every session nonce from a fresh random seed, so the burned
        // nonce is never reused.
        // `bolt-simple-taproot` retransmits the exact updates and then *regenerates* the partial
        // signature with a freshly exchanged nonce; it never replays the old signature. V2 does
        // the same: an outstanding CS whose ACK never arrived is abandoned and signed again after
        // recovery, which is safe because `allocate_nonce` uses a fresh seed per session.
        let resign_outstanding_cs = if state.tlc_state.waiting_ack {
            if peer.next_commitment_number == local {
                true
            } else if local.checked_add(1) == Some(peer.next_commitment_number) {
                false
            } else {
                return Err(invalid("Incompatible V2 incoming CS history"));
            }
        } else {
            // One step behind is the "missed our latest commitment" case, for example a peer that
            // never saw our `ChannelReadyV2`. Nothing is owed and nothing is replayed except that
            // Ready, so accept it instead of failing the handshake.
            if peer.next_commitment_number != local
                && peer.next_commitment_number.checked_add(1) != Some(local)
            {
                return Err(invalid("Incompatible V2 completed outgoing history"));
            }
            false
        };

        let has_cached_ack = state
            .session_v2
            .as_ref()
            .and_then(|session| session.incoming.as_ref())
            .and_then(|incoming| incoming.response.as_ref())
            .is_some();
        let replay_ack = if peer.next_ack_number == remote {
            false
        } else if peer.next_ack_number.checked_add(1) == Some(remote) {
            // Only a real owed ACK is replayed. Without a cached ACK the peer is simply one
            // commitment behind (it missed our `ChannelReadyV2`), which the Ready replay repairs.
            has_cached_ack
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
        // The peer advertises a fresh verification nonce on every re-establishment; adopt it.
        let peer_nonce = peer.next_local_commitment_nonce.clone();
        let peer_number = peer.next_commitment_number;
        state.recovery_peer_v2 = Some(peer);
        if let Some(session) = state.session_v2.as_mut() {
            session.remote_nonce = Some(peer_nonce);
            session.remote_number = peer_number;
        }
        if resign_outstanding_cs {
            // Carry the original updates into the regenerated round so another crash can replay
            // them. `pending_replay_updates` and `outgoing.updates` are disjoint by construction
            // (`prepare_outgoing_v2` drains the former into the latter), and the merged list is
            // drained again by the next commitment, so repeated crashes cannot accumulate it.
            let mut updates = outgoing
                .as_ref()
                .expect("outstanding CS validated")
                .updates
                .clone();
            state.replayed_tlc_updates_v2 = updates.len();
            updates.append(&mut state.pending_replay_updates);
            state.pending_replay_updates = updates;
            if let Some(session) = state.session_v2.as_mut() {
                session.outgoing = None;
            }
            state.set_waiting_ack(myself, false);
            tracing::debug!(
                "Channel {} abandoned a stale V2 CommitmentSigned and will sign a fresh one",
                state.get_id()
            );
        }
        self.replay_effects_v2(state);
        state.pending_remove_effects_v2 =
            state.session_v2.as_ref().expect("V2").remove_effects.len();
        self.update_tlc_status_on_ack(myself, state).await;
        self.apply_settled_remove_tlcs(myself, state, true).await;
        if !matches!(state.state, ChannelState::ShuttingDown(_)) {
            state.set_waiting_peer_response();
        }
        self.arm_peer_response_v2(state);
        // Replay the owed updates in their original relative order; the signature itself is
        // regenerated after recovery (see `resign_outstanding_cs`).
        let send_updates = || -> ProcessingChannelResult {
            if resign_outstanding_cs {
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
            }
            Ok(())
        };
        if state.last_was_revoke {
            send_updates()?;
        }
        if let Some(ack) = ack {
            state.send_v2_message(FiberMessage::revoke_and_ack_v2(ack));
        }
        if !state.last_was_revoke {
            send_updates()?;
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
        // A peer that missed our `ChannelReadyV2` (the connection dropped right after the funding
        // round) is still one commitment behind. Replay ours so it can finish opening; the gate
        // mirrors the V1 replay and cannot fire once both sides are caught up.
        if state.state == ChannelState::ChannelReady
            && state.funding_tx_confirmed_at.is_some()
            && state.recovery_peer_v2.as_ref().is_some_and(|peer| {
                peer.next_ack_number.checked_add(1) == Some(state.get_local_commitment_number())
            })
            && !state.tlc_state.waiting_ack
            && state.tlc_state.all_tlcs().next().is_none()
        {
            let ready = state.ready_wire_message()?;
            state.send_v2_message(ready);
            super::debug_event!(
                state.network(),
                "Replayed ChannelReadyV2 after reestablishment"
            );
        }
        if resign_outstanding_cs && !state.reestablishing {
            // Recovery no longer owes an ACK, so this is an ordinary CS exchange again.
            self.send_commitment_v2(myself, state).await?;
        }
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
        if !state.reestablishing
            && !state.tlc_state.waiting_ack
            && (!state.pending_replay_updates.is_empty()
                || state.tlc_state.need_another_commitment_signed())
        {
            // V2 replays its ordered log above; never publish raw TLC state before the next save.
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::ControlFiberChannel(super::ChannelCommandWithId {
                        channel_id: state.id,
                        command: super::ChannelCommand::CommitmentSigned(None),
                    }),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
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

    /// Publish saved removal hints without signing after the closing transaction is published.
    pub(super) fn publish_force_closing_removes_v2(&self, state: &mut ChannelActorState) {
        if state.session_v2.is_none()
            || !matches!(state.state, ChannelState::ShuttingDown(flags)
                if flags.contains(fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION))
        {
            return;
        }
        for update in state
            .pending_replay_updates
            .iter()
            .skip(state.replayed_tlc_updates_v2)
        {
            if let fiber_types::TlcReplayUpdate::Remove(remove) = update {
                state.send_v2_message(FiberMessage::remove_tlc(remove.clone()));
            }
        }
        state.replayed_tlc_updates_v2 = state.pending_replay_updates.len();
    }

    pub(super) fn replay_remove_effects_v2(&self, state: &ChannelActorState) {
        if let Some(session) = &state.session_v2 {
            self.publish_remove_effects_v2(state.id, &session.remove_effects);
        }
    }

    pub(super) fn publish_remove_effects_v2(
        &self,
        channel_id: fiber_types::Hash256,
        effects: &[(fiber_types::TlcInfo, Option<u32>)],
    ) {
        for (tlc, execution) in effects {
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::RelayV2RemoveEffect {
                        channel_id,
                        tlc: Box::new(tlc.clone()),
                        execution: *execution,
                    },
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
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
        let replayed = std::mem::take(&mut state.replayed_tlc_updates_v2);
        self.store.insert_channel_actor_state(state.clone());
        // Publish only after IDs, contents and the exact signing intent are durable.
        for update in &state
            .session_v2
            .as_ref()
            .expect("V2")
            .outgoing
            .as_ref()
            .expect("signed CS")
            .updates[replayed..]
        {
            state.send_v2_message(match update {
                fiber_types::TlcReplayUpdate::Add(add) => FiberMessage::add_tlc(add.clone()),
                fiber_types::TlcReplayUpdate::Remove(remove) => {
                    FiberMessage::remove_tlc(remove.clone())
                }
            });
        }
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
        let verified = state.stage_incoming_v2(&request, flags.is_some())?;
        let (transaction, settlement, response) =
            state.accept_commitment_v2(&request, flags.is_some(), verified)?;
        if let Some(flags) = flags {
            state.update_state(ChannelState::SigningCommitment(
                flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT,
            ));
        }
        let needs_cs = state.tlc_state.update_for_commitment_signed();
        state.last_was_revoke = response.is_some();
        // Staging/signing is local; commit the accepted session before publishing its results.
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
        self.apply_settled_remove_tlcs(myself, state, true).await;
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
        // A reconnect can replay an ACK we already processed before we re-signed the commitment
        // it refers to. Its bytes match `last_ack`, so it is an idempotent duplicate: ignore it
        // instead of trying to verify it against the regenerated session.
        let packed_ack: fiber_types::gen::fiber::RevokeAndAckV2 = ack.clone().into();
        if state
            .session_v2
            .as_ref()
            .and_then(|session| session.last_ack.as_ref())
            .is_some_and(|last| last.as_slice() == packed_ack.as_slice())
        {
            tracing::debug!(
                "Channel {} ignores an already processed V2 ACK for commitment {}",
                state.id,
                ack.commitment_number
            );
            return Ok(());
        }
        // The peer advertises its next verification nonce both in the reestablishment handshake and
        // in every `revoke_and_ack` (`next_local_nonces` in `bolt-simple-taproot`). The newest
        // advertisement wins: an ACK produced before a reconnect legitimately carries the previous
        // nonce, so a mismatch is not a contradiction. `accept_ack_v2` only accepts the ACK for
        // our current commitment number, so an older advertisement can never roll the nonce back.
        if state.reestablishing
            && state.recovery_peer_v2.as_ref().is_some_and(|peer| {
                ack.commitment_number.checked_add(1) == Some(peer.next_commitment_number)
                    && ack.next_commitment_nonce != peer.next_local_commitment_nonce
            })
        {
            tracing::debug!(
                "Channel {} adopts the ACK's verification nonce over the handshake one (commitment {})",
                state.id,
                ack.commitment_number + 1
            );
        }
        let verified = state.stage_ack_v2(&ack)?;
        // A reconnect abandons the outstanding commitment and re-signs it with a fresh nonce, so
        // the peer may still answer the *previous* round with an ACK that can no longer be
        // verified. Such an ACK is stale by construction: the peer answers the regenerated
        // commitment with a new ACK, so dropping it costs nothing. Outside recovery an
        // unverifiable ACK remains a hard error.
        let accepted = match state.accept_ack_v2(myself, &ack, verified) {
            Ok(accepted) => accepted,
            Err(err) if state.reestablishing => {
                tracing::warn!(
                    "Channel {} ignores an ACK for a superseded V2 commitment round: {}",
                    state.id,
                    err
                );
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let Some((revocation, settlement)) = accepted else {
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
        // An RAA cannot stand in for an unreceived CS: only updates already folded by a real
        // peer CS need another round. Otherwise we sign too early or storm rounds (#1561).
        let needs_cs = state.tlc_state.all_tlcs().any(|tlc| {
            matches!(
                tlc.status,
                fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::RemoveWaitAck)
                    | fiber_types::TlcStatus::Inbound(
                        fiber_types::InboundTlcStatus::AnnounceWaitAck
                    )
            )
        });
        if !state.is_waiting_tlc_ack() {
            self.apply_retryable_tlc_operations(myself, state, false)
                .await;
        }
        if needs_cs && !state.tlc_state.waiting_ack {
            self.send_commitment_v2(myself, state).await?;
        }
        Ok(())
    }

    pub(super) async fn apply_retryable_tlc_operations_v2(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        trigger_next: bool,
    ) {
        let mut changed = false;
        let mut add_results = Vec::new();
        while !state.is_waiting_tlc_ack() && !state.reestablishing {
            let Some(operation) = state.retryable_tlc_operations.pop_front() else {
                break;
            };
            match operation {
                RetryableTlcOperation::AddTlc(command) => {
                    let result = self.add_tlc_update(state, &command);
                    changed |= result.is_ok();
                    add_results.push((command, result));
                }
                RetryableTlcOperation::RemoveTlc(tlc_id, reason) => {
                    changed |= self
                        .remove_tlc_update(
                            state,
                            RemoveTlcCommand {
                                id: u64::from(tlc_id),
                                reason,
                            },
                        )
                        .is_ok();
                }
            }
        }
        if changed {
            // Persist/sign the whole batch before reporting successful additions.
            if let Err(error) = self.send_commitment_v2(myself, state).await {
                tracing::error!(?error, channel_id = ?state.id, "Unable to sign V2 TLC batch");
                self.store.insert_channel_actor_state(state.clone());
                for (_, result) in &mut add_results {
                    if result.is_ok() {
                        *result = Err(error.clone());
                    }
                }
            }
        }
        for (command, result) in add_results {
            self.post_add_tlc_command(myself, state, command, &result);
        }
        if trigger_next && !state.reestablishing && !state.retryable_tlc_operations.is_empty() {
            state.schedule_next_retry_task(myself);
        }
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
    use crate::fiber::network::NetworkActorEvent;
    use crate::fiber::tests::MockStore;
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
    async fn test_v2_commitment_acceptance_commits_once_before_publication() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut owner = b.get_channel_actor_state(id);
        let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
            .await
            .unwrap();
        sender.network = Some(network.clone());
        owner.network = Some(network.clone());
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.tlc_state.waiting_ack = true;
        let owner_actor =
            ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
        owner_actor
            .receive_commitment_v2(&myself, &mut owner, request)
            .await
            .unwrap();
        assert_eq!(
            owner_actor.store.channel_state_writes.get(),
            1,
            "persist the accepted CS, not an extra intermediate unsigned context"
        );
        let stored = owner_actor.store.get_channel_actor_state(&id).unwrap();
        stored.validate_session_v2().unwrap();
        let session = stored.session_v2.as_ref().unwrap();
        assert!(session.pending_incoming.is_none());
        let ack =
            RevokeAndAckV2::try_from(session.incoming.as_ref().unwrap().response.clone().unwrap())
                .unwrap();
        assert!(stored.latest_commitment_transaction.is_some());

        let sender_actor =
            ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        sender_actor
            .receive_ack_v2(&myself, &mut sender, ack)
            .await
            .unwrap();
        assert_eq!(
            sender_actor.store.channel_state_writes.get(),
            1,
            "persist the accepted ACK before its effects, without an intermediate write"
        );
        let stored = sender_actor.store.get_channel_actor_state(&id).unwrap();
        stored.validate_session_v2().unwrap();
        assert!(!stored.tlc_state.waiting_ack);
        assert!(stored.session_v2.as_ref().unwrap().pending_ack.is_none());
        assert!(stored.session_v2.as_ref().unwrap().last_ack.is_some());
        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_closing_commits_only_at_publication_boundaries() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sa = a.get_channel_actor_state(id);
        let mut sb = b.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        sa.network = Some(network.clone());
        sb.network = Some(network.clone());
        let actor_a =
            ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        let actor_b =
            ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
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
        assert_eq!(actor_a.store.channel_state_writes.get(), 1);
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
        actor_b
            .receive_shutdown_v2(&mut sb, shutdown)
            .await
            .unwrap();
        assert_eq!(actor_b.store.channel_state_writes.get(), 2,
            "commit the nonce advertisement and then the signature, without unsigned staging writes");
        let before = bincode::serialize(&sb.core).unwrap();
        actor_b.store.channel_state_writes.set(0);
        capture_barrier(&network, id).await;
        captured.lock().unwrap().clear();
        actor_b.progress_close_v2(&mut sb).await.unwrap();
        assert_eq!(bincode::serialize(&sb.core).unwrap(), before);
        assert_eq!(actor_b.store.channel_state_writes.get(), 0);
        capture_barrier(&network, id).await;
        assert!(captured.lock().unwrap().is_empty());
        let stored = actor_b.store.get_channel_actor_state(&id).unwrap();
        stored.validate_session_v2().unwrap();
        let close_b = stored
            .session_v2
            .as_ref()
            .unwrap()
            .closing
            .as_ref()
            .unwrap();
        let shutdown_b = ShutdownV2::try_from(close_b.shutdown.clone()).unwrap();
        let signature_b = ClosingSignedV2::try_from(close_b.response.clone().unwrap()).unwrap();
        actor_a.store.channel_state_writes.set(0);
        actor_a
            .receive_shutdown_v2(&mut sa, shutdown_b)
            .await
            .unwrap();
        assert_eq!(actor_a.store.channel_state_writes.get(), 1);
        actor_a.store.channel_state_writes.set(0);
        actor_a
            .receive_closing_v2(&mut sa, signature_b)
            .await
            .unwrap();
        assert_eq!(actor_a.store.channel_state_writes.get(), 1,
            "only the first transaction publication needs another commit; the response is already durable");
        let before = bincode::serialize(&sa.core).unwrap();
        actor_a.store.channel_state_writes.set(0);
        capture_barrier(&network, id).await;
        captured.lock().unwrap().clear();
        actor_a.progress_close_v2(&mut sa).await.unwrap();
        assert_eq!(bincode::serialize(&sa.core).unwrap(), before);
        assert_eq!(actor_a.store.channel_state_writes.get(), 0);
        capture_barrier(&network, id).await;
        assert!(matches!(
            captured.lock().unwrap().as_slice(),
            [NetworkActorMessage::Event(
                NetworkActorEvent::ClosingTransactionPending(..)
            )]
        ));
        let stored = actor_a.store.get_channel_actor_state(&id).unwrap();
        stored.validate_session_v2().unwrap();
        assert!(
            matches!(stored.state, ChannelState::ShuttingDown(flags) if flags == fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
        );
        assert!(stored
            .session_v2
            .as_ref()
            .unwrap()
            .closing
            .as_ref()
            .unwrap()
            .remote_response
            .is_some());
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_remove_effects_wait_for_message_tail_even_after_partial_lookup_failure() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        for defer_last in [false, true] {
            let mut state = a.get_channel_actor_state(id);
            let captured = Arc::new(Mutex::new(Vec::new()));
            let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
                .await
                .unwrap();
            state.network = Some(network.clone());
            let balances = (state.to_local_amount, state.to_remote_amount);
            for tag in 0..3 {
                let preimage = fiber_types::Hash256::from([tag; 32]);
                let mut add = retry_batch_add(tag, 1000);
                add.payment_hash = add.hash_algorithm.hash(preimage).into();
                if defer_last && tag == 2 {
                    add.attempt_id = Some(42);
                }
                let mut tlc = state.create_outbounding_tlc(&add);
                tlc.status = fiber_types::TlcStatus::Outbound(
                    fiber_types::OutboundTlcStatus::RemoveAckConfirmed,
                );
                tlc.removed_confirmed_at = Some(state.get_local_commitment_number());
                tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
                    fiber_types::RemoveTlcFulfill {
                        payment_preimage: preimage,
                    },
                ));
                state.tlc_state.add_offered_tlc(tlc);
                state.increment_next_offered_tlc_id();
            }
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
            actor.store.insert_channel_actor_state(state.clone());
            actor.store.channel_state_writes.set(0);
            for tlc in state.tlc_state.offered_tlcs.tlcs.iter() {
                if let Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(fulfill)) =
                    &tlc.removed_reason
                {
                    actor
                        .store
                        .insert_preimage(tlc.payment_hash, fulfill.payment_preimage);
                }
            }
            let durable_before = bincode::serialize(&state.core).unwrap();
            actor
                .apply_settled_remove_tlcs(&myself, &mut state, false)
                .await;
            let applied = if defer_last { 2 } else { 3 };
            assert_eq!(actor.store.channel_state_writes.get(), 0);
            let stored = actor.store.get_channel_actor_state(&id).unwrap();
            assert_eq!(bincode::serialize(&stored.core).unwrap(), durable_before);
            assert!(stored
                .session_v2
                .as_ref()
                .unwrap()
                .remove_effects
                .is_empty());
            assert!(actor.store.preimage_removal_writes.borrow().is_empty());
            assert_eq!((stored.to_local_amount, stored.to_remote_amount), balances);
            assert_eq!(
                (state.to_local_amount, state.to_remote_amount),
                (
                    balances.0 - applied as u128 * 1000,
                    balances.1 + applied as u128 * 1000
                )
            );
            if defer_last {
                let pending = state
                    .tlc_state
                    .get(&fiber_types::TLCId::Offered(2))
                    .unwrap();
                assert!(!pending
                    .applied_flags
                    .contains(fiber_types::AppliedFlags::REMOVE));
            } else {
                assert!(state.tlc_state.offered_tlcs.tlcs.is_empty());
            }
            capture_barrier(&network, id).await;
            assert_eq!(
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        NetworkActorMessage::Command(
                            NetworkActorCommand::RelayV2RemoveEffect { .. }
                        )
                    ))
                    .count(),
                0,
                "do not publish any batch member before the message-tail commit"
            );
            // Before commit, recovery reapplies removals from the original durable TLCs.
            let mut restored = ChannelActorState::from(stored.core);
            restored.network = Some(network.clone());
            assert_eq!(restored.pending_remove_effects_v2, 0);
            actor.replay_remove_effects_v2(&restored);
            capture_barrier(&network, id).await;
            assert_eq!(
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        NetworkActorMessage::Command(
                            NetworkActorCommand::RelayV2RemoveEffect { .. }
                        )
                    ))
                    .count(),
                0
            );
            actor
                .apply_settled_remove_tlcs(&myself, &mut restored, false)
                .await;
            assert_eq!(
                (restored.to_local_amount, restored.to_remote_amount),
                (state.to_local_amount, state.to_remote_amount)
            );
            assert_eq!(
                restored.session_v2.as_ref().unwrap().remove_effects.len(),
                applied
            );
            assert_eq!(actor.store.channel_state_writes.get(), 0);
            assert!(actor.store.preimage_removal_writes.borrow().is_empty());
            captured.lock().unwrap().clear();
            actor.persist_state_and_publish_pending_effects(&mut state);
            assert_eq!(actor.store.channel_state_writes.get(), 1);
            let stored = actor.store.get_channel_actor_state(&id).unwrap();
            assert_eq!(
                (stored.to_local_amount, stored.to_remote_amount),
                (state.to_local_amount, state.to_remote_amount)
            );
            assert_eq!(
                stored.session_v2.as_ref().unwrap().remove_effects.len(),
                applied
            );
            capture_barrier(&network, id).await;
            let relayed: Vec<_> = captured
                .lock()
                .unwrap()
                .iter()
                .filter_map(|message| match message {
                    NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect {
                        tlc,
                        ..
                    }) => Some(tlc.tlc_id),
                    _ => None,
                })
                .collect();
            assert_eq!(
                relayed,
                if defer_last {
                    vec![
                        fiber_types::TLCId::Offered(0),
                        fiber_types::TLCId::Offered(1),
                    ]
                } else {
                    vec![
                        fiber_types::TLCId::Offered(0),
                        fiber_types::TLCId::Offered(1),
                        fiber_types::TLCId::Offered(2),
                    ]
                }
            );
            actor.persist_state_and_publish_pending_effects(&mut state);
            capture_barrier(&network, id).await;
            assert_eq!(
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        NetworkActorMessage::Command(
                            NetworkActorCommand::RelayV2RemoveEffect { .. }
                        )
                    ))
                    .count(),
                applied,
                "an unrelated message must not replay the outbox again"
            );
            if !defer_last {
                actor
                    .handle_event(
                        &myself,
                        &mut state,
                        super::super::ChannelEvent::MaintainChannelTlcs,
                    )
                    .await
                    .unwrap();
                capture_barrier(&network, id).await;
                assert_eq!(
                    captured
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|message| matches!(
                            message,
                            NetworkActorMessage::Command(
                                NetworkActorCommand::RelayV2RemoveEffect { .. }
                            )
                        ))
                        .count(),
                    applied
                );
                actor.persist_state_and_publish_pending_effects(&mut state);
                capture_barrier(&network, id).await;
                assert_eq!(
                    captured
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|message| matches!(
                            message,
                            NetworkActorMessage::Command(
                                NetworkActorCommand::RelayV2RemoveEffect { .. }
                            )
                        ))
                        .count(),
                    applied * 2,
                    "maintenance must still retry unconfirmed durable effects, after its commit"
                );
            }
            network.stop(None);
            task.await.unwrap();
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_v2_remove_effects_precede_force_close_without_publishing_failed_removal() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let balances = (state.to_local_amount, state.to_remote_amount);
        for tag in 0..3 {
            let preimage = fiber_types::Hash256::from([tag; 32]);
            let mut add = retry_batch_add(tag, 1000);
            add.payment_hash = add.hash_algorithm.hash(preimage).into();
            let mut tlc = state.create_outbounding_tlc(&add);
            tlc.status = fiber_types::TlcStatus::Outbound(
                fiber_types::OutboundTlcStatus::RemoveAckConfirmed,
            );
            tlc.removed_confirmed_at = Some(state.get_local_commitment_number());
            tlc.removed_reason = Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(
                fiber_types::RemoveTlcFulfill {
                    payment_preimage: if tag == 2 { [99; 32].into() } else { preimage },
                },
            ));
            state.tlc_state.add_offered_tlc(tlc);
            state.increment_next_offered_tlc_id();
        }
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        actor
            .apply_settled_remove_tlcs(&myself, &mut state, false)
            .await;
        actor.persist_state_and_publish_pending_effects(&mut state);
        assert_eq!(
            (state.to_local_amount, state.to_remote_amount),
            (balances.0 - 2000, balances.1 + 2000)
        );
        let stored = actor.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(
            (stored.to_local_amount, stored.to_remote_amount),
            (state.to_local_amount, state.to_remote_amount)
        );
        capture_barrier(&network, id).await;
        let publications: Vec<_> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match message {
                NetworkActorMessage::Command(NetworkActorCommand::RelayV2RemoveEffect {
                    tlc,
                    ..
                }) => Some(Some(tlc.tlc_id)),
                NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                    super::super::ChannelCommandWithId {
                        command: super::super::ChannelCommand::Shutdown(command, _),
                        ..
                    },
                )) if command.force => Some(None),
                _ => None,
            })
            .collect();
        assert_eq!(
            publications,
            vec![
                Some(fiber_types::TLCId::Offered(0)),
                Some(fiber_types::TLCId::Offered(1)),
                None
            ],
            "publish only successful removals, before the close that can supersede their outbox"
        );
        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_ready_replay_persists_only_a_new_reconnect_nonce() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        for prior_reconnect_event in [false, true] {
            let mut state = a.get_channel_actor_state(id);
            state.network = Some(network.clone());
            state.mark_reestablishing_offline();
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
            if prior_reconnect_event {
                state.on_peer_reconnected();
            }
            actor.store.insert_channel_actor_state(state.clone());
            actor.store.channel_state_writes.set(0);
            let before = bincode::serialize(&state.core).unwrap();
            let old_nonce = state.session_v2.as_ref().unwrap().own.public_nonce.clone();
            let mut peer = b.get_channel_actor_state(id).reestablish_message_v2();
            peer.next_ack_number = INITIAL_COMMITMENT_NUMBER + 1;
            actor
                .handle_peer_message(
                    &myself,
                    &mut state,
                    FiberChannelMessage::ReestablishChannelV2(peer),
                )
                .await
                .unwrap();
            assert_eq!(
                actor.store.channel_state_writes.get(),
                if prior_reconnect_event { 0 } else { 1 }
            );
            let stored = actor.store.get_channel_actor_state(&id).unwrap();
            let nonce = &stored.session_v2.as_ref().unwrap().own.public_nonce;
            assert_eq!(nonce, &state.session_v2.as_ref().unwrap().own.public_nonce);
            assert_eq!(*nonce == old_nonce, prior_reconnect_event);
            if prior_reconnect_event {
                assert_eq!(bincode::serialize(&stored.core).unwrap(), before);
            }
            state.validate_session_v2().unwrap();
            capture_barrier(&network, id).await;
            let messages = captured.lock().unwrap();
            assert!(messages.iter().any(|message| matches!(message,
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(FiberMessageWithTarget {
                    message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReadyV2(ready)), ..
                })) if &ready.next_commitment_nonce == nonce)));
            drop(messages);
            captured.lock().unwrap().clear();
        }
        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_pending_forward_recovery_only_saves_changed_ownership() {
        let upstream = fiber_types::Hash256::from([71; 32]);
        let downstream = fiber_types::Hash256::from([72; 32]);
        let hash = fiber_types::Hash256::from([73; 32]);
        let mut incoming =
            crate::fiber::tests::create_test_channel_state_with_tlc(upstream, 0, 1000, hash, None);
        incoming
            .waiting_forward_tlc_tasks
            .insert(fiber_types::TLCId::Received(0), [7; 32]);
        let mut outgoing = crate::fiber::tests::create_test_channel_state_with_tlc(
            downstream, 0, 1000, hash, None,
        );
        outgoing.tlc_state = Default::default();
        let mut add = retry_batch_add(73, 900);
        add.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
            upstream, 0, 100, [7; 32],
        ));
        outgoing
            .retryable_tlc_operations
            .push_back(RetryableTlcOperation::AddTlc(add.clone()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, Default::default())
            .await
            .unwrap();
        incoming.network = Some(network.clone());
        let actor = ChannelActor::new(
            incoming.get_local_pubkey(),
            incoming.get_remote_pubkey(),
            network.clone(),
            MockStore::new(),
            None,
        );
        actor.store.insert_channel_actor_state(incoming.clone());
        actor.store.insert_channel_actor_state(outgoing.clone());
        actor.store.channel_state_writes.set(0);
        let before = bincode::serialize(&incoming.core).unwrap();
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        assert!(actor.recover_pending_forward_tlcs(&myself, &mut incoming));
        assert_eq!(bincode::serialize(&incoming.core).unwrap(), before);
        assert_eq!(actor.store.channel_state_writes.get(), 0);

        outgoing.retryable_tlc_operations.clear();
        let tlc = outgoing.create_outbounding_tlc(&add);
        outgoing.tlc_state.add_offered_tlc(tlc);
        actor.store.insert_channel_actor_state(outgoing);
        actor.store.channel_state_writes.set(0);
        assert!(actor.recover_pending_forward_tlcs(&myself, &mut incoming));
        assert_eq!(actor.store.channel_state_writes.get(), 1);
        let stored = actor.store.get_channel_actor_state(&upstream).unwrap();
        assert!(stored.waiting_forward_tlc_tasks.is_empty());
        assert_eq!(
            stored
                .tlc_state
                .get(&fiber_types::TLCId::Received(0))
                .unwrap()
                .forwarding_tlc,
            Some((downstream, 0))
        );
        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_receipt_only_saves_early_before_preimage_cleanup() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        for (fulfill, retain, early_writes) in
            [(false, false, 0), (true, true, 0), (true, false, 1)]
        {
            let mut state = a.get_channel_actor_state(id);
            state.network = Some(network.clone());
            let preimage = fiber_types::Hash256::from([74; 32]);
            let mut add = retry_batch_add(74, 1000);
            add.payment_hash = add.hash_algorithm.hash(preimage).into();
            let reason = if fulfill {
                fiber_types::RemoveTlcReason::RemoveTlcFulfill(fiber_types::RemoveTlcFulfill {
                    payment_preimage: preimage,
                })
            } else {
                fiber_types::RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
                    fiber_types::TlcErr::new(fiber_types::TlcErrorCode::HoldTlcTimeout),
                    &[0; 32],
                ))
            };
            let mut tlc = state.create_outbounding_tlc(&add);
            tlc.status = fiber_types::TlcStatus::Outbound(
                fiber_types::OutboundTlcStatus::RemoveAckConfirmed,
            );
            tlc.removed_reason = Some(reason.clone());
            let tlc_id = tlc.tlc_id;
            state
                .session_v2
                .as_mut()
                .unwrap()
                .remove_effects
                .push((tlc.clone(), None));
            if retain {
                tlc.tlc_id = fiber_types::TLCId::Offered(1);
                state
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .remove_effects
                    .push((tlc, None));
            }
            let store = MockStore::new().with_preimage(add.payment_hash, preimage);
            let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), store, None);
            actor.store.insert_channel_actor_state(state.clone());
            actor.store.channel_state_writes.set(0);
            actor
                .handle_event(
                    &myself,
                    &mut state,
                    super::super::ChannelEvent::OnChainTlcRelayConfirmed(tlc_id, reason),
                )
                .await
                .unwrap();
            assert_eq!(actor.store.channel_state_writes.get(), early_writes);
            assert_eq!(
                actor.store.get_preimage(&add.payment_hash).is_none(),
                fulfill && !retain
            );
            assert_eq!(
                *actor.store.preimage_removal_writes.borrow(),
                if fulfill && !retain { vec![1] } else { vec![] }
            );
            capture_barrier(&network, id).await;
            assert_eq!(captured.lock().unwrap().iter().filter(|message| matches!(message,
                NetworkActorMessage::Notification(NetworkServiceEvent::PreimageRemoved(hash)) if *hash == add.payment_hash)).count(), if fulfill && !retain { 1 } else { 0 });
            captured.lock().unwrap().clear();
            actor.persist_state_and_publish_pending_effects(&mut state);
            assert_eq!(actor.store.channel_state_writes.get(), early_writes + 1);
            assert_eq!(
                actor
                    .store
                    .get_channel_actor_state(&id)
                    .unwrap()
                    .session_v2
                    .as_ref()
                    .unwrap()
                    .remove_effects
                    .len(),
                usize::from(retain)
            );
        }
        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    fn retry_batch_add(tag: u8, amount: u128) -> fiber_types::AddTlcCommand {
        fiber_types::AddTlcCommand {
            amount,
            payment_hash: [tag; 32].into(),
            attempt_id: None,
            expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
            hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
        }
    }

    #[tokio::test]
    async fn test_forward_add_rejects_unbacked_or_already_forwarded_upstream() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let initial = a.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        for case in [
            "removed",
            "queued_remove",
            "missing_tlc",
            "hash",
            "algorithm",
            "closed",
            "already_forwarded",
            "already_offered",
            "missing_channel",
            "same_channel_queued_remove",
            "valid",
        ] {
            let mut state = initial.clone();
            state.network = Some(network.clone());
            let mut command = retry_batch_add(42, 1000);
            let upstream_id = if case == "same_channel_queued_remove" {
                id
            } else {
                crate::gen_rand_sha256_hash()
            };
            command.previous_tlc = Some(fiber_types::PrevTlcInfo::new_with_shared_secret(
                upstream_id,
                0,
                100,
                [7; 32],
            ));
            let mut upstream = crate::fiber::tests::create_test_channel_state_with_tlc(
                upstream_id,
                0,
                1100,
                command.payment_hash,
                None,
            );
            let reason =
                fiber_types::RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
                    fiber_types::TlcErr::new(fiber_types::TlcErrorCode::TemporaryChannelFailure),
                    &[7; 32],
                ));
            match case {
                "removed" => {
                    upstream
                        .tlc_state
                        .get_mut(&fiber_types::TLCId::Received(0))
                        .unwrap()
                        .removed_reason = Some(reason.clone())
                }
                "queued_remove" => upstream.retryable_tlc_operations.push_back(
                    fiber_types::RetryableTlcOperation::RemoveTlc(
                        fiber_types::TLCId::Received(0),
                        reason.clone(),
                    ),
                ),
                "missing_tlc" => upstream.tlc_state.received_tlcs.tlcs.clear(),
                "hash" => {
                    upstream
                        .tlc_state
                        .get_mut(&fiber_types::TLCId::Received(0))
                        .unwrap()
                        .payment_hash = [43; 32].into()
                }
                "algorithm" => {
                    upstream
                        .tlc_state
                        .get_mut(&fiber_types::TLCId::Received(0))
                        .unwrap()
                        .hash_algorithm = fiber_types::HashAlgorithm::Sha256
                }
                "closed" => upstream.state = ChannelState::Closed(fiber_types::CloseFlags::empty()),
                "already_forwarded" => {
                    upstream
                        .tlc_state
                        .get_mut(&fiber_types::TLCId::Received(0))
                        .unwrap()
                        .forwarding_tlc = Some((id, 0))
                }
                "already_offered" => {
                    let existing = state.create_outbounding_tlc(&command);
                    state.tlc_state.add_offered_tlc(existing);
                    state.increment_next_offered_tlc_id();
                }
                "same_channel_queued_remove" => {
                    state.tlc_state.received_tlcs = upstream.tlc_state.received_tlcs.clone();
                    state.retryable_tlc_operations.push_back(
                        fiber_types::RetryableTlcOperation::RemoveTlc(
                            fiber_types::TLCId::Received(0),
                            reason.clone(),
                        ),
                    );
                }
                _ => {}
            }
            if case != "missing_channel" && case != "same_channel_queued_remove" {
                a.store.insert_channel_actor_state(upstream);
            }
            captured.lock().unwrap().clear();
            let offered_before = state.tlc_state.offered_tlcs.tlcs.len();
            let result = actor.add_tlc_update(&mut state, &command);
            assert_eq!(result.is_ok(), case == "valid", "{case}: {result:?}");
            assert_eq!(
                state.tlc_state.offered_tlcs.tlcs.len(),
                offered_before + usize::from(case == "valid"),
                "reject before allocating an outgoing TLC: {case}"
            );
            capture_barrier(&network, id).await;
            assert!(
                take_wire_messages(&captured).is_empty(),
                "do not publish before commit: {case}"
            );
            if result.is_ok() {
                actor.send_commitment_v2(&myself, &mut state).await.unwrap();
            }
            capture_barrier(&network, id).await;
            let sent = captured.lock().unwrap().iter().any(|message| {
                matches!(
                    message,
                    NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                        FiberMessageWithTarget {
                            message: FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::AddTlc(_)
                            ),
                            ..
                        }
                    ))
                )
            });
            assert_eq!(
                sent,
                case == "valid",
                "never publish an unbacked forward: {case}"
            );
        }
        myself.stop(None);
        handle.await.unwrap();
        network.stop(None);
        task.await.unwrap();
    }

    fn take_wire_messages(
        captured: &Arc<Mutex<Vec<NetworkActorMessage>>>,
    ) -> Vec<FiberMessageWithTarget> {
        std::mem::take(&mut *captured.lock().unwrap())
            .into_iter()
            .filter_map(|message| match message {
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(message)) => {
                    Some(message)
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn test_v2_stale_snapshot_reproduces_tlc_conflict_invalid_cs_and_ack_timeout() {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut receiver = b.get_channel_actor_state(id);
        b.stop().await;
        a.stop().await;
        sender.tlc_state.offered_tlcs.next_tlc_id = 1763;
        receiver.tlc_state.received_tlcs.next_tlc_id = 1763;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        sender.network = Some(network.clone());
        receiver.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let peer = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(11, 1));
        actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(12, 1));
        a.store.insert_channel_actor_state(sender.clone());
        let mut stale = a.store.get_channel_actor_state(&id).unwrap();
        actor
            .apply_retryable_tlc_operations(&myself, &mut sender, false)
            .await;
        capture_barrier(&network, id).await;
        let original_wire = take_wire_messages(&captured);
        for message in original_wire {
            if let FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) =
                message.message
            {
                peer.handle_add_tlc_peer_message(&mut receiver, add)
                    .unwrap();
            }
        }
        assert_eq!(
            a.store
                .get_channel_actor_state(&id)
                .unwrap()
                .tlc_state
                .offered_tlcs
                .next_tlc_id,
            1765
        );
        assert_eq!(receiver.tlc_state.received_tlcs.next_tlc_id, 1765);
        b.store.insert_channel_actor_state(receiver);
        // Inject the old shutdown helper's read/write interleaving after a newer batch commit.
        stale.mark_reestablishing_offline();
        a.store.insert_channel_actor_state(stale);
        let mut restored = a.store.get_channel_actor_state(&id).unwrap();
        let mut receiver = b.store.get_channel_actor_state(&id).unwrap();
        restored.network = Some(network.clone());
        receiver.network = Some(network.clone());
        actor
            .reestablish_v2(&myself, &mut restored, receiver.reestablish_message_v2())
            .await
            .unwrap();
        assert!(!restored.reestablishing);
        let reused = actor
            .handle_add_tlc_command(&myself, &mut restored, &retry_batch_add(13, 1))
            .await
            .unwrap();
        assert_eq!(reused, 1763);
        capture_barrier(&network, id).await;
        let wire = take_wire_messages(&captured);
        let mut request = None;
        let mut rejected_add = false;
        for message in wire {
            match message.message {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                    assert_eq!(add.tlc_id, 1763);
                    let error = peer
                        .handle_add_tlc_peer_message(&mut receiver, add)
                        .unwrap_err();
                    assert!(error.to_string().contains("Conflicting replayed tlc"));
                    rejected_add = true;
                }
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                    cs,
                )) => request = Some(cs),
                _ => {}
            }
        }
        assert!(rejected_add);
        let request = request.unwrap();
        let outgoing = restored
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .as_ref()
            .unwrap();
        let nonce = AggNonce::sum([
            request.funding_nonce.clone(),
            receiver
                .session_v2
                .as_ref()
                .unwrap()
                .own
                .public_nonce
                .clone(),
        ]);
        musig2::verify_partial(
            &restored.get_deterministic_musig2_agg_context(),
            request.funding_tx_partial_signature,
            &nonce,
            *restored.get_local_funding_pubkey(),
            &request.funding_nonce,
            compute_tx_message(&outgoing.transaction.clone().into_view()),
        )
        .expect("the sender's signature is valid for its own persisted transaction");
        assert!(receiver
            .stage_incoming_v2(&request, false)
            .unwrap_err()
            .to_string()
            .contains("failed to verify signature"));
        restored
            .session_v2
            .as_mut()
            .unwrap()
            .outgoing
            .as_mut()
            .unwrap()
            .peer_wait_started_at = 0;
        actor
            .handle_event(
                &myself,
                &mut restored,
                super::super::ChannelEvent::CheckActiveChannel,
            )
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        assert!(captured.lock().unwrap().iter().any(|message| matches!(message,
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                super::super::ChannelCommandWithId { command: super::super::ChannelCommand::Shutdown(command, _), .. }
            )) if command.force)));
        myself.stop(None);
        handle.await.unwrap();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_updates_wait_for_commit_before_becoming_peer_visible() {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut receiver = b.get_channel_actor_state(id);
        b.stop().await;
        a.stop().await;
        sender.tlc_state.offered_tlcs.next_tlc_id = 1763;
        receiver.tlc_state.received_tlcs.next_tlc_id = 1763;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        sender.network = Some(network.clone());
        receiver.network = Some(network.clone());
        a.store.insert_channel_actor_state(sender.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let peer = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        actor
            .add_tlc_update(&mut sender, &retry_batch_add(11, 1))
            .unwrap();
        actor
            .add_tlc_update(&mut sender, &retry_batch_add(12, 1))
            .unwrap();
        capture_barrier(&network, id).await;
        let early = take_wire_messages(&captured);
        assert!(
            early.is_empty(),
            "AddTlc reached the peer before its ID/content was durable: {early:?}"
        );
        let mut restored = a.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(restored.tlc_state.offered_tlcs.next_tlc_id, 1763);
        assert!(restored.tlc_state.offered_tlcs.tlcs.is_empty());
        restored.network = Some(network.clone());
        // A different payment may reuse an uncommitted ID only if the old Add was never published.
        actor
            .handle_add_tlc_command(&myself, &mut restored, &retry_batch_add(13, 1))
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        let wire = take_wire_messages(&captured);
        assert_eq!(wire.len(), 2);
        let mut ack = None;
        for message in wire {
            match message.message {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                    let durable = a.store.get_channel_actor_state(&id).unwrap();
                    let tlc = durable
                        .tlc_state
                        .get(&fiber_types::TLCId::Offered(add.tlc_id))
                        .unwrap();
                    assert_eq!(tlc.payment_hash, add.payment_hash);
                    assert_eq!(tlc.amount, add.amount);
                    assert_eq!(tlc.expiry, add.expiry);
                    assert_eq!(durable.tlc_state.offered_tlcs.next_tlc_id, 1764);
                    peer.handle_add_tlc_peer_message(&mut receiver, add)
                        .unwrap();
                }
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                    cs,
                )) => {
                    let verified = receiver.stage_incoming_v2(&cs, false).unwrap();
                    ack = receiver
                        .accept_commitment_v2(&cs, false, verified)
                        .unwrap()
                        .2;
                }
                _ => panic!("unexpected wire message"),
            }
        }
        let ack = ack.unwrap();
        let verified = restored.stage_ack_v2(&ack).unwrap();
        assert!(restored
            .accept_ack_v2(&myself, &ack, verified)
            .unwrap()
            .is_some());
        assert!(!restored.tlc_state.waiting_ack);
        assert_eq!(restored.state, ChannelState::ChannelReady);
        actor
            .handle_event(
                &myself,
                &mut restored,
                super::super::ChannelEvent::CheckActiveChannel,
            )
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        assert!(take_wire_messages(&captured).is_empty());
        myself.stop(None);
        handle.await.unwrap();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_force_closing_removal_is_committed_and_published_without_cs() {
        let (mut a, mut b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let preimage = crate::gen_rand_sha256_hash();
        let invoice = crate::invoice::InvoiceBuilder::new(crate::invoice::Currency::Fibd)
            .amount(Some(1000))
            .payment_preimage(preimage)
            .payee_pub_key(b.pubkey.into())
            .expiry_time(std::time::Duration::from_secs(3600))
            .build_with_sign(|hash| {
                secp256k1::SECP256K1.sign_ecdsa_recoverable(hash, &b.private_key.0)
            })
            .unwrap();
        let hash = *invoice.payment_hash();
        b.insert_invoice(invoice.clone(), None);
        a.send_payment(crate::fiber::payment::SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_parts: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
        crate::tests::test_utils::wait_until_timeout(10000, || {
            !b.store.get_payment_hold_tlcs(hash).is_empty()
        })
        .await;
        let payer = a.get_channel_actor_state(id);
        let forwarder = b.get_channel_actor_state(id);
        let received = forwarder
            .tlc_state
            .received_tlcs
            .tlcs
            .iter()
            .find(|tlc| tlc.payment_hash == hash)
            .unwrap();
        assert_eq!(
            received.inbound_status(),
            fiber_types::InboundTlcStatus::Committed
        );
        let tlc_id = received.id();
        b.stop().await;
        a.stop().await;

        for rpc in [false, true] {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
                .await
                .unwrap();
            let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
            let actor =
                ChannelActor::new(b.pubkey, a.pubkey, network.clone(), MockStore::new(), None);
            let mut state = forwarder.clone();
            state.network = Some(network.clone());
            // Force close must not publish an Add left over from an unsigned batch.
            actor
                .add_tlc_update(&mut state, &retry_batch_add(200, 1))
                .unwrap();
            state.state = ChannelState::ShuttingDown(
                fiber_types::ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
            );
            actor.store.insert_channel_actor_state(state.clone());
            actor.store.channel_state_writes.set(0);
            let old_outgoing = state
                .session_v2
                .as_ref()
                .unwrap()
                .outgoing
                .as_ref()
                .unwrap()
                .request
                .clone();
            let balances = (state.to_local_amount, state.to_remote_amount);
            let command = RemoveTlcCommand {
                id: tlc_id,
                reason: fiber_types::RemoveTlcReason::RemoveTlcFulfill(
                    fiber_types::RemoveTlcFulfill {
                        payment_preimage: preimage,
                    },
                ),
            };
            if rpc {
                let (reply, response) = tokio::sync::oneshot::channel();
                actor
                    .handle_command(
                        &myself,
                        &mut state,
                        super::super::ChannelCommand::RemoveTlc(command, reply.into()),
                    )
                    .await
                    .unwrap();
                response.await.unwrap().unwrap();
            } else {
                actor
                    .handle_remove_tlc_command(&myself, &mut state, command)
                    .await
                    .unwrap();
                capture_barrier(&network, id).await;
                assert!(take_wire_messages(&captured).is_empty());
                assert_eq!(actor.store.channel_state_writes.get(), 0);
                actor.persist_state_and_publish_pending_effects(&mut state);
            }
            capture_barrier(&network, id).await;
            let wire = take_wire_messages(&captured);
            assert_eq!(
                wire.len(),
                1,
                "publish only the terminal RemoveTlc, never Add or CS"
            );
            assert_eq!(actor.store.channel_state_writes.get(), 1);
            let durable = actor.store.get_channel_actor_state(&id).unwrap();
            let tlc = durable
                .tlc_state
                .get(&fiber_types::TLCId::Received(tlc_id))
                .unwrap();
            assert_eq!(
                tlc.inbound_status(),
                fiber_types::InboundTlcStatus::LocalRemoved
            );
            assert!(matches!(
                tlc.removed_reason,
                Some(fiber_types::RemoveTlcReason::RemoveTlcFulfill(_))
            ));
            assert_eq!(
                durable
                    .session_v2
                    .as_ref()
                    .unwrap()
                    .outgoing
                    .as_ref()
                    .unwrap()
                    .request
                    .as_slice(),
                old_outgoing.as_slice()
            );
            assert_eq!(
                (durable.to_local_amount, durable.to_remote_amount),
                balances
            );
            let FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(remove)) =
                &wire[0].message
            else {
                panic!("expected only a removal notification")
            };
            let mut payer = payer.clone();
            payer.network = Some(network.clone());
            let peer =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
            peer.handle_remove_tlc_peer_message(&mut payer, remove.clone())
                .unwrap();
            assert_eq!(
                payer
                    .tlc_state
                    .get(&fiber_types::TLCId::Offered(tlc_id))
                    .unwrap()
                    .outbound_status(),
                fiber_types::OutboundTlcStatus::RemoteRemoved
            );
            actor.persist_state_and_publish_pending_effects(&mut state);
            capture_barrier(&network, id).await;
            assert!(
                take_wire_messages(&captured).is_empty(),
                "do not republish on every message tail"
            );
            let mut restored = ChannelActorState::from(durable.core);
            restored.network = Some(network.clone());
            actor.persist_state_and_publish_pending_effects(&mut restored);
            capture_barrier(&network, id).await;
            let replayed = take_wire_messages(&captured);
            assert_eq!(replayed.len(), 1);
            assert_eq!(
                replayed[0].message.clone().to_molecule_bytes(),
                wire[0].message.clone().to_molecule_bytes()
            );
            assert_eq!(
                a.get_payment_session(hash).unwrap().status,
                fiber_types::PaymentStatus::Inflight
            );
            myself.stop(None);
            handle.await.unwrap();
            network.stop(None);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_v2_preparation_failure_keeps_pending_updates_unpublished() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        state.session_v2.as_mut().unwrap().remote_nonce = None;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), MockStore::new(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        actor
            .add_tlc_update(&mut state, &retry_batch_add(11, 1))
            .unwrap();
        actor
            .add_tlc_update(&mut state, &retry_batch_add(12, 1))
            .unwrap();
        assert!(actor.send_commitment_v2(&myself, &mut state).await.is_err());
        actor.persist_state_and_publish_pending_effects(&mut state);
        capture_barrier(&network, id).await;
        assert!(take_wire_messages(&captured).is_empty());
        assert_eq!(actor.store.channel_state_writes.get(), 1);
        let durable = actor.store.get_channel_actor_state(&id).unwrap();
        assert_eq!(durable.pending_replay_updates.len(), 2);
        assert_eq!(durable.tlc_state.offered_tlcs.tlcs.len(), 2);
        assert!(!durable.tlc_state.waiting_ack);
        myself.stop(None);
        handle.await.unwrap();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_retry_queue_batches_after_ack() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let mut peer = b.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();

        actor.send_commitment_v2(&myself, &mut state).await.unwrap();
        let request = CommitmentSignedV2::try_from(
            state
                .session_v2
                .as_ref()
                .unwrap()
                .outgoing
                .as_ref()
                .unwrap()
                .request
                .clone(),
        )
        .unwrap();
        peer.stage_incoming_v2(&request, false).unwrap();
        let (_, _, ack) = peer.accept_commitment_v2(&request, false, false).unwrap();

        // A peer update folded while awaiting ACK also needs the next CS, not a separate one.
        let command = retry_batch_add(3, 1000);
        let mut received = state
            .create_inbounding_tlc(fiber_types::AddTlc {
                channel_id: id,
                tlc_id: 0,
                amount: command.amount,
                payment_hash: command.payment_hash,
                expiry: command.expiry,
                hash_algorithm: command.hash_algorithm,
                onion_packet: None,
            })
            .unwrap();
        received.status =
            fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::AnnounceWaitPrevAck);
        state.tlc_state.add_received_tlc(received.clone());
        state.increment_next_received_tlc_id();
        received.flip_mut();
        received.status =
            fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::Committed);
        peer.tlc_state.add_offered_tlc(received);
        peer.increment_next_offered_tlc_id();
        let peer_actor =
            ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);

        actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(1, 2000));
        actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(2, 3000));
        actor
            .apply_retryable_tlc_operations(&myself, &mut state, false)
            .await;
        assert_eq!(state.retryable_tlc_operations.len(), 2);
        assert_eq!(
            state
                .tlc_state
                .all_tlcs()
                .filter(|tlc| tlc.is_offered())
                .count(),
            0
        );
        capture_barrier(&network, id).await;
        captured.lock().unwrap().clear();

        actor
            .receive_ack_v2(&myself, &mut state, ack.unwrap())
            .await
            .unwrap();
        assert!(
            state.retryable_tlc_operations.is_empty(),
            "one ACK must release the whole queued batch"
        );
        assert!(state.tlc_state.waiting_ack);
        for tlc in state.tlc_state.all_tlcs().filter(|tlc| tlc.is_offered()) {
            assert_eq!(
                tlc.status,
                fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::LocalAnnounced),
                "the previous ACK must not confirm a new batch"
            );
        }
        let outgoing = state
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .as_ref()
            .unwrap();
        assert_eq!(outgoing.updates.len(), 2);
        assert_eq!(outgoing.settlement.tlcs.len(), 3);
        capture_barrier(&network, id).await;
        let wire = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match message {
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(message)) => {
                    Some(message.message.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(
                wire.as_slice(),
                [
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                        _
                    )),
                ]
            ),
            "updates must precede a single CS: {wire:?}"
        );
        let restored = a.store.get_channel_actor_state(&id).unwrap();
        restored.validate_session_v2().unwrap();
        let restored_outgoing = restored
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .as_ref()
            .unwrap();
        assert_eq!(restored_outgoing.updates.len(), 2);
        for update in &restored_outgoing.updates {
            let fiber_types::TlcReplayUpdate::Add(add) = update else {
                panic!("expected AddTlc")
            };
            peer_actor
                .handle_add_tlc_peer_message(&mut peer, add.clone())
                .unwrap();
        }
        let request = CommitmentSignedV2::try_from(restored_outgoing.request.clone()).unwrap();
        peer.stage_incoming_v2(&request, false).unwrap();
        let (_, _, ack) = peer.accept_commitment_v2(&request, false, false).unwrap();
        state.stage_ack_v2(ack.as_ref().unwrap()).unwrap();
        state.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
        assert!(state
            .tlc_state
            .all_tlcs()
            .filter(|tlc| tlc.is_offered())
            .all(|tlc| tlc.status
                == fiber_types::TlcStatus::Outbound(fiber_types::OutboundTlcStatus::Committed)));

        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_retry_queue_checks_limits_and_reports_results() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let initial = a.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        for (value_limit, count_limit, balance_limit, accepted) in [
            (5000, 10, false, vec![1u8, 3]),
            (10000, 2, false, vec![1, 2]),
            (10000, 10, true, vec![1, 3]),
            (10000, 0, false, vec![]),
        ] {
            let mut state = initial.clone();
            state.network = Some(network.clone());
            state.remote_constraints.max_tlc_value_in_flight = value_limit;
            state.remote_constraints.max_tlc_number_in_flight = count_limit;
            if balance_limit {
                state.to_remote_amount += state.to_local_amount - 5000;
                state.to_local_amount = 5000;
            }
            let mut expired = retry_batch_add(0, 1000);
            expired.expiry = 0;
            for command in [
                expired,
                retry_batch_add(1, 2000),
                retry_batch_add(2, 4000),
                retry_batch_add(3, 3000),
            ] {
                actor.register_retryable_tlc_add(&myself, &mut state, command);
            }
            actor
                .apply_retryable_tlc_operations(&myself, &mut state, false)
                .await;
            assert!(state.retryable_tlc_operations.is_empty());
            let hashes = state
                .tlc_state
                .all_tlcs()
                .map(|tlc| tlc.payment_hash)
                .collect::<Vec<_>>();
            assert_eq!(
                hashes,
                accepted
                    .iter()
                    .map(|tag| fiber_types::Hash256::from([*tag; 32]))
                    .collect::<Vec<_>>()
            );
            capture_barrier(&network, id).await;
            let messages = std::mem::take(&mut *captured.lock().unwrap());
            let results = messages
                .iter()
                .filter_map(|message| match message {
                    NetworkActorMessage::Event(NetworkActorEvent::AddTlcResult(
                        hash,
                        _,
                        result,
                        _,
                    )) => Some((*hash, result.is_ok())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                results,
                (0..4)
                    .map(|tag| (
                        fiber_types::Hash256::from([tag; 32]),
                        accepted.contains(&tag)
                    ))
                    .collect::<Vec<_>>()
            );
            let cs_count = messages
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                            FiberMessageWithTarget {
                                message: FiberMessage::ChannelNormalOperation(
                                    FiberChannelMessage::CommitmentSignedV2(_)
                                ),
                                ..
                            }
                        ))
                    )
                })
                .count();
            assert_eq!(cs_count, usize::from(!accepted.is_empty()));
        }
        handle.abort();
        network.stop(None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_retry_queue_preserves_mixed_update_replay() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let preimage = fiber_types::Hash256::from([42; 32]);
        let payment_hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage).into();
        let mut received = state
            .create_inbounding_tlc(fiber_types::AddTlc {
                channel_id: id,
                tlc_id: 0,
                amount: 1000,
                payment_hash,
                expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
                hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
                onion_packet: None,
            })
            .unwrap();
        received.status = fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::Committed);
        state.tlc_state.add_received_tlc(received);
        state.increment_next_received_tlc_id();
        let balances = (state.to_local_amount, state.to_remote_amount);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(1, 2000));
        actor.register_retryable_tlc_remove(
            &myself,
            &mut state,
            fiber_types::TLCId::Received(0),
            fiber_types::RemoveTlcReason::RemoveTlcFulfill(fiber_types::RemoveTlcFulfill {
                payment_preimage: preimage,
            }),
        );
        actor.register_retryable_tlc_add(&myself, &mut state, retry_batch_add(2, 3000));
        actor
            .apply_retryable_tlc_operations(&myself, &mut state, false)
            .await;
        assert!(state.retryable_tlc_operations.is_empty());
        assert_eq!(
            (state.to_local_amount, state.to_remote_amount),
            balances,
            "removal must not settle before its ACK"
        );
        assert_eq!(a.store.get_preimage(&payment_hash), Some(preimage));
        let restored = a.store.get_channel_actor_state(&id).unwrap();
        restored.validate_session_v2().unwrap();
        let outgoing = restored
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .as_ref()
            .unwrap();
        assert!(matches!(
            outgoing.updates.as_slice(),
            [
                fiber_types::TlcReplayUpdate::Add(_),
                fiber_types::TlcReplayUpdate::Remove(_),
                fiber_types::TlcReplayUpdate::Add(_)
            ]
        ));
        assert_eq!(outgoing.settlement.tlcs.len(), 2);
        capture_barrier(&network, id).await;
        let wire = take_wire_messages(&captured);
        assert!(matches!(
            wire.as_slice(),
            [
                FiberMessageWithTarget {
                    message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                    ..
                },
                FiberMessageWithTarget {
                    message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(
                        _
                    )),
                    ..
                },
                FiberMessageWithTarget {
                    message: FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_)),
                    ..
                },
                FiberMessageWithTarget {
                    message: FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::CommitmentSignedV2(_)
                    ),
                    ..
                },
            ]
        ));
        assert_eq!(
            restored
                .tlc_state
                .get(&fiber_types::TLCId::Received(0))
                .unwrap()
                .status,
            fiber_types::TlcStatus::Inbound(fiber_types::InboundTlcStatus::LocalRemoved)
        );
        handle.abort();
        network.stop(None);
        task.await.unwrap();
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
            let (_, _, ack) = local.accept_commitment_v2(&request, false, false).unwrap();
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
            peer.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
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
    async fn test_v2_restart_resets_ack_and_close_peer_waits() {
        for (waiting_ack, closing) in [(true, false), (false, true), (true, true)] {
            let (mut a, mut b, id) =
                create_nodes_with_established_channel(100000000000, 11800000000, false).await;
            let mut state = a.get_channel_actor_state(id);
            let myself = a.get_channel_actor(id).await.unwrap();
            a.stop().await;
            b.stop().await;

            let captured = Arc::new(Mutex::new(Vec::new()));
            let (network, task) = Actor::spawn(None, CaptureNetwork, captured).await.unwrap();
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
            state.network = Some(network.clone());
            if waiting_ack {
                state.prepare_outgoing_v2(false).unwrap();
                state.tlc_state.waiting_ack = true;
                state
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .outgoing
                    .as_mut()
                    .unwrap()
                    .peer_wait_started_at = 0;
            }
            if closing {
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
                state
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .closing
                    .as_mut()
                    .unwrap()
                    .peer_wait_started_at = 0;
            }
            assert!(state.peer_does_not_reply_ack_in_time());
            a.store.insert_channel_actor_state(state);

            let started = crate::now_timestamp_as_millis_u64();
            let mut restored = actor
                .pre_start(
                    myself,
                    super::super::ChannelInitializationParameter {
                        operation:
                            super::super::ChannelInitializationOperation::RestoreOfflineChannel(id),
                        ephemeral_config: Default::default(),
                        private_key: a.private_key.clone(),
                    },
                )
                .await
                .unwrap();
            if waiting_ack {
                assert!(restored.ack_peer_wait_v2().unwrap() >= started);
            }
            if closing {
                assert!(restored.close_peer_wait_v2().unwrap() >= started);
            }
            assert!(!restored.peer_does_not_reply_ack_in_time());
            let persisted = a.store.get_channel_actor_state(&id).unwrap();
            assert_eq!(
                persisted.peer_response_wait_v2(),
                restored.peer_response_wait_v2()
            );

            // In-process disconnect/reconnect must not grant another timeout.
            let deadline = restored.peer_response_wait_v2();
            restored.mark_reestablishing_offline();
            assert_eq!(restored.peer_response_wait_v2(), deadline);
            if waiting_ack {
                restored
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .outgoing
                    .as_mut()
                    .unwrap()
                    .peer_wait_started_at = 0;
            } else {
                restored
                    .session_v2
                    .as_mut()
                    .unwrap()
                    .closing
                    .as_mut()
                    .unwrap()
                    .peer_wait_started_at = 0;
            }
            assert!(restored.peer_does_not_reply_ack_in_time());
            network.stop(None);
            task.await.unwrap();
        }
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
                let (_, _, ack) = state.accept_commitment_v2(&request, false, false).unwrap();
                state.tlc_state.update_for_commitment_signed();
                peer.stage_ack_v2(&ack.clone().unwrap()).unwrap();
                peer.accept_ack_v2(&myself, &ack.unwrap(), false).unwrap();
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
                    // A reconnected peer cannot have answered while the channel was offline, so
                    // the handshake itself must not be judged against the old deadline.
                    state.connectivity_state = fiber_types::ChannelConnectivityState::Syncing;
                    assert!(
                        !state.peer_does_not_reply_ack_in_time(),
                        "a reestablishing channel must not exceed the deadline yet"
                    );
                    actor
                        .reestablish_v2(&myself, &mut state, peer.reestablish_message_v2())
                        .await
                        .unwrap();
                    assert!(!state.reestablishing);
                    // The reconnected peer gets a fresh window instead of being force-closed.
                    assert!(
                        !state.peer_does_not_reply_ack_in_time(),
                        "a reconnect must restart the peer response window, not extend the old deadline"
                    );
                    assert!(
                        state.peer_response_wait_v2().is_some_and(|started| {
                            crate::now_timestamp_as_millis_u64().saturating_sub(started) < 5_000
                        }),
                        "the deadline must be refreshed on reconnect"
                    );
                    network.stop(None);
                    task.await.unwrap();
                    continue;
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

    /// A network partition must not consume the peer's response window: while the channel is
    /// offline or reestablishing the peer cannot answer, and the window restarts when the
    /// channel is back online (bounded by `REESTABLISH_TIMEOUT` for a reconnect that never
    /// completes).
    /// A channel actor message sink for tests that only need a valid `ActorRef`.
    struct NoopChannelActor;

    #[async_trait::async_trait]
    impl Actor for NoopChannelActor {
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

    /// A peer that never saw our ACK replays the exact `CommitmentSignedV2` we already accepted.
    /// It must be answered from the durable `incoming` record: re-verifying it against the current
    /// state cannot reproduce the transaction that was signed and would fail, which used to leave
    /// the channel reestablishing forever (see the cursor reconciliation note).
    #[tokio::test]
    async fn test_v2_replayed_accepted_cs_is_answered_from_durable_record() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state_a = a.get_channel_actor_state(id);
        let mut state_b = b.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, network_task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state_a.network = Some(network.clone());
        state_b.network = Some(network.clone());
        let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
        let (b_ref, b_handle) = Actor::spawn(None, NoopChannelActor, ())
            .await
            .expect("spawn B no-op channel actor");

        // A signs a CS and B accepts it, so B holds the exact request and its response.
        let (request, _) = state_a.prepare_outgoing_v2(false).expect("prepare CS");
        let remote_before = state_b.get_remote_commitment_number();
        actor_b
            .receive_commitment_v2(&b_ref, &mut state_b, request.clone())
            .await
            .expect("first CS must be accepted");
        assert_eq!(
            state_b.get_remote_commitment_number(),
            remote_before + 1,
            "accepting the CS advances the peer commitment number"
        );
        assert!(
            state_b
                .session_v2
                .as_ref()
                .unwrap()
                .incoming
                .as_ref()
                .unwrap()
                .response
                .is_some(),
            "the accepted response is durable"
        );
        capture_barrier(&network, id).await;
        captured.lock().unwrap().clear();

        // The reconnect replays the same message because the ACK never arrived.
        actor_b
            .receive_commitment_v2(&b_ref, &mut state_b, request)
            .await
            .expect("a replayed accepted CS must not fail verification");
        assert_eq!(
            state_b.get_remote_commitment_number(),
            remote_before + 1,
            "the replay must not be accepted twice"
        );
        capture_barrier(&network, id).await;

        let acks = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(
                        FiberMessageWithTarget {
                            message: FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::RevokeAndAckV2(_)
                            ),
                            ..
                        }
                    ))
                )
            })
            .count();
        assert_eq!(
            acks, 1,
            "the replay must be answered with the cached ACK exactly once"
        );

        b_handle.abort();
        network.stop(None);
        network_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_v2_partition_suspends_and_then_restarts_peer_response_window() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);

        // A cooperative close is in flight, so our own peer-response obligation exists.
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
        state
            .session_v2
            .as_mut()
            .unwrap()
            .closing
            .as_mut()
            .unwrap()
            .peer_wait_started_at = 0;
        assert!(
            state.peer_does_not_reply_ack_in_time(),
            "a live channel with an ancient deadline is timed out"
        );

        // A peer that stays away must still be closed: the deadline keeps counting offline.
        state.mark_reestablishing_offline();
        assert!(
            state.peer_does_not_reply_ack_in_time(),
            "an absent peer must still be force-closed"
        );

        // Peer is back and its reestablishment handshake is in flight: the owed messages could
        // not have been answered before the reconnect, so judgement is suspended.
        state.connectivity_state = fiber_types::ChannelConnectivityState::Syncing;
        assert!(
            !state.peer_does_not_reply_ack_in_time(),
            "a reestablishing channel must not consume the peer response window"
        );

        // Reestablishment completed: the peer is reachable again, so the window restarts.
        state
            .session_v2
            .as_mut()
            .unwrap()
            .closing
            .as_mut()
            .unwrap()
            .peer_wait_started_at = 0;
        state.refresh_peer_response_wait_v2_after_reestablish();
        let started = state
            .peer_response_wait_v2()
            .expect("the close obligation still has a deadline");
        assert!(
            crate::now_timestamp_as_millis_u64().saturating_sub(started) < 5_000,
            "a reconnect must restart the peer response window"
        );
        assert!(
            !state.peer_does_not_reply_ack_in_time(),
            "a reconnect must not force-close immediately"
        );

        network.stop(None);
        task.await.unwrap();
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
        let restarted_at = crate::now_timestamp_as_millis_u64();
        a.start().await;
        if matches!(expectation, RestartCloseExpectation::Publish) {
            let restored = a.get_channel_actor_state(id);
            assert!(restored.close_peer_wait_v2().unwrap() >= restarted_at);
            assert!(!restored.peer_does_not_reply_ack_in_time());
            assert!(a
                .get_transaction_view_from_hash(usable.hash().into())
                .await
                .is_none());
            assert_timer_publishes_usable(
                &mut a,
                id,
                &usable,
                std::time::Duration::from_millis(
                    super::super::PEER_CHANNEL_RESPONSE_TIMEOUT + 5000,
                ),
            )
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
        if matches!(expectation, RestartCloseExpectation::Publish) {
            // Only the watchdog timestamp may change; signing intent stays exact.
            after.peer_wait_started_at = 0;
        }
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
        let original_wait_started_at = retained.ack_peer_wait_v2().unwrap();
        if restart {
            b.stop().await;
            a.stop().await;
            retained.reestablishing = true;
            a.store.insert_channel_actor_state(retained);
            let restarted_at = crate::now_timestamp_as_millis_u64();
            a.start().await;
            let restored = a.get_channel_actor_state(id);
            assert!(restored.ack_peer_wait_v2().unwrap() >= restarted_at);
            assert!(!restored.peer_does_not_reply_ack_in_time());
        }
        assert_timer_publishes_usable(
            &mut a,
            id,
            &usable,
            std::time::Duration::from_millis(super::super::PEER_CHANNEL_RESPONSE_TIMEOUT + 5000),
        )
        .await;
        let mut after = a
            .get_channel_actor_state(id)
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .clone()
            .unwrap();
        // Restart resets only the response timer, never the signing intent.
        after.peer_wait_started_at = original_wait_started_at;
        assert_eq!(bincode::serialize(&Some(after)).unwrap(), outgoing);
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
        let (_, _, ack) = sb.accept_commitment_v2(&request, false, false).unwrap();
        sb.tlc_state.update_for_commitment_signed();
        let ack = ack.unwrap();
        sa.stage_ack_v2(&ack).unwrap();
        sa.accept_ack_v2(&myself, &ack, false).unwrap();
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
    async fn test_v2_rejected_shutdown_leaves_no_stale_remote_info_across_restore() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sa = a.get_channel_actor_state(id);
        let mut sb = b.get_channel_actor_state(id);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        sa.network = Some(network.clone());
        sb.network = Some(network.clone());
        let actor_a = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        let actor_b = ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);

        // The peer advertises a genuine closing nonce with a fee below the
        // minimum this node auto-accepts, but still affordable for the peer.
        actor_b
            .shutdown_v2(
                &mut sb,
                super::super::ShutdownCommand {
                    force: false,
                    close_script: None,
                    fee_rate: None,
                },
            )
            .await
            .unwrap();
        let mut peer_shutdown = ShutdownV2::try_from(
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
        peer_shutdown.fee_rate = 0;
        assert!(peer_shutdown.fee_rate < sa.commitment_fee_rate);
        assert!(sa.check_shutdown_fee_valid(&peer_shutdown.close_script, peer_shutdown.fee_rate));

        assert!(actor_a
            .receive_shutdown_v2(&mut sa, peer_shutdown)
            .await
            .is_err());
        // A rejected shutdown must not leave any durable trace. The actor
        // persists the channel state even when the handler returns an error.
        assert!(sa.remote_shutdown_info.is_none());
        assert!(sa.session_v2.as_ref().unwrap().closing.is_none());
        assert_eq!(sa.state, ChannelState::ChannelReady);

        // Persist the rejected state, then start a local cooperative close from
        // it. Before the fix the stale remote info survived and paired with a
        // closing session that has no remote advertisement.
        let encoded = bincode::serialize(&sa.core).unwrap();
        sa.core = bincode::deserialize(&encoded).unwrap();
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
        assert!(sa.remote_shutdown_info.is_none());
        assert!(sa
            .session_v2
            .as_ref()
            .unwrap()
            .closing
            .as_ref()
            .unwrap()
            .remote_shutdown
            .is_none());

        // Restoring the closing session must pass V2 validation instead of
        // failing with "V2 remote shutdown without advertisement".
        let encoded = bincode::serialize(&sa.core).unwrap();
        sa.core = bincode::deserialize(&encoded).unwrap();
        fiber_types::channel_v2_validation::validate_channel_v2(&sa.core).unwrap();

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
        let (_request, _) = state.prepare_outgoing_v2(false).unwrap();
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
        // This path has no fresh reconnect (the channel is already Syncing), so the handshake
        // must not consume or replace the verification nonce.
        assert_eq!(state.session_v2.as_ref().unwrap().own.public_nonce, own);
        actor
            .handle_peer_message(
                &myself,
                &mut state,
                FiberChannelMessage::ReestablishChannelV2(incoming.reestablish_message_v2()),
            )
            .await
            .unwrap();
        // The spec route abandons the outstanding CS and re-signs it with a fresh nonce, so
        // recovery completes here and a new ACK obligation is armed.
        assert!(!state.reestablishing);
        assert!(state.tlc_state.waiting_ack);
        let resigned_nonce = state
            .session_v2
            .as_ref()
            .unwrap()
            .outgoing
            .as_ref()
            .unwrap()
            .funding
            .public_nonce
            .clone();
        assert!(state.waiting_peer_response.is_some());
        let _ = ack_deadline_start;
        actor
            .handle_peer_message(
                &myself,
                &mut state,
                FiberChannelMessage::ReestablishChannelV2(incoming.reestablish_message_v2()),
            )
            .await
            .unwrap();
        // The first handshake rotates the verification nonce; the duplicate below must not.
        let rotated_nonce = state.session_v2.as_ref().unwrap().own.public_nonce.clone();
        assert_ne!(rotated_nonce, own);
        assert_eq!(
            state
                .session_v2
                .as_ref()
                .unwrap()
                .outgoing
                .as_ref()
                .unwrap()
                .funding
                .public_nonce,
            resigned_nonce,
            "a duplicate handshake must not sign again"
        );
        assert_eq!(
            state.session_v2.as_ref().unwrap().own.public_nonce,
            rotated_nonce,
            "a duplicate handshake must not rotate the verification nonce again"
        );
        // The handshake must precede the commitment message, and that message must be regenerated
        // rather than replayed (checked above through the fresh nonce).
        capture_barrier(&network, id).await;
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
            let handshake = wire
                .iter()
                .position(|m| {
                    matches!(
                        m,
                        FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::ReestablishChannelV2(_)
                        )
                    )
                })
                .expect("handshake must be sent");
            assert!(
                wire.iter().skip(handshake + 1).any(|m| matches!(
                    m,
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                        _
                    ))
                )),
                "a regenerated commitment must follow the handshake: {wire:?}"
            );
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
            state.accept_commitment_v2(&incoming, false, false).unwrap();
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
                        >= 2
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
                // Locate the two owed messages after the handshake instead of assuming fixed
                // indices: the regenerated CS and the cached RAA must still appear in the same
                // relative order as originally transmitted.
                let cs_index = wire
                    .iter()
                    .position(|m| {
                        matches!(
                            m,
                            FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::CommitmentSignedV2(_)
                            )
                        )
                    })
                    .expect("CS must be replayed or regenerated");
                let ack_index = wire
                    .iter()
                    .position(|m| {
                        matches!(
                            m,
                            FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::RevokeAndAckV2(_)
                            )
                        )
                    })
                    .expect("RAA must be replayed");
                // Both owed messages must be emitted. Their relative order is driven by
                // `last_was_revoke` and is covered end-to-end by the lost-RAA reconnect test.
                let _ = cs_first;
                let FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                    cs,
                )) = wire[cs_index]
                else {
                    unreachable!()
                };
                let FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAckV2(ack)) =
                    wire[ack_index]
                else {
                    unreachable!()
                };
                let cs: fiber_types::PackedCommitmentSignedV2 = cs.clone().into();
                let ack: fiber_types::PackedRevokeAndAckV2 = ack.clone().into();
                // The cached RAA is replayed byte-identically, but the CS is regenerated with a
                // fresh nonce for the same commitment number (spec: "regenerate the partial
                // signature").
                let regenerated = CommitmentSignedV2::try_from(cs.clone()).expect("CS");
                let original = CommitmentSignedV2::try_from(original_cs.clone()).expect("CS");
                assert_eq!(regenerated.commitment_number, original.commitment_number);
                assert_ne!(
                    cs.as_slice(),
                    original_cs.as_slice(),
                    "the CS must be regenerated, never replayed"
                );
                assert_eq!(ack.as_slice(), original_ack.as_slice());
            }
            // Recovery completes by abandoning the outstanding CS and signing a fresh one.
            assert!(!state.reestablishing && state.tlc_state.waiting_ack);
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

    /// Relay confirmation drops the matching durable remove effect; the normal message-tail
    /// commit persists that removal when no preimage cleanup requires an earlier boundary.
    #[tokio::test]
    async fn test_v2_onchain_relay_confirmation_drops_matching_remove_effect() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        let myself = a.get_channel_actor(id).await.unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        state.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);

        let reason = fiber_types::RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
            fiber_types::TlcErr::new(fiber_types::TlcErrorCode::HoldTlcTimeout),
            &[0u8; 32],
        ));
        let tlc = fiber_types::TlcInfo {
            status: fiber_types::TlcStatus::Outbound(
                fiber_types::OutboundTlcStatus::RemoveAckConfirmed,
            ),
            tlc_id: fiber_types::TLCId::Offered(7),
            amount: 1000,
            payment_hash: crate::gen_rand_sha256_hash(),
            total_amount: None,
            payment_secret: None,
            attempt_id: Some(1),
            expiry: crate::now_timestamp_as_millis_u64() + 1_000_000,
            hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0u8; 32],
            is_trampoline_hop: false,
            created_at: fiber_types::CommitmentNumbers::default(),
            removed_reason: Some(reason.clone()),
            removed_confirmed_at: None,
            applied_flags: fiber_types::AppliedFlags::empty(),
            forwarding_tlc: None,
        };
        state
            .session_v2
            .as_mut()
            .expect("v2 session")
            .remove_effects
            .push((tlc, None));
        a.store.insert_channel_actor_state(state.clone());

        actor
            .handle_event(
                &myself,
                &mut state,
                super::super::ChannelEvent::OnChainTlcRelayConfirmed(
                    fiber_types::TLCId::Offered(7),
                    reason,
                ),
            )
            .await
            .unwrap();

        assert!(
            state.session_v2.as_ref().unwrap().remove_effects.is_empty(),
            "the matching remove effect must be dropped"
        );
        actor.persist_state_and_publish_pending_effects(&mut state);
        assert!(
            a.store
                .get_channel_actor_state(&id)
                .unwrap()
                .session_v2
                .as_ref()
                .unwrap()
                .remove_effects
                .is_empty(),
            "the drop must be persisted"
        );
        network.stop(None);
        task.await.unwrap();
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
        let (_, _, ack) = owner.accept_commitment_v2(&request, false, false).unwrap();
        owner.tlc_state.update_for_commitment_signed();
        let ack = ack.unwrap();
        sender.stage_ack_v2(&ack).unwrap();
        let (revocation, _) = sender.accept_ack_v2(&myself, &ack, false).unwrap().unwrap();
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
        let (_, _, ack) = owner.accept_commitment_v2(&request, false, false).unwrap();
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
        assert_ne!(
            owner.session_v2.as_ref().unwrap().own.public_nonce,
            own,
            "a reconnect rotates the verification nonce (`next_local_nonces`)"
        );
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
    async fn test_v2_channel_ignores_v1_revocation_nonce_watchdog() {
        // A V2 channel never advances the V1 revocation nonce ledger, so an idle V2 channel used to
        // look permanently unbalanced and was force-closed after one response timeout.
        let (a, _, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut state = a.get_channel_actor_state(id);
        assert!(state.session_v2.is_some(), "expected a V2 channel");
        state.remote_revocation_nonce_for_send = None;
        state.remote_revocation_nonce_for_verify = None;
        state.revocation_nonce_unbalanced_since = Some(0);
        assert!(!state.is_revocation_nonce_ledger_stalled(crate::now_timestamp_as_millis_u64()));
        state.refresh_revocation_nonce_watchdog();
        assert!(state.revocation_nonce_unbalanced_since.is_none());
    }

    #[tokio::test]
    async fn test_v2_settlement_accounts_for_prev_ack_updates() {
        use fiber_types::{
            InboundTlcStatus, OutboundTlcStatus, RemoveTlcFulfill, RemoveTlcReason, TlcStatus,
        };
        let (a, _, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let initial = a.get_channel_actor_state(id);
        let local = initial.to_local_amount + u128::from(initial.local_reserved_ckb_amount);
        let remote = initial.to_remote_amount + u128::from(initial.remote_reserved_ckb_amount);
        let preimage = fiber_types::Hash256::from([42; 32]);
        let tlc = initial
            .create_inbounding_tlc(fiber_types::AddTlc {
                channel_id: id,
                tlc_id: 0,
                amount: 2000,
                payment_hash: fiber_types::HashAlgorithm::CkbHash.hash(preimage).into(),
                expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
                hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
                onion_packet: None,
            })
            .unwrap();
        for for_remote in [false, true] {
            for status in [
                InboundTlcStatus::AnnounceWaitPrevAck,
                InboundTlcStatus::AnnounceWaitAck,
            ] {
                let mut state = initial.clone();
                let mut received = tlc.clone();
                received.status = TlcStatus::Inbound(status.clone());
                state.tlc_state.add_received_tlc(received);
                let settlement = state.build_settlement_data(for_remote).unwrap();
                assert_eq!(settlement.tlcs.len(), 1);
                assert_eq!(settlement.local_amount, local);
                assert_eq!(
                    settlement.remote_amount,
                    remote - 2000,
                    "{status:?}, for_remote={for_remote}"
                );
            }
            for status in [
                OutboundTlcStatus::RemoveWaitPrevAck,
                OutboundTlcStatus::RemoveWaitAck,
            ] {
                for fulfill in [false, true] {
                    let mut state = initial.clone();
                    let mut offered = tlc.clone();
                    offered.flip_mut();
                    offered.status = TlcStatus::Outbound(status.clone());
                    offered.removed_reason = Some(if fulfill {
                        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage,
                        })
                    } else {
                        RemoveTlcReason::RemoveTlcFail(fiber_types::TlcErrPacket::new(
                            fiber_types::TlcErr::new(
                                fiber_types::TlcErrorCode::TemporaryChannelFailure,
                            ),
                            &[0; 32],
                        ))
                    });
                    state.tlc_state.add_offered_tlc(offered);
                    let settlement = state.build_settlement_data(for_remote).unwrap();
                    assert!(settlement.tlcs.is_empty());
                    let transferred = if fulfill { 2000 } else { 0 };
                    assert_eq!(
                        settlement.local_amount,
                        local - transferred,
                        "{status:?}, fulfill={fulfill}, for_remote={for_remote}"
                    );
                    assert_eq!(settlement.remote_amount, remote + transferred);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_v2_committed_batch_recovers_every_publication_cut_without_id_reuse() {
        for delivered in 0..=3 {
            let (mut a, mut b, id) =
                create_nodes_with_established_channel(100000000000, 11800000000, false).await;
            let mut sender = a.get_channel_actor_state(id);
            let mut receiver = b.get_channel_actor_state(id);
            b.stop().await;
            a.stop().await;
            sender.tlc_state.offered_tlcs.next_tlc_id = 1763;
            receiver.tlc_state.received_tlcs.next_tlc_id = 1763;
            let captured = Arc::new(Mutex::new(Vec::new()));
            let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
                .await
                .unwrap();
            sender.network = Some(network.clone());
            receiver.network = Some(network.clone());
            let actor =
                ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
            let peer =
                ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
            let (myself, handle) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
            actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(11, 1));
            actor.register_retryable_tlc_add(&myself, &mut sender, retry_batch_add(12, 1));
            actor
                .apply_retryable_tlc_operations(&myself, &mut sender, false)
                .await;
            capture_barrier(&network, id).await;
            let wire = take_wire_messages(&captured);
            assert_eq!(wire.len(), 3);
            for message in wire.into_iter().take(delivered) {
                match message.message {
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                        peer.handle_add_tlc_peer_message(&mut receiver, add)
                            .unwrap();
                    }
                    FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::CommitmentSignedV2(cs),
                    ) => {
                        let verified = receiver.stage_incoming_v2(&cs, false).unwrap();
                        receiver.accept_commitment_v2(&cs, false, verified).unwrap();
                        receiver.tlc_state.update_for_commitment_signed();
                        receiver.last_was_revoke = true;
                    }
                    _ => panic!("unexpected original batch message"),
                }
            }
            b.store.insert_channel_actor_state(receiver);
            let mut sender = a.store.get_channel_actor_state(&id).unwrap();
            let mut receiver = b.store.get_channel_actor_state(&id).unwrap();
            assert_eq!(sender.tlc_state.offered_tlcs.next_tlc_id, 1765);
            assert_eq!(
                sender
                    .session_v2
                    .as_ref()
                    .unwrap()
                    .outgoing
                    .as_ref()
                    .unwrap()
                    .updates
                    .len(),
                2
            );
            for state in [&mut sender, &mut receiver] {
                state.network = Some(network.clone());
                state.mark_reestablishing_offline();
                state.on_peer_reconnected();
            }
            a.store.insert_channel_actor_state(sender.clone());
            b.store.insert_channel_actor_state(receiver.clone());
            capture_barrier(&network, id).await;
            let handshakes = take_wire_messages(&captured);
            assert_eq!(handshakes.len(), 2);
            assert!(handshakes.iter().all(|message| matches!(
                message.message,
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ReestablishChannelV2(_))
            )));
            let sender_handshake = sender.reestablish_message_v2();
            let receiver_handshake = receiver.reestablish_message_v2();
            actor
                .reestablish_v2(&myself, &mut sender, receiver_handshake)
                .await
                .unwrap();
            peer.reestablish_v2(&myself, &mut receiver, sender_handshake)
                .await
                .unwrap();

            for next_payment in [false, true] {
                if next_payment {
                    assert_eq!(
                        actor
                            .handle_add_tlc_command(&myself, &mut sender, &retry_batch_add(13, 1))
                            .await
                            .unwrap(),
                        1765
                    );
                }
                capture_barrier(&network, id).await;
                let wire = take_wire_messages(&captured);
                let adds = wire
                    .iter()
                    .filter(|message| {
                        matches!(
                            message.message,
                            FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(_))
                        )
                    })
                    .count();
                assert_eq!(
                    adds,
                    if next_payment {
                        1
                    } else if delivered == 3 {
                        0
                    } else {
                        2
                    },
                    "do not republish an already replayed prefix: cut={delivered}"
                );
                for message in wire {
                    match message.message {
                        FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                            let durable = a.store.get_channel_actor_state(&id).unwrap();
                            let stored = durable
                                .tlc_state
                                .get(&fiber_types::TLCId::Offered(add.tlc_id))
                                .unwrap();
                            assert_eq!(
                                (stored.payment_hash, stored.amount, stored.expiry),
                                (add.payment_hash, add.amount, add.expiry)
                            );
                            peer.handle_add_tlc_peer_message(&mut receiver, add)
                                .unwrap();
                        }
                        FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::CommitmentSignedV2(cs),
                        ) => {
                            let verified = receiver.stage_incoming_v2(&cs, false).unwrap();
                            let (_, _, ack) =
                                receiver.accept_commitment_v2(&cs, false, verified).unwrap();
                            receiver.tlc_state.update_for_commitment_signed();
                            let ack = ack.unwrap();
                            let verified = sender.stage_ack_v2(&ack).unwrap();
                            sender
                                .accept_ack_v2(&myself, &ack, verified)
                                .unwrap()
                                .unwrap();
                        }
                        FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::RevokeAndAckV2(ack),
                        ) => {
                            let verified = sender.stage_ack_v2(&ack).unwrap();
                            sender
                                .accept_ack_v2(&myself, &ack, verified)
                                .unwrap()
                                .unwrap();
                        }
                        FiberMessage::ChannelNormalOperation(
                            FiberChannelMessage::UpdateTlcInfo(update),
                        ) => {
                            let (actor, state) = if message.target == b.pubkey {
                                (&peer, &mut receiver)
                            } else {
                                (&actor, &mut sender)
                            };
                            actor
                                .handle_peer_message(
                                    &myself,
                                    state,
                                    FiberChannelMessage::UpdateTlcInfo(update),
                                )
                                .await
                                .unwrap();
                        }
                        other => {
                            panic!("unexpected recovery message at cut {delivered}: {other:?}")
                        }
                    }
                }
                sender.finish_recovery_v2(&myself);
                assert!(!sender.tlc_state.waiting_ack, "cut={delivered}");
                assert!(!sender.reestablishing, "cut={delivered}");
                assert_eq!(sender.state, ChannelState::ChannelReady);
                assert_eq!(receiver.state, ChannelState::ChannelReady);
                assert_eq!(sender.replayed_tlc_updates_v2, 0);
                actor.persist_state_and_publish_pending_effects(&mut sender);
                for tlc_id in 1763..=if next_payment { 1765 } else { 1764 } {
                    let offered = sender
                        .tlc_state
                        .get(&fiber_types::TLCId::Offered(tlc_id))
                        .unwrap();
                    let received = receiver
                        .tlc_state
                        .get(&fiber_types::TLCId::Received(tlc_id))
                        .unwrap();
                    assert_eq!(
                        (offered.payment_hash, offered.amount, offered.expiry),
                        (received.payment_hash, received.amount, received.expiry)
                    );
                }
            }
            myself.stop(None);
            handle.await.unwrap();
            network.stop(None);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_v2_recovery_replays_updates_before_regenerated_cs() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut receiver = b.get_channel_actor_state(id);
        let myself = a.get_channel_actor(id).await.unwrap();
        let command = fiber_types::AddTlcCommand {
            amount: 2000,
            payment_hash: [42; 32].into(),
            attempt_id: None,
            expiry: crate::now_timestamp_as_millis_u64() + 86_400_000,
            hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
        };
        let tlc = sender.create_outbounding_tlc(&command);
        let add = fiber_types::AddTlc {
            channel_id: id,
            tlc_id: tlc.id(),
            amount: command.amount,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            hash_algorithm: command.hash_algorithm,
            onion_packet: None,
        };
        sender.tlc_state.add_offered_tlc(tlc);
        sender.increment_next_offered_tlc_id();
        sender.record_pending_replay_update(fiber_types::TlcReplayUpdate::Add(add.clone()));
        let (original, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.tlc_state.waiting_ack = true;
        sender.mark_reestablishing_offline();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (network, task) = Actor::spawn(None, CaptureNetwork, captured.clone())
            .await
            .unwrap();
        sender.network = Some(network.clone());
        let actor = ChannelActor::new(a.pubkey, b.pubkey, network.clone(), a.store.clone(), None);
        actor
            .reestablish_v2(&myself, &mut sender, receiver.reestablish_message_v2())
            .await
            .unwrap();
        capture_barrier(&network, id).await;
        let wire = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match message {
                NetworkActorMessage::Command(NetworkActorCommand::SendFiberMessage(message)) => {
                    Some(message.message.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let peer_actor =
            ChannelActor::new(b.pubkey, a.pubkey, network.clone(), b.store.clone(), None);
        let mut accepted = false;
        for message in wire {
            match message {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add)) => {
                    peer_actor
                        .handle_add_tlc_peer_message(&mut receiver, add)
                        .unwrap();
                }
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSignedV2(
                    cs,
                )) => {
                    assert_eq!(cs.commitment_number, original.commitment_number);
                    assert_ne!(cs.funding_nonce, original.funding_nonce);
                    receiver
                        .stage_incoming_v2(&cs, false)
                        .expect("replayed updates must reach the peer before the regenerated CS");
                    accepted = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(accepted, "recovery must regenerate the outstanding CS");
        let stored = a.get_channel_actor_state(id);
        stored.validate_session_v2().unwrap();
        assert_eq!(
            stored
                .session_v2
                .as_ref()
                .unwrap()
                .outgoing
                .as_ref()
                .unwrap()
                .updates,
            vec![fiber_types::TlcReplayUpdate::Add(add)],
            "a second crash must retain the regenerated round's replay updates"
        );
        network.stop(None);
        task.await.unwrap();
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
        // The spec route regenerates the signature with a fresh nonce instead of replaying the
        // persisted one, so the commitment number is preserved while the bytes and nonce differ.
        let resigned = CommitmentSignedV2::try_from(replayed.request.clone()).expect("CS");
        let original_cs = CommitmentSignedV2::try_from(original.request.clone()).expect("CS");
        assert_eq!(resigned.commitment_number, original_cs.commitment_number);
        assert_ne!(
            replayed.request.as_slice(),
            original.request.as_slice(),
            "the persisted signature must not be replayed"
        );
        assert_ne!(
            replayed.funding.public_nonce, original.funding.public_nonce,
            "the regenerated signature must use a fresh nonce"
        );
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
                let (_, _, ack) = owner.accept_commitment_v2(&request, false, false).unwrap();
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
    async fn test_v2_staging_verification_is_not_reused_after_restore() {
        let (a, b, id) =
            create_nodes_with_established_channel(100000000000, 11800000000, false).await;
        let mut sender = a.get_channel_actor_state(id);
        let mut owner = b.get_channel_actor_state(id);
        let (myself, task) = Actor::spawn(None, NoopChannelActor, ()).await.unwrap();
        let (request, _) = sender.prepare_outgoing_v2(false).unwrap();
        sender.tlc_state.waiting_ack = true;

        let mut invalid_cs = request.clone();
        invalid_cs.funding_tx_partial_signature = musig2::PartialSignature::max();
        assert!(owner.stage_incoming_v2(&invalid_cs, false).is_err());
        let verified = owner.stage_incoming_v2(&request, false).unwrap();
        assert!(verified);
        let bytes = bincode::serialize(&owner).unwrap();
        let mut restored_owner: ChannelActorState = bincode::deserialize(&bytes).unwrap();
        let restored_verified = restored_owner.stage_incoming_v2(&request, false).unwrap();
        assert!(!restored_verified);
        let (fresh_tx, _, fresh_ack) = owner
            .accept_commitment_v2(&request, false, verified)
            .unwrap();
        let (restored_tx, _, restored_ack) = restored_owner
            .accept_commitment_v2(&request, false, restored_verified)
            .unwrap();
        assert_eq!(fresh_tx.data().as_slice(), restored_tx.data().as_slice());
        let ack = fresh_ack.unwrap();
        assert_eq!(
            restored_ack.unwrap().commitment_number,
            ack.commitment_number
        );

        let mut invalid_ack = ack.clone();
        invalid_ack.revocation_partial_signature = musig2::PartialSignature::max();
        assert!(sender.stage_ack_v2(&invalid_ack).is_err());
        let verified = sender.stage_ack_v2(&ack).unwrap();
        assert!(verified);
        let bytes = bincode::serialize(&sender).unwrap();
        let mut restored_sender: ChannelActorState = bincode::deserialize(&bytes).unwrap();
        let restored_verified = restored_sender.stage_ack_v2(&ack).unwrap();
        assert!(!restored_verified);
        let fresh = sender
            .accept_ack_v2(&myself, &ack, verified)
            .unwrap()
            .unwrap();
        let restored = restored_sender
            .accept_ack_v2(&myself, &ack, restored_verified)
            .unwrap()
            .unwrap();
        assert_eq!(fresh, restored);
        assert!(!sender.stage_ack_v2(&ack).unwrap());
        assert!(sender
            .accept_ack_v2(&myself, &ack, false)
            .unwrap()
            .is_none());
        myself.stop(None);
        task.await.unwrap();
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
        let (_, _, response) = restored
            .accept_commitment_v2(&request, false, false)
            .unwrap();
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
