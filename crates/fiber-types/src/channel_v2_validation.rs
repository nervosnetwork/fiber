//! Validation of the actual current persisted channel schema before database writes.
use crate::{
    ChannelActorData, NoncePurposeV2, Privkey, Pubkey, RevocationContextV2, SigningNonceV2,
};
use ckb_types::{packed, prelude::*};
use molecule::prelude::Entity;
use musig2::{
    AggNonce, BinaryEncoding, KeyAggContext, PartialSignature, PubNonce, SecNonce, SecNonceBuilder,
};

/// Canonical funding witness prefix required for XUDT compatibility.
pub const XUDT_COMPATIBLE_WITNESS: [u8; 16] = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];

/// Encode the complete funding-lock witness, shared by publication and preflight.
pub fn canonical_funding_witness(key: [u8; 32], signature: musig2::CompactSignature) -> [u8; 112] {
    let mut witness = [0; 112];
    witness[..16].copy_from_slice(&XUDT_COMPATIBLE_WITNESS);
    witness[16..48].copy_from_slice(&key);
    witness[48..].copy_from_slice(&signature.serialize());
    witness
}

/// Regenerate only the nonce allocated by this durable record, never a new session.
pub fn secret_nonce_v2(record: &SigningNonceV2, key: &Privkey) -> SecNonce {
    SecNonceBuilder::new(record.seed)
        .with_seckey(key)
        .with_extra_input(&record.channel_id)
        .with_extra_input(&record.owner.serialize())
        .with_extra_input(&record.number.to_be_bytes())
        .with_extra_input(&[match record.purpose {
            NoncePurposeV2::Commitment => 0,
            NoncePurposeV2::Revocation => 1,
            NoncePurposeV2::Closing => 2,
        }])
        .build()
}

fn tx_message(tx: &packed::Transaction) -> [u8; 32] {
    ckb_hash::blake2b_256(
        tx.raw()
            .as_builder()
            .cell_deps(packed::CellDepVec::default())
            .build()
            .as_slice(),
    )
}

fn check(ok: bool, message: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn nonce(
    record: &SigningNonceV2,
    key: &Privkey,
    owner: Pubkey,
    purpose: NoncePurposeV2,
    number: u64,
) -> Result<(), String> {
    check(
        record.owner == owner && record.purpose == purpose && record.number == number,
        "V2 nonce owner/purpose/number mismatch",
    )?;
    check(
        record.public_nonce == secret_nonce_v2(record, key).public_nonce(),
        "V2 nonce seed/key/public mismatch",
    )?;
    check(
        record.signature.is_none() || record.context.is_some(),
        "V2 signature without context",
    )
}

fn context(
    record: &SigningNonceV2,
    key: &Privkey,
    keys: [Pubkey; 2],
    message: [u8; 32],
    peer_nonce: Option<PubNonce>,
    required: bool,
) -> Result<(), String> {
    let Some(bytes) = &record.context else {
        return check(
            !required && record.signature.is_none(),
            "Missing V2 signing context",
        );
    };
    let prefix = [
        keys[0].serialize().as_slice(),
        keys[1].serialize().as_slice(),
    ]
    .concat();
    check(
        bytes.len() == 164 && bytes[..66] == prefix && bytes[132..] == message,
        "V2 ordered keys/message context mismatch",
    )?;
    let agg_nonce = AggNonce::from_bytes(&bytes[66..132]).map_err(|e| e.to_string())?;
    if let Some(peer) = peer_nonce {
        check(
            agg_nonce == AggNonce::sum([record.public_nonce.clone(), peer]),
            "V2 aggregate nonce context mismatch",
        )?;
    }
    if let Some(signature) = record.signature {
        let agg = KeyAggContext::new(keys).map_err(|e| e.to_string())?;
        musig2::verify_partial(
            &agg,
            signature,
            &agg_nonce,
            key.pubkey(),
            &record.public_nonce,
            message,
        )
        .map_err(|e| format!("Invalid V2 cached local signature: {e}"))?;
    }
    Ok(())
}

fn revocation(
    channel: &ChannelActorData,
    snapshot: &RevocationContextV2,
    keys: [Pubkey; 2],
    number: u64,
) -> Result<(), String> {
    let agg = KeyAggContext::new(keys).map_err(|e| e.to_string())?;
    let xonly = agg
        .aggregated_pubkey::<musig2::secp::Point>()
        .serialize_xonly();
    let old = number
        .checked_sub(1)
        .ok_or("V2 revocation number underflow")?;
    let delay = (channel.commitment_delay_epoch | 0xa000000000000000).to_le_bytes();
    let args = [
        &ckb_hash::blake2b_256(xonly)[..20],
        &delay,
        &old.to_be_bytes(),
    ]
    .concat();
    check(
        snapshot.message
            == ckb_hash::blake2b_256(
                [
                    snapshot.output.as_slice(),
                    snapshot.output_data.as_slice(),
                    &args,
                ]
                .concat(),
            ),
        "V2 revocation snapshot hash mismatch",
    )
}

fn commitment_snapshot(
    channel: &ChannelActorData,
    tx: &packed::Transaction,
    settlement: &crate::SettlementData,
    for_remote: bool,
    number: u64,
) -> Result<(), String> {
    let remote = channel
        .remote_channel_public_keys
        .as_ref()
        .ok_or("Missing V2 commitment keys")?;
    let local = channel.signer.funding_key.pubkey();
    let keys = if for_remote {
        [local, remote.funding_pubkey]
    } else {
        [remote.funding_pubkey, local]
    };
    let agg = KeyAggContext::new(keys).map_err(|e| e.to_string())?;
    let xonly = agg
        .aggregated_pubkey::<musig2::secp::Point>()
        .serialize_xonly();
    let witness = crate::watchtower::settlement_data_witness(
        settlement,
        for_remote,
        channel.commitment_contract_features,
        channel.signer.tlc_base_key.pubkey(),
        remote.tlc_base_key,
    )?;
    let args = [
        &ckb_hash::blake2b_256(xonly)[..20],
        &(channel.commitment_delay_epoch | 0xa000000000000000).to_le_bytes(),
        &number.to_be_bytes(),
        &ckb_hash::blake2b_256(witness)[..20],
        &[0, channel.commitment_contract_features.bits()],
    ]
    .concat();
    check(
        tx.raw().outputs().len() == 1
            && tx
                .raw()
                .outputs()
                .get(0)
                .is_some_and(|o| o.lock().args().raw_data().as_ref() == args),
        "V2 commitment transaction/settlement snapshot mismatch",
    )?;
    let funding = channel
        .funding_tx
        .as_ref()
        .ok_or("Missing V2 original funding transaction")?;
    check(
        tx.raw().inputs().len() == 1
            && tx.raw().inputs().get(0).is_some_and(|i| {
                i.previous_output().tx_hash().as_slice()
                    == ckb_hash::blake2b_256(funding.raw().as_slice())
                    && i.previous_output().index().as_slice() == [0; 4]
            }),
        "V2 commitment funding outpoint mismatch",
    )
}

/// Validate legacy/session correspondence and all retained V2 cryptographic intents.
/// Bound-but-unsigned records are legitimate crash-recovery intents.
pub fn validate_channel_v2(channel: &ChannelActorData) -> Result<(), String> {
    let Some(session) = &channel.session_v2 else {
        return check(
            !channel.commitment_contract_features.is_v2(),
            "Missing V2 session for full-hash channel",
        );
    };
    check(
        channel.commitment_contract_features.is_v2() && session.marker == 0x56320001,
        "Invalid V2 feature/session marker",
    )?;
    let funded = matches!(
        channel.state,
        crate::ChannelState::ChannelReady
            | crate::ChannelState::ShuttingDown(_)
            | crate::ChannelState::Stale
            | crate::ChannelState::AwaitingTxSignatures(_)
            | crate::ChannelState::AwaitingChannelReady(_)
    ) || matches!(channel.state, crate::ChannelState::Closed(flags) if flags.intersects(crate::CloseFlags::COOPERATIVE | crate::CloseFlags::UNCOOPERATIVE_LOCAL | crate::CloseFlags::UNCOOPERATIVE_REMOTE));
    if funded {
        check(
            channel.remote_channel_public_keys.is_some()
                && channel.funding_tx.is_some()
                && channel.latest_commitment_transaction.is_some()
                && session.incoming.is_some()
                && session.outgoing.is_some(),
            "Missing funded V2 transaction/history",
        )?;
    }
    let key = &channel.signer.funding_key;
    let local = key.pubkey();
    check(
        channel.local_channel_public_keys.funding_pubkey == local,
        "V2 local key mismatch",
    )?;
    nonce(
        &session.own,
        key,
        local,
        NoncePurposeV2::Commitment,
        session.own.number,
    )?;
    if session.own.number > 1 {
        check(
            session.own.channel_id == channel.id,
            "V2 active nonce channel mismatch",
        )?;
    }
    if matches!(
        channel.state,
        crate::ChannelState::ChannelReady
            | crate::ChannelState::ShuttingDown(_)
            | crate::ChannelState::Stale
    ) {
        check(
            session.own.number == channel.commitment_numbers.remote
                && session.remote_number == channel.commitment_numbers.local,
            "V2 active nonce counter mismatch",
        )?;
    }
    let Some(remote_keys) = &channel.remote_channel_public_keys else {
        return check(
            session.outgoing.is_none()
                && session.incoming.is_none()
                && session.pending_incoming.is_none()
                && session.closing.is_none()
                && session.own.context.is_none(),
            "V2 signing without remote keys",
        );
    };
    let remote = remote_keys.funding_pubkey;
    let established = matches!(
        channel.state,
        crate::ChannelState::ChannelReady
            | crate::ChannelState::ShuttingDown(_)
            | crate::ChannelState::Stale
    ) || matches!(channel.state, crate::ChannelState::Closed(flags) if flags.intersects(crate::CloseFlags::COOPERATIVE | crate::CloseFlags::UNCOOPERATIVE_LOCAL | crate::CloseFlags::UNCOOPERATIVE_REMOTE));
    if established
        && channel.commitment_numbers.local >= 2
        && channel.commitment_numbers.remote >= 2
    {
        check(
            session.incoming.is_some()
                && session.outgoing.is_some()
                && session.remote_nonce.is_some(),
            "Missing established V2 history",
        )?;
        check(
            session.own.number == channel.commitment_numbers.remote
                && session.remote_number == channel.commitment_numbers.local,
            "V2 established counters mismatch",
        )?;
        let out = session
            .outgoing
            .as_ref()
            .ok_or("Missing V2 outgoing history")?;
        check(
            if channel.tlc_state.waiting_ack {
                out.funding.number == session.remote_number
            } else {
                out.funding.number.checked_add(1) == Some(session.remote_number)
            },
            "V2 outgoing history counter mismatch",
        )?;
        let incoming = session
            .incoming
            .as_ref()
            .ok_or("Missing V2 incoming history")?;
        check(
            incoming.funding.number.checked_add(1) == Some(session.own.number),
            "V2 usable incoming history counter mismatch",
        )?;
    }
    let keys = if local <= remote {
        [local, remote]
    } else {
        [remote, local]
    };
    if let Some(out) = &session.outgoing {
        let req = &out.request;
        let number: u64 = req.commitment_number().unpack();
        check(
            number >= 1
                && (number == 1) == req.revocation_nonce().to_opt().is_none()
                && (number == 1) == out.revocation_context.is_none(),
            "V2 outgoing initial/revocation context mismatch",
        )?;
        check(
            req.channel_id().as_slice() == channel.id.as_ref(),
            "V2 outgoing channel mismatch",
        )?;
        nonce(
            &out.funding,
            key,
            remote,
            NoncePurposeV2::Commitment,
            number,
        )?;
        check(
            out.funding.channel_id == channel.id,
            "V2 outgoing nonce channel mismatch",
        )?;
        context(
            &out.funding,
            key,
            keys,
            tx_message(&out.transaction),
            None,
            true,
        )?;
        commitment_snapshot(channel, &out.transaction, &out.settlement, true, number)?;
        check(
            out.funding.signature.is_some()
                && out.funding.signature.map(|s| s.serialize().to_vec())
                    == Some(req.funding_tx_partial_signature().as_slice().to_vec())
                && out.funding.public_nonce.to_bytes().as_slice() == req.funding_nonce().as_slice(),
            "V2 outgoing cache mismatch",
        )?;
        if let Some(rev) = &out.revocation {
            nonce(rev, key, remote, NoncePurposeV2::Revocation, number)?;
            check(
                rev.channel_id == channel.id
                    && req
                        .revocation_nonce()
                        .to_opt()
                        .is_some_and(|n| n.as_slice() == rev.public_nonce.to_bytes()),
                "V2 outgoing revocation advertisement mismatch",
            )?;
            let snapshot = out
                .revocation_context
                .as_ref()
                .ok_or("Missing V2 outgoing revocation snapshot")?;
            revocation(channel, snapshot, [local, remote], number)?;
            let ack = session
                .pending_ack
                .as_ref()
                .or(session.last_ack.as_ref())
                .filter(|a| {
                    u64::from_le_bytes(a.commitment_number().as_slice().try_into().expect("u64"))
                        == number
                });
            let peer = ack
                .map(|a| PubNonce::from_bytes(a.revocation_nonce().as_slice()))
                .transpose()
                .map_err(|e| e.to_string())?;
            check(
                rev.context.is_some() == ack.is_some(),
                "V2 bound revocation without matching ACK",
            )?;
            context(
                rev,
                key,
                [local, remote],
                snapshot.message,
                peer.clone(),
                ack.is_some(),
            )?;
            if let Some(ack) = ack {
                check(
                    ack.channel_id().as_slice() == channel.id.as_ref(),
                    "V2 ACK channel mismatch",
                )?;
                let peer = peer.ok_or("Missing V2 ACK nonce")?;
                let signature =
                    PartialSignature::from_slice(ack.revocation_partial_signature().as_slice())
                        .map_err(|e| e.to_string())?;
                let agg = KeyAggContext::new([local, remote]).map_err(|e| e.to_string())?;
                musig2::verify_partial(
                    &agg,
                    signature,
                    &AggNonce::sum([rev.public_nonce.clone(), peer.clone()]),
                    remote,
                    &peer,
                    snapshot.message,
                )
                .map_err(|e| e.to_string())?;
            }
        } else {
            check(
                number == 1 && out.revocation_context.is_none(),
                "Missing V2 outgoing revocation record",
            )?;
        }
    }
    for (incoming, pending) in session
        .incoming
        .iter()
        .map(|i| (i, false))
        .chain(session.pending_incoming.iter().map(|i| (i, true)))
    {
        let req = &incoming.request;
        let number: u64 = req.commitment_number().unpack();
        check(
            number >= 1
                && (number == 1) == req.revocation_nonce().to_opt().is_none()
                && (number == 1) == incoming.revocation_context.is_none(),
            "V2 incoming initial/revocation context mismatch",
        )?;
        if pending {
            check(
                number == session.own.number
                    && incoming.funding.public_nonce == session.own.public_nonce,
                "V2 staged incoming active nonce mismatch",
            )?;
        }
        check(
            req.channel_id().as_slice() == channel.id.as_ref(),
            "V2 incoming channel mismatch",
        )?;
        nonce(
            &incoming.funding,
            key,
            local,
            NoncePurposeV2::Commitment,
            number,
        )?;
        check(
            number == 1 || incoming.funding.channel_id == channel.id,
            "V2 incoming nonce channel mismatch",
        )?;
        let peer =
            PubNonce::from_bytes(req.funding_nonce().as_slice()).map_err(|e| e.to_string())?;
        context(
            &incoming.funding,
            key,
            keys,
            tx_message(&incoming.transaction),
            Some(peer.clone()),
            true,
        )?;
        commitment_snapshot(
            channel,
            &incoming.transaction,
            &incoming.settlement,
            false,
            number,
        )?;
        let aggregate = KeyAggContext::new(keys).map_err(|e| e.to_string())?;
        let agg_nonce = AggNonce::sum([incoming.funding.public_nonce.clone(), peer.clone()]);
        let signature = PartialSignature::from_slice(req.funding_tx_partial_signature().as_slice())
            .map_err(|e| e.to_string())?;
        musig2::verify_partial(
            &aggregate,
            signature,
            &agg_nonce,
            remote,
            &peer,
            tx_message(&incoming.transaction),
        )
        .map_err(|e| e.to_string())?;
        if !pending {
            let own = incoming
                .funding
                .signature
                .ok_or("Unsigned usable V2 commitment")?;
            let signature: musig2::CompactSignature = musig2::aggregate_partial_signatures(
                &aggregate,
                &agg_nonce,
                [own, signature],
                tx_message(&incoming.transaction),
            )
            .map_err(|e| e.to_string())?;
            let witness = incoming
                .transaction
                .witnesses()
                .get(0)
                .ok_or("Missing V2 usable commitment witness")?
                .raw_data();
            check(
                incoming.transaction.witnesses().len() == 1
                    && witness.as_ref()
                        == canonical_funding_witness(
                            aggregate
                                .aggregated_pubkey::<musig2::secp::Point>()
                                .serialize_xonly(),
                            signature,
                        ),
                "V2 usable commitment witness mismatch",
            )?;
        }
        if let Some(rev) = &incoming.revocation {
            nonce(rev, key, local, NoncePurposeV2::Revocation, number)?;
            check(
                rev.channel_id == channel.id,
                "V2 incoming revocation channel mismatch",
            )?;
            let snapshot = incoming
                .revocation_context
                .as_ref()
                .ok_or("Missing V2 incoming revocation snapshot")?;
            revocation(channel, snapshot, [remote, local], number)?;
            let peer = req
                .revocation_nonce()
                .to_opt()
                .map(|n| PubNonce::from_bytes(n.as_slice()))
                .transpose()
                .map_err(|e| e.to_string())?;
            context(rev, key, [remote, local], snapshot.message, peer, true)?;
            let response = incoming.response.as_ref().ok_or("Missing V2 cached ACK")?;
            check(
                response.next_commitment_nonce().as_slice() == session.own.public_nonce.to_bytes(),
                "V2 cached ACK next nonce mismatch",
            )?;
            check(
                response.channel_id().as_slice() == channel.id.as_ref()
                    && response.commitment_number().as_slice()
                        == req.commitment_number().as_slice()
                    && rev.public_nonce.to_bytes().as_slice()
                        == response.revocation_nonce().as_slice()
                    && rev.signature.map(|s| s.serialize().to_vec())
                        == Some(response.revocation_partial_signature().as_slice().to_vec()),
                "V2 incoming ACK cache mismatch",
            )?;
        } else if !pending {
            check(
                number == 1 && incoming.response.is_none(),
                "Missing V2 incoming revocation record",
            )?;
        }
        if let Some(snapshot) = &incoming.revocation_context {
            revocation(channel, snapshot, [remote, local], number)?;
        }
    }
    if let Some(close) = &session.closing {
        check(
            close.response.is_some() == close.own.signature.is_some(),
            "V2 closing signature/response correspondence mismatch",
        )?;
        let info = channel
            .local_shutdown_info
            .as_ref()
            .ok_or("Missing V2 local shutdown info")?;
        let fee: u64 = close.shutdown.fee_rate().unpack();
        check(
            info.close_script.as_slice() == close.shutdown.close_script().as_slice()
                && info.fee_rate == fee
                && info.signature == close.own.signature,
            "V2 local shutdown info/cache mismatch",
        )?;
        if let Some(remote_shutdown) = &close.remote_shutdown {
            let info = channel
                .remote_shutdown_info
                .as_ref()
                .ok_or("Missing V2 remote shutdown info")?;
            let fee: u64 = remote_shutdown.fee_rate().unpack();
            check(
                remote_shutdown.channel_id().as_slice() == channel.id.as_ref()
                    && info.close_script.as_slice() == remote_shutdown.close_script().as_slice()
                    && info.fee_rate == fee,
                "V2 remote shutdown info/cache mismatch",
            )?;
            let signature = close
                .remote_response
                .as_ref()
                .map(|r| PartialSignature::from_slice(r.partial_signature().as_slice()))
                .transpose()
                .map_err(|e| e.to_string())?;
            check(
                info.signature == signature,
                "V2 remote shutdown signature cache mismatch",
            )?;
        } else {
            check(
                channel.remote_shutdown_info.is_none(),
                "V2 remote shutdown without advertisement",
            )?;
        }
        nonce(&close.own, key, local, NoncePurposeV2::Closing, 0)?;
        check(
            close.own.channel_id == channel.id
                && close.shutdown.channel_id().as_slice() == channel.id.as_ref()
                && close.shutdown.closing_nonce().as_slice() == close.own.public_nonce.to_bytes(),
            "V2 shutdown nonce/cache mismatch",
        )?;
        if let Some(tx) = &close.transaction {
            let remote_shutdown = close
                .remote_shutdown
                .as_ref()
                .ok_or("V2 close transaction without remote nonce")?;
            let peer = PubNonce::from_bytes(remote_shutdown.closing_nonce().as_slice())
                .map_err(|e| e.to_string())?;
            context(
                &close.own,
                key,
                keys,
                tx_message(tx),
                Some(peer.clone()),
                true,
            )?;
            if let Some(response) = &close.remote_response {
                check(
                    response.channel_id().as_slice() == channel.id.as_ref(),
                    "V2 remote closing channel mismatch",
                )?;
                let signature =
                    PartialSignature::from_slice(response.partial_signature().as_slice())
                        .map_err(|e| e.to_string())?;
                let agg = KeyAggContext::new(keys).map_err(|e| e.to_string())?;
                musig2::verify_partial(
                    &agg,
                    signature,
                    &AggNonce::sum([close.own.public_nonce.clone(), peer.clone()]),
                    remote,
                    &peer,
                    tx_message(tx),
                )
                .map_err(|e| e.to_string())?;
            }
        } else {
            check(
                close.own.context.is_none()
                    && close.response.is_none()
                    && close.remote_response.is_none(),
                "V2 close signature without fixed transaction",
            )?;
        }
        if let Some(response) = &close.response {
            check(
                response.channel_id().as_slice() == channel.id.as_ref()
                    && close.own.signature.map(|s| s.serialize().to_vec())
                        == Some(response.partial_signature().as_slice().to_vec()),
                "V2 closing response cache mismatch",
            )?;
        }
    }
    if let Some(incoming) = &session.incoming {
        check(
            channel
                .latest_commitment_transaction
                .as_ref()
                .map(Entity::as_slice)
                == Some(incoming.transaction.as_slice()),
            "V2 latest usable commitment mismatch",
        )?;
    }
    if session.own.context.is_some() {
        check(
            session
                .pending_incoming
                .as_ref()
                .or(session.incoming.as_ref())
                .is_some_and(|i| {
                    i.funding.public_nonce == session.own.public_nonce
                        && i.funding.context == session.own.context
                        && i.funding.signature == session.own.signature
                }),
            "V2 active bound nonce has no matching intent",
        )?;
    }
    if let Some(ack) = &session.pending_ack {
        check(
            session.outgoing.as_ref().is_some_and(|o| {
                o.request.commitment_number().as_slice() == ack.commitment_number().as_slice()
            }),
            "V2 staged ACK without matching outgoing request",
        )?;
    }
    if let Some(ack) = &session.last_ack {
        let number: u64 = ack.commitment_number().unpack();
        check(
            ack.channel_id().as_slice() == channel.id.as_ref()
                && number.checked_add(1) == Some(session.remote_number)
                && session.remote_nonce.as_ref().is_some_and(|n| {
                    n.to_bytes().as_slice() == ack.next_commitment_nonce().as_slice()
                }),
            "V2 accepted ACK active nonce/counter mismatch",
        )?;
    } else {
        check(
            session.remote_number <= 2,
            "Missing V2 accepted ACK history",
        )?;
    }
    if let Some((effect, settlement)) = &session.revocation_effect {
        let snapshot = RevocationContextV2 {
            message: [0; 32],
            output: effect.output.clone(),
            output_data: effect.output_data.clone(),
        };
        let agg = KeyAggContext::new([local, remote]).map_err(|e| e.to_string())?;
        let xonly = agg
            .aggregated_pubkey::<musig2::secp::Point>()
            .serialize_xonly();
        let delay = (channel.commitment_delay_epoch | 0xa000000000000000).to_le_bytes();
        let args = [
            &ckb_hash::blake2b_256(xonly)[..20],
            &delay,
            &effect.commitment_number.to_be_bytes(),
        ]
        .concat();
        let message = ckb_hash::blake2b_256(
            [
                snapshot.output.as_slice(),
                snapshot.output_data.as_slice(),
                &args,
            ]
            .concat(),
        );
        musig2::verify_single(
            agg.aggregated_pubkey::<musig2::secp::Point>(),
            effect.aggregated_signature,
            message,
        )
        .map_err(|e| e.to_string())?;
        if let Some(out) = session
            .outgoing
            .as_ref()
            .filter(|o| Some(o.funding.number) == effect.commitment_number.checked_add(1))
        {
            let context = out
                .revocation_context
                .as_ref()
                .ok_or("V2 revocation effect without original context")?;
            check(
                effect.output.as_slice() == context.output.as_slice()
                    && effect.output_data.as_slice() == context.output_data.as_slice()
                    && settlement == &out.settlement,
                "V2 revocation effect context/settlement mismatch",
            )?;
        }
    }
    for (effect, _) in &session.remove_effects {
        check(
            effect.tlc_id.is_offered() && effect.removed_reason.is_some(),
            "Invalid V2 removal outbox source",
        )?;
        if let Some(source) = channel.tlc_state.get(&effect.tlc_id) {
            check(
                source.payment_hash == effect.payment_hash
                    && source.amount == effect.amount
                    && source.removed_reason == effect.removed_reason
                    && source.attempt_id == effect.attempt_id
                    && source.forwarding_tlc == effect.forwarding_tlc,
                "V2 removal outbox source identity mismatch",
            )?;
        }
    }
    Ok(())
}
