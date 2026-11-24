use fiber_types::ChannelActorData;

use super::super::{
    convert_named, decode_as_new, ChannelSessionV2Wire, CurrentChannelSessionV2Wire,
    IncomingCommitmentV2Wire, OldChannelActorData, OutgoingCommitmentV2Wire,
    ReleasedChannelSessionV2Wire,
};

/// The released v0.10.0-rc2 ..= v0.10.1 layout: the session is prefixed with its marker.
pub(super) const RELEASED_SESSION_MARKER: u32 = 0x56320001;

pub(super) fn to_released_settlement(
    data: fiber_types::SettlementData,
) -> fiber_types_0100::SettlementData {
    fiber_types_0100::SettlementData {
        local_amount: data.local_amount,
        remote_amount: data.remote_amount,
        tlcs: data
            .tlcs
            .into_iter()
            .map(|tlc| fiber_types_0100::SettlementTlc {
                tlc_id: decode_as_new(tlc.tlc_id).unwrap(),
                hash_algorithm: decode_as_new(tlc.hash_algorithm).unwrap(),
                payment_amount: tlc.payment_amount,
                payment_hash: decode_as_new(tlc.payment_hash).unwrap(),
                expiry: tlc.expiry,
                local_key: decode_as_new(
                    tlc.local_key
                        .expect("released v0.10.1 settlement TLC always stores local_key"),
                )
                .unwrap(),
                remote_key: decode_as_new(tlc.remote_key).unwrap(),
            })
            .collect(),
    }
}

fn to_released_session(session: fiber_types::ChannelSessionV2) -> ReleasedChannelSessionV2Wire {
    let wire: CurrentChannelSessionV2Wire = decode_as_new(session).unwrap();
    ChannelSessionV2Wire {
        own: wire.own,
        own_retired: wire.own_retired,
        remote_nonce: wire.remote_nonce,
        remote_number: wire.remote_number,
        bootstrap_remote_nonce: wire.bootstrap_remote_nonce,
        remote_ready: wire.remote_ready,
        outgoing: wire.outgoing.map(|out| OutgoingCommitmentV2Wire {
            peer_wait_started_at: out.peer_wait_started_at,
            request: out.request,
            transaction: out.transaction,
            settlement: to_released_settlement(out.settlement),
            updates: out.updates,
            funding: out.funding,
            revocation: out.revocation,
            revocation_context: out.revocation_context,
        }),
        incoming: wire.incoming.map(|inc| IncomingCommitmentV2Wire {
            request: inc.request,
            funding: inc.funding,
            revocation: inc.revocation,
            revocation_context: inc.revocation_context,
            transaction: inc.transaction,
            settlement: to_released_settlement(inc.settlement),
            response: inc.response,
        }),
        pending_incoming: wire.pending_incoming.map(|inc| IncomingCommitmentV2Wire {
            request: inc.request,
            funding: inc.funding,
            revocation: inc.revocation,
            revocation_context: inc.revocation_context,
            transaction: inc.transaction,
            settlement: to_released_settlement(inc.settlement),
            response: inc.response,
        }),
        pending_ack: wire.pending_ack,
        last_ack: wire.last_ack,
        revocation_effect: wire
            .revocation_effect
            .map(|(revocation, settlement)| (revocation, to_released_settlement(settlement))),
        bootstrap_settlement: wire.bootstrap_settlement.map(to_released_settlement),
        remove_effects: wire
            .remove_effects
            .into_iter()
            .map(|(tlc, execution)| (convert_named(tlc).unwrap(), execution))
            .collect(),
        closing: wire.closing,
    }
}

pub(super) fn released_v2(
    channel: ChannelActorData,
) -> (
    OldChannelActorData,
    Option<(u32, ReleasedChannelSessionV2Wire)>,
) {
    let session = to_released_session(channel.session.clone());
    let latest_commitment_transaction = channel.latest_commitment_transaction();
    let old = OldChannelActorData {
        state: decode_as_new(channel.state).unwrap(),
        public_channel_info: decode_as_new(channel.public_channel_info).unwrap(),
        local_tlc_info: decode_as_new(channel.local_tlc_info).unwrap(),
        remote_tlc_info: decode_as_new(channel.remote_tlc_info).unwrap(),
        local_pubkey: decode_as_new(channel.local_pubkey).unwrap(),
        remote_pubkey: decode_as_new(channel.remote_pubkey).unwrap(),
        id: decode_as_new(channel.id).unwrap(),
        funding_tx: channel.funding_tx,
        funding_tx_confirmed_at: channel.funding_tx_confirmed_at,
        funding_udt_type_script: channel.funding_udt_type_script,
        latest_commitment_transaction,
        is_acceptor: channel.is_acceptor,
        is_one_way: channel.is_one_way,
        to_local_amount: channel.to_local_amount,
        to_remote_amount: channel.to_remote_amount,
        local_reserved_ckb_amount: channel.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: channel.remote_reserved_ckb_amount,
        commitment_fee_rate: channel.commitment_fee_rate,
        commitment_delay_epoch: channel.commitment_delay_epoch,
        funding_fee_rate: channel.funding_fee_rate,
        signer: decode_as_new(channel.signer.expect("released channel has a local signer"))
            .unwrap(),
        local_channel_public_keys: decode_as_new(channel.local_channel_public_keys).unwrap(),
        commitment_numbers: decode_as_new(channel.commitment_numbers).unwrap(),
        local_constraints: decode_as_new(channel.local_constraints).unwrap(),
        remote_constraints: decode_as_new(channel.remote_constraints).unwrap(),
        tlc_state: serde_json::from_value(serde_json::to_value(channel.tlc_state).unwrap())
            .unwrap(),
        retryable_tlc_operations: serde_json::from_value(
            serde_json::to_value(channel.retryable_tlc_operations).unwrap(),
        )
        .unwrap(),
        waiting_forward_tlc_tasks: decode_as_new(channel.waiting_forward_tlc_tasks).unwrap(),
        remote_shutdown_script: channel.remote_shutdown_script,
        local_shutdown_script: channel.local_shutdown_script,
        remote_commitment_points: decode_as_new(channel.remote_commitment_points).unwrap(),
        remote_channel_public_keys: decode_as_new(channel.remote_channel_public_keys).unwrap(),
        local_shutdown_info: decode_as_new(channel.local_shutdown_info).unwrap(),
        remote_shutdown_info: decode_as_new(channel.remote_shutdown_info).unwrap(),
        shutdown_transaction_hash: channel.shutdown_transaction_hash,
        reestablishing: channel.reestablishing,
        created_at: channel.created_at,
        pending_replay_updates: decode_as_new(channel.pending_replay_updates).unwrap(),
        last_was_revoke: channel.last_was_revoke,
        connectivity_state: decode_as_new(channel.connectivity_state).unwrap(),
        external_funding: decode_as_new(channel.external_funding).unwrap(),
        last_committed_remote_nonce: None,
        remote_revocation_nonce_for_verify: None,
        remote_revocation_nonce_for_send: None,
        remote_revocation_nonce_for_next: None,
        last_revoke_ack_msg: None,
        commitment_contract_features:
            fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    };
    (old, Some((RELEASED_SESSION_MARKER, session)))
}
