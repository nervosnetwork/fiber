use fiber_types::{ChannelActorData, ChannelSessionV2};
use fiber_types_0100::ChannelActorData as OldChannelActorData;

use super::decode_as_new;

pub(super) fn released_v2(
    channel: ChannelActorData,
) -> (OldChannelActorData, Option<ChannelSessionV2>) {
    let session = channel.session;
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
        is_acceptor: channel.is_acceptor,
        is_one_way: channel.is_one_way,
        to_local_amount: channel.to_local_amount,
        to_remote_amount: channel.to_remote_amount,
        local_reserved_ckb_amount: channel.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: channel.remote_reserved_ckb_amount,
        commitment_fee_rate: channel.commitment_fee_rate,
        commitment_delay_epoch: channel.commitment_delay_epoch,
        funding_fee_rate: channel.funding_fee_rate,
        signer: decode_as_new(channel.signer).unwrap(),
        local_channel_public_keys: decode_as_new(channel.local_channel_public_keys).unwrap(),
        commitment_numbers: decode_as_new(channel.commitment_numbers).unwrap(),
        local_constraints: decode_as_new(channel.local_constraints).unwrap(),
        remote_constraints: decode_as_new(channel.remote_constraints).unwrap(),
        tlc_state: decode_as_new(channel.tlc_state).unwrap(),
        retryable_tlc_operations: decode_as_new(channel.retryable_tlc_operations).unwrap(),
        waiting_forward_tlc_tasks: decode_as_new(channel.waiting_forward_tlc_tasks).unwrap(),
        remote_shutdown_script: channel.remote_shutdown_script,
        local_shutdown_script: channel.local_shutdown_script,
        latest_commitment_transaction: channel.latest_commitment_transaction,
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
    (old, Some(session))
}
