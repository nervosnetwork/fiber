use fiber_types::{
    ChannelFeatures, Hash256, HashAlgorithm, OutboundTlcStatus, RemoveTlcFulfill, RemoveTlcReason,
    TLCId, TlcErr, TlcErrPacket, TlcErrorCode, TlcStatus,
};

use crate::fiber::channel::ChannelActorState;
use crate::fiber::tests::settle_tlc_set_command_tests::create_test_channel_state_with_tlc;
use crate::gen_rand_sha256_hash;

fn state_with_remove_wait_prev_ack(
    reason: RemoveTlcReason,
    payment_hash: Hash256,
) -> ChannelActorState {
    let mut state =
        create_test_channel_state_with_tlc(gen_rand_sha256_hash(), 0, 300, payment_hash, None);
    state.channel_features = ChannelFeatures::V2;
    state.to_local_amount = 800;
    state.to_remote_amount = 200;

    let mut tlc = state.tlc_state.received_tlcs.tlcs.pop().unwrap();
    tlc.tlc_id = TLCId::Offered(0);
    tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoteRemoved);
    tlc.removed_reason = Some(reason);
    state.tlc_state.offered_tlcs.tlcs.push(tlc);

    // A peer's CommitmentSigned arrives while our preceding one still awaits RevokeAndAck.
    state.tlc_state.waiting_ack = true;
    state.tlc_state.update_for_commitment_signed();
    assert_eq!(
        state.tlc_state.offered_tlcs.tlcs[0].outbound_status(),
        OutboundTlcStatus::RemoveWaitPrevAck
    );
    state
}

#[test]
fn failed_offered_tlc_in_remove_wait_prev_ack_is_refunded_on_local_commitment() {
    let state = state_with_remove_wait_prev_ack(
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::TemporaryChannelFailure),
            &[7; 32],
        )),
        gen_rand_sha256_hash(),
    );

    let settlement = state.build_settlement_data(false).unwrap();
    assert!(settlement.tlcs.is_empty());
    assert_eq!(settlement.local_amount, 800);
    assert_eq!(settlement.remote_amount, 200);
    assert_eq!(settlement.local_amount + settlement.remote_amount, 1_000);
}

#[test]
fn fulfilled_offered_tlc_in_remove_wait_prev_ack_moves_to_remote_balance() {
    let preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
    let state = state_with_remove_wait_prev_ack(
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        }),
        payment_hash,
    );

    let settlement = state.build_settlement_data(false).unwrap();
    assert!(settlement.tlcs.is_empty());
    assert_eq!(settlement.local_amount, 500);
    assert_eq!(settlement.remote_amount, 500);
    assert_eq!(settlement.local_amount + settlement.remote_amount, 1_000);
}
