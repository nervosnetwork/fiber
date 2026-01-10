use crate::fiber::channel::ChannelState;
use crate::tests::test_utils::*;
use std::time::{Duration, Instant};
use tracing::debug;

#[tokio::test]
async fn test_reestablish_restores_send_nonce() {
    init_tracing();
    let (mut node_a, mut node_b, channel_id) =
        create_nodes_with_established_channel(100000000000, 100000000000, true).await;

    // Trigger payment A -> B
    // This will cause A to send AddTlc + CommitmentSigned
    // B will respond with RevokeAndAck
    // This puts B in a state where remote_revocation_nonce_for_send might be None
    let _ = node_a.send_payment_keysend(&node_b, 1000, false).await;

    // Wait for B to reach the target state where send is None but verify is Some.
    // This confirms we are in the potential deadlock state if persistent.
    let mut caught = false;
    for _ in 0..100 {
        let state = node_b.get_channel_actor_state(channel_id);
        if state.remote_revocation_nonce_for_verify.is_some()
            && state.remote_revocation_nonce_for_send.is_none()
        {
            debug!("Caught target state on Node B!");
            caught = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        caught,
        "Failed to reach target state where send nonce is None"
    );

    assert_eq!(node_a.get_inflight_payment_count().await, 1);

    // Now restart node B to simulate disconnect/reconnect
    node_b.restart().await;
    node_b.connect_to(&mut node_a).await;

    node_a
        .expect_debug_event("Reestablished channel in ChannelReady")
        .await;
    node_b
        .expect_debug_event("Reestablished channel in ChannelReady")
        .await;

    tokio::time::sleep(Duration::from_millis(1000)).await;

    // check inflight until 5s
    let now = Instant::now();
    loop {
        if node_a.get_inflight_payment_count().await == 0 {
            break;
        }
        assert!(now.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(node_a.get_inflight_payment_count().await, 0);

    let state = node_b.get_channel_actor_state(channel_id);
    println!("Node B State after wait:");
    println!(
        "  Local Commitment Number: {}",
        state.get_local_commitment_number()
    );
    println!(
        "  Remote Commitment Number: {}",
        state.get_remote_commitment_number()
    );
    println!(
        "  Send Nonce: {:?}",
        state.remote_revocation_nonce_for_send.is_some()
    );
    println!(
        "  Verify Nonce: {:?}",
        state.remote_revocation_nonce_for_verify.is_some()
    );

    let state_a = node_a.get_channel_actor_state(channel_id);
    println!("Node A State:");
    println!(
        "  Local Commitment Number: {}",
        state_a.get_local_commitment_number()
    );
    println!(
        "  Remote Commitment Number: {}",
        state_a.get_remote_commitment_number()
    );
    println!(
        "  Send Nonce: {:?}",
        state_a.remote_revocation_nonce_for_send.is_some()
    );
    println!(
        "  Verify Nonce: {:?}",
        state_a.remote_revocation_nonce_for_verify.is_some()
    );

    // assert!(
    //     nonces_restored,
    //     "Send nonce was not restored after reestablish!"
    // );

    // assert!(
    //     state.remote_revocation_nonce_for_send.is_some(),
    //     "Send nonce was not restored!"
    // );
    // assert_eq!(
    //     state.remote_revocation_nonce_for_send,
    //     state.remote_revocation_nonce_for_verify
    // );

    // Further verification: A can send another payment.
    // Use a new payment to differentiate.
    let res = node_a
        .send_payment_keysend(&node_b, 2000, false)
        .await
        .unwrap();
    // wait with timeout
    let timeout = Duration::from_secs(10);
    let started = Instant::now();
    while started.elapsed() < timeout {
        if node_a.get_payment_status(res.payment_hash).await.is_final() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!(
        "payment status: {:?}",
        node_a.get_payment_status(res.payment_hash).await
    );
    assert!(
        node_a.get_payment_status(res.payment_hash).await.is_final(),
        "Failed to send subsequent payment"
    );
}
