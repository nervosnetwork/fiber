use ckb_types::core::TransactionBuilder;
use molecule::prelude::Entity;
use musig2::{BinaryEncoding, SecNonceBuilder};

use crate::fiber::channel::{
    require_pending_commit_diff_for_replay, resolve_dual_owed_replay_order,
    validate_commit_diff_for_replay_inputs, CommitDiff, CommitmentSignedTemplate,
    ProcessingChannelError, ReplayOrderHint, TlcReplayUpdate,
};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::types::{
    AddTlc, Hash256, ReestablishChannel, RemoveTlc, RemoveTlcFulfill, RemoveTlcReason,
};
use crate::now_timestamp_as_millis_u64;

#[test]
fn test_commit_diff_roundtrip_v2() {
    let channel_id = Hash256::from([1u8; 32]);
    let payment_hash = Hash256::from([2u8; 32]);
    let payment_preimage = Hash256::from([3u8; 32]);
    let commit_tx = TransactionBuilder::default().build().data();
    let next_commitment_nonce = SecNonceBuilder::new([7u8; 32]).build().public_nonce();

    let diff = CommitDiff {
        version: 2,
        channel_id,
        local_commitment_number_at_send: 10,
        remote_commitment_number_at_send: 9,
        commit_tx: commit_tx.clone(),
        replay_updates: vec![
            TlcReplayUpdate::Add(AddTlc {
                channel_id,
                tlc_id: 1,
                amount: 42,
                payment_hash,
                expiry: 1000,
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: None,
            }),
            TlcReplayUpdate::Remove(RemoveTlc {
                channel_id,
                tlc_id: 1,
                reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
            }),
        ],
        commitment_signed_template: Some(CommitmentSignedTemplate {
            next_commitment_nonce: next_commitment_nonce.clone(),
            funding_tx_partial_signature: None,
        }),
        replay_order_hint: Some(ReplayOrderHint::CommitThenRevoke),
        created_at_ms: now_timestamp_as_millis_u64(),
    };

    let encoded = serde_json::to_vec(&diff).expect("serialize commit diff");
    let decoded: CommitDiff = serde_json::from_slice(&encoded).expect("deserialize commit diff");

    assert_eq!(decoded.version, 2);
    assert_eq!(decoded.channel_id, channel_id);
    assert_eq!(decoded.local_commitment_number_at_send, 10);
    assert_eq!(decoded.remote_commitment_number_at_send, 9);
    assert_eq!(decoded.commit_tx.as_slice(), commit_tx.as_slice());
    assert_eq!(decoded.replay_updates.len(), 2);
    assert_eq!(
        decoded
            .commitment_signed_template
            .as_ref()
            .expect("commitment template present")
            .next_commitment_nonce
            .to_bytes(),
        next_commitment_nonce.to_bytes()
    );
    assert_eq!(
        decoded.replay_order_hint,
        Some(ReplayOrderHint::CommitThenRevoke)
    );
}

#[test]
fn test_validate_commit_diff_for_replay() {
    let channel_id = Hash256::from([1u8; 32]);
    let other_channel_id = Hash256::from([9u8; 32]);
    let commit_tx = TransactionBuilder::default().build().data();

    let reestablish = ReestablishChannel {
        channel_id,
        local_commitment_number: 8,
        remote_commitment_number: 10,
    };
    let base_diff = CommitDiff {
        version: 2,
        channel_id,
        local_commitment_number_at_send: 10,
        remote_commitment_number_at_send: 8,
        commit_tx,
        replay_updates: vec![TlcReplayUpdate::Add(AddTlc {
            channel_id,
            tlc_id: 1,
            amount: 42,
            payment_hash: Hash256::from([2u8; 32]),
            expiry: 1000,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
        })],
        commitment_signed_template: None,
        replay_order_hint: None,
        created_at_ms: now_timestamp_as_millis_u64(),
    };

    validate_commit_diff_for_replay_inputs(channel_id, true, 10, 8, &reestablish, &base_diff)
        .expect("valid commit diff should pass");

    let mut channel_mismatch = base_diff.clone();
    channel_mismatch.channel_id = other_channel_id;
    let err = validate_commit_diff_for_replay_inputs(
        channel_id,
        true,
        10,
        8,
        &reestablish,
        &channel_mismatch,
    )
    .expect_err("channel mismatch should fail");
    assert!(matches!(
        err,
        ProcessingChannelError::InvalidState(msg) if msg.contains("channel mismatch")
    ));

    let err =
        validate_commit_diff_for_replay_inputs(channel_id, false, 10, 8, &reestablish, &base_diff)
            .expect_err("not waiting ack should fail");
    assert!(matches!(
        err,
        ProcessingChannelError::InvalidState(msg) if msg.contains("not waiting ack")
    ));

    let mut stale_local = base_diff.clone();
    stale_local.local_commitment_number_at_send = 9;
    let err =
        validate_commit_diff_for_replay_inputs(channel_id, true, 10, 8, &reestablish, &stale_local)
            .expect_err("stale local commitment number should fail");
    assert!(matches!(
        err,
        ProcessingChannelError::InvalidState(msg)
            if msg.contains("stale commit diff local commitment number")
    ));

    let err =
        validate_commit_diff_for_replay_inputs(channel_id, true, 10, 11, &reestablish, &base_diff)
            .expect_err("stale remote commitment drift should fail");
    assert!(matches!(
        err,
        ProcessingChannelError::InvalidState(msg) if msg.contains("remote commitment drift")
    ));
}

#[test]
fn test_require_pending_commit_diff_for_replay() {
    let channel_id = Hash256::from([5u8; 32]);
    let err = require_pending_commit_diff_for_replay(channel_id, None)
        .expect_err("missing pending commit diff should fail");
    assert!(matches!(
        err,
        ProcessingChannelError::InvalidState(msg) if msg.contains("Missing pending CommitDiff")
    ));
}

#[test]
fn test_resolve_dual_owed_replay_order() {
    let channel_id = Hash256::from([6u8; 32]);
    assert_eq!(
        resolve_dual_owed_replay_order(
            channel_id,
            Some(ReplayOrderHint::CommitThenRevoke),
            11,
            10,
            false
        ),
        ReplayOrderHint::CommitThenRevoke
    );
    assert_eq!(
        resolve_dual_owed_replay_order(
            channel_id,
            Some(ReplayOrderHint::RevokeThenCommit),
            10,
            10,
            false
        ),
        ReplayOrderHint::RevokeThenCommit
    );
    assert_eq!(
        resolve_dual_owed_replay_order(
            channel_id,
            Some(ReplayOrderHint::RevokeThenCommit),
            11,
            10,
            false
        ),
        ReplayOrderHint::CommitThenRevoke
    );
    assert_eq!(
        resolve_dual_owed_replay_order(channel_id, None, 10, 10, false),
        ReplayOrderHint::RevokeThenCommit
    );
    assert_eq!(
        resolve_dual_owed_replay_order(channel_id, None, 11, 10, false),
        ReplayOrderHint::CommitThenRevoke
    );
    assert_eq!(
        resolve_dual_owed_replay_order(
            channel_id,
            Some(ReplayOrderHint::RevokeThenCommit),
            10,
            10,
            true
        ),
        ReplayOrderHint::CommitThenRevoke
    );
}
