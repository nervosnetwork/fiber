#![cfg(not(target_arch = "wasm32"))]
use crate::fiber::channel::{ChannelActorStateStore, PrevTlcInfo};
use crate::fiber::config::{DEFAULT_FINAL_TLC_EXPIRY_DELTA, DEFAULT_TLC_EXPIRY_DELTA};
use crate::fiber::features::FeatureVector;
use crate::fiber::graph::*;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::{NetworkActorCommand, NetworkActorMessage, SendOnionPacketCommand};
use crate::fiber::payment::{PaymentStatus, SendPaymentCommand, TrampolineHop};
use crate::fiber::types::{
    CurrentPaymentHopData, Hash256, PeeledPaymentOnionPacket, Privkey, Pubkey, TlcErrorCode,
    TrampolineHopPayload, TrampolineOnionPacket,
};
use crate::gen_rand_fiber_public_key;
use crate::invoice::{Currency, InvoiceBuilder, InvoiceStore, PreimageStore};
use crate::tests::test_utils::*;
use crate::{
    create_channel_with_nodes, gen_rand_sha256_hash, ChannelParameters, HUGE_CKB_AMOUNT,
    MIN_RESERVED_CKB,
};
use ractor::RpcReplyPort;
use rand::Rng;
use secp256k1::Secp256k1;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error};

#[tokio::test]
async fn test_trampoline_routing_basic() {
    init_tracing();

    // A --(public)--> B --(private)--> C
    // A cannot find a route to C from gossip graph; B can forward to C using its direct channel.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    // ================================================================
    // Create an invoice on C.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(500),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_a.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_one_way_channel_rejects_reverse_payment() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[(
            (0, 1),
            ChannelParameters {
                public: false,
                one_way: true,
                node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                node_b_funding_amount: MIN_RESERVED_CKB + 100000,
                ..Default::default()
            },
        )],
        2,
        None,
    )
    .await;
    let [node_a, node_b] = nodes.try_into().expect("2 nodes");

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_a.get_public_key().into())
        .build()
        .expect("build invoice");
    node_a.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_b
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());
    let error = res.err().unwrap();
    assert!(error.contains("Failed to build route"));

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_b.get_public_key().into())
        .build()
        .expect("build invoice");
    node_b.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_a.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_keysend_success() {
    init_tracing();

    // A --(public)--> B --(private)--> C
    // A cannot find a route to C from gossip graph; B can forward to C using its direct channel.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    let amount: u128 = 1000;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c.pubkey),
            amount: Some(amount),
            keysend: Some(true),
            max_fee_amount: Some(5_000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_keysend_when_trampoline_use_default_fee() {
    init_tracing();

    // A --(public)--> B --(public)--> C --(private, expensive)--> D
    // A -> D with trampoline [C]. C forwards to D.
    // the payment should succeed with sufficient max_fee_amount.
    let (nodes, channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
            (
                (2, 3),
                ChannelParameters {
                    public: false,
                    node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
        ],
        4,
        None,
    )
    .await;

    let [node_a, node_b, node_c, node_d] = nodes.try_into().expect("4 nodes");

    // Record initial balances for first payment test
    let node_a_initial_1 = node_a.get_local_balance_from_channel(channels[0]);
    let node_b_initial_1_a_side = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_initial_1_c_side = node_b.get_local_balance_from_channel(channels[1]);
    let node_c_initial_1_b_side = node_c.get_local_balance_from_channel(channels[1]);
    let node_c_initial_1_d_side = node_c.get_local_balance_from_channel(channels[2]);
    let node_d_initial_1 = node_d.get_local_balance_from_channel(channels[2]);

    // will success with default fee calculation
    let amount: u128 = 1000;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_d.pubkey),
            amount: Some(amount),
            keysend: Some(true),
            max_fee_amount: None, // no max fee amount specified
            trampoline_hops: Some(vec![TrampolineHop::new(node_c.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let response_1 = res.unwrap();
    let payment_hash = response_1.payment_hash;
    debug!("Payment 1 response fee: {}", response_1.fee);
    node_a.wait_until_success(payment_hash).await;

    // Verify balances after first payment (default fee calculation)
    let node_a_final_1 = node_a.get_local_balance_from_channel(channels[0]);
    let node_a_paid_1 = node_a_initial_1 - node_a_final_1;
    let actual_fee_1 = node_a_paid_1 - amount;
    debug!(
        "Payment 1: Node A paid: {} (amount: {}, fee: {})",
        node_a_paid_1, amount, actual_fee_1
    );
    assert_eq!(
        node_a_paid_1,
        amount + actual_fee_1,
        "Node A should pay amount + fee"
    );
    assert_eq!(
        response_1.fee, actual_fee_1,
        "Response fee should match actual fee paid"
    );

    let node_d_final_1 = node_d.get_local_balance_from_channel(channels[2]);
    let node_d_received_1 = node_d_final_1 - node_d_initial_1;
    debug!("Payment 1: Node D received: {}", node_d_received_1);
    assert_eq!(
        node_d_received_1, amount,
        "Node D should receive exactly the amount"
    );

    // Node B's balance changes (A->B channel)
    let node_b_final_1_a_side = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_received_from_a_1 = node_b_final_1_a_side - node_b_initial_1_a_side;

    // Node B's balance changes (B->C channel)
    let node_b_final_1_c_side = node_b.get_local_balance_from_channel(channels[1]);
    let node_b_paid_to_c_1 = node_b_initial_1_c_side - node_b_final_1_c_side;

    // Node B's net gain (forwarding fee earned)
    let node_b_net_gain_1 = node_b_received_from_a_1 - node_b_paid_to_c_1;
    debug!(
        "Payment 1: Node B net gain: {} (received: {}, paid: {})",
        node_b_net_gain_1, node_b_received_from_a_1, node_b_paid_to_c_1
    );
    assert!(node_b_net_gain_1 > 0, "Node B should earn forwarding fee");

    // Node C's balance changes (B->C channel)
    let node_c_final_1_b_side = node_c.get_local_balance_from_channel(channels[1]);
    let node_c_received_from_b_1 = node_c_final_1_b_side - node_c_initial_1_b_side;

    // Node C's balance changes (C->D channel)
    let node_c_final_1_d_side = node_c.get_local_balance_from_channel(channels[2]);
    let node_c_paid_to_d_1 = node_c_initial_1_d_side - node_c_final_1_d_side;

    // Node C's net gain (trampoline service fee + forwarding fee)
    let node_c_net_gain_1 = node_c_received_from_b_1 - node_c_paid_to_d_1;
    debug!(
        "Payment 1: Node C net gain: {} (received: {}, paid: {})",
        node_c_net_gain_1, node_c_received_from_b_1, node_c_paid_to_d_1
    );
    assert!(
        node_c_net_gain_1 > 0,
        "Node C should earn trampoline service fee"
    );

    // Record initial balances for second payment test
    let node_a_initial_2 = node_a_final_1;
    let node_b_initial_2_a_side = node_b_final_1_a_side;
    let node_b_initial_2_c_side = node_b_final_1_c_side;
    let node_c_initial_2_b_side = node_c_final_1_b_side;
    let node_c_initial_2_d_side = node_c_final_1_d_side;
    let node_d_initial_2 = node_d_final_1;

    // will success with sufficient max_fee_amount
    let amount: u128 = 1000;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_d.pubkey),
            amount: Some(amount),
            keysend: Some(true),
            max_fee_amount: Some(2000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_c.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let response_2 = res.unwrap();
    let payment_hash = response_2.payment_hash;
    debug!("Payment 2 response fee: {}", response_2.fee);
    node_a.wait_until_success(payment_hash).await;

    // Verify balances after second payment (with max_fee_amount = 2000)
    let node_a_final_2 = node_a.get_local_balance_from_channel(channels[0]);
    let node_a_paid_2 = node_a_initial_2 - node_a_final_2;
    let actual_fee_2 = node_a_paid_2 - amount;
    debug!(
        "Payment 2: Node A paid: {} (amount: {}, fee: {})",
        node_a_paid_2, amount, actual_fee_2
    );
    assert_eq!(
        node_a_paid_2,
        amount + actual_fee_2,
        "Node A should pay amount + fee"
    );
    assert_eq!(
        response_2.fee, actual_fee_2,
        "Response fee should match actual fee paid"
    );

    let node_d_final_2 = node_d.get_local_balance_from_channel(channels[2]);
    let node_d_received_2 = node_d_final_2 - node_d_initial_2;
    debug!("Payment 2: Node D received: {}", node_d_received_2);
    assert_eq!(
        node_d_received_2, amount,
        "Node D should receive exactly the amount"
    );

    // Node B's balance changes (A->B channel)
    let node_b_final_2_a_side = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_received_from_a_2 = node_b_final_2_a_side - node_b_initial_2_a_side;

    // Node B's balance changes (B->C channel)
    let node_b_final_2_c_side = node_b.get_local_balance_from_channel(channels[1]);
    let node_b_paid_to_c_2 = node_b_initial_2_c_side - node_b_final_2_c_side;

    // Node B's net gain (forwarding fee earned)
    let node_b_net_gain_2 = node_b_received_from_a_2 - node_b_paid_to_c_2;
    debug!(
        "Payment 2: Node B net gain: {} (received: {}, paid: {})",
        node_b_net_gain_2, node_b_received_from_a_2, node_b_paid_to_c_2
    );
    assert!(node_b_net_gain_2 > 0, "Node B should earn forwarding fee");

    // Node C's balance changes (B->C channel)
    let node_c_final_2_b_side = node_c.get_local_balance_from_channel(channels[1]);
    let node_c_received_from_b_2 = node_c_final_2_b_side - node_c_initial_2_b_side;

    // Node C's balance changes (C->D channel)
    let node_c_final_2_d_side = node_c.get_local_balance_from_channel(channels[2]);
    let node_c_paid_to_d_2 = node_c_initial_2_d_side - node_c_final_2_d_side;

    // Node C's net gain (trampoline service fee + forwarding fee)
    let node_c_net_gain_2 = node_c_received_from_b_2 - node_c_paid_to_d_2;
    debug!(
        "Payment 2: Node C net gain: {} (received: {}, paid: {})",
        node_c_net_gain_2, node_c_received_from_b_2, node_c_paid_to_d_2
    );
    assert!(
        node_c_net_gain_2 > 0,
        "Node C should earn trampoline service fee"
    );
}

#[tokio::test]
async fn test_trampoline_routing_multi_trampoline_hops_keysend_success() {
    init_tracing();

    // A --(public)--> T1 --(public)--> T2 --(private)--> C
    // A cannot find a route to C from gossip graph; chained trampolines can forward.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        4,
    )
    .await;

    let [node_a, node_t1, node_t2, node_c] = nodes.try_into().expect("4 nodes");

    // Ensure A knows the first trampoline supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;

    // Ensure A learns enough public channels to reach the first trampoline.
    wait_until_node_has_public_channels_at_least(&node_a, 2).await;

    let amount: u128 = 1000;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c.pubkey),
            amount: Some(amount),
            keysend: Some(true),
            max_fee_amount: Some(10_000),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_private_last_hop_payment_success() {
    init_tracing();

    // A --(public)--> B --(private)--> C
    // A cannot find a route to C from gossip graph; B can forward to C using its direct channel.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    // Create an invoice on C.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    // Without explicit trampoline hops, routing should fail.
    let res = node_a
        .assert_send_payment_failure(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;
    eprintln!("payment failure reason: {}", res);
    assert!(res.contains("Failed to build route"));

    // ================================================================
    // With explicit trampoline hops, routing should succeed.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    node_a
        .assert_send_payment_success(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;

    // ================================================================
    // Disable trampoline capability on B, then routing should fail.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));
    // disable trampoline capability on B.
    let mut features = FeatureVector::default();
    features.unset_trampoline_routing_required();
    node_b.update_node_features(features).await;

    // Wait until A learns B does not supports trampoline routing.
    for _ in 0..50 {
        let ok = node_a
            .get_network_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id == node_b.get_public_key())
            .is_some_and(|n| !n.features.supports_trampoline_routing());
        if ok {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    let payment_hash = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(payment_hash.is_err());
    let error = payment_hash.err().unwrap();
    assert!(error.contains("Failed to build route"));
}

#[tokio::test]
async fn test_trampoline_routing_with_two_networks() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true)],
        2,
    )
    .await;

    let [node_a, mut node_b] = nodes.try_into().expect("3 nodes");

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
        ],
        3,
    )
    .await;

    let [mut node_d, _node_e, node_f] = nodes.try_into().expect("3 nodes");

    // no direct connection between node_b and node_d
    // ---------------------------------------------------------------
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_f.get_public_key().into())
        .build()
        .expect("build invoice");
    node_f.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_failed(res.unwrap().payment_hash).await;
    // ---------------------------------------------------------------

    // now create a private channel for node_c and node_d
    node_b.connect_to(&mut node_d).await;

    // this channel's funding tx needs to be known by node_b's chain actor for ChannelAnnouncement verification
    // but here we haven't synced the funding tx to node_b, so we skip the gossip part in this test.
    let _res = create_channel_with_nodes(
        &mut node_b,
        &mut node_d,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_f.get_public_key().into())
        .build()
        .expect("build invoice");
    node_f.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_b
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_d.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_b.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_multi_trampoline_hops() {
    init_tracing();

    // A --(public)--> T1 --(public)--> T2 --(private)--> C
    // A cannot find a route to C from gossip graph; chained trampolines can forward.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        4,
    )
    .await;

    let [node_a, node_t1, node_t2, node_c] = nodes.try_into().expect("4 nodes");

    // Wait until A learns T1 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;

    // Wait until A learns enough public channels to reach the first trampoline.
    wait_until_node_has_public_channels_at_least(&node_a, 2).await;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_four_private_trampoline_hops_payment_success() {
    init_tracing();

    // A --(public)--> T1 --(private)--> T2 --(private)--> T3 --(private)--> T4 --(private)--> C
    // A only needs a route to the first trampoline. The remaining hops are forwarded via private
    // channels using the trampoline onion.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
            ((3, 4), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
            ((4, 5), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        6,
    )
    .await;

    let [node_a, node_t1, node_t2, node_t3, node_t4, node_c] = nodes.try_into().expect("6 nodes");

    // Wait until A learns T1 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;

    // Wait until A learns enough public channels to reach the first trampoline.
    wait_until_node_has_public_channels_at_least(&node_a, 1).await;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(100_000),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
                TrampolineHop::new(node_t3.get_public_key()),
                TrampolineHop::new(node_t4.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_max_trampoline_hops_success_and_each_hop_pathfinds() {
    init_tracing();

    // MAX_TRAMPOLINE_HOPS_LIMIT is 5.
    // Topology forces trampoline routing (final hop is private), and forces each trampoline
    // forwarding step to run path finding over a public multi-hop sub-path.
    //
    // A --(public)--> T1 --(public)--> X12 --(public)--> T2 --(public)--> X23 --(public)--> T3
    //  --(public)--> X34 --(public)--> T4 --(public)--> X45 --(public)--> T5 --(private)--> C
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((3, 4), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((4, 5), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((5, 6), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((6, 7), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((7, 8), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((8, 9), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((9, 10), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        11,
    )
    .await;

    let [node_a, node_t1, node_x12, node_t2, node_x23, node_t3, node_x34, node_t4, node_x45, node_t5, node_c] =
        nodes.try_into().expect("11 nodes");

    // Ensure A knows all trampoline hops support trampoline routing.
    for hop in [&node_t1, &node_t2, &node_t3, &node_t4, &node_t5] {
        wait_until_node_supports_trampoline_routing(&node_a, hop).await;
    }

    // Ensure each forwarding trampoline has the public graph needed to route to the next hop.
    wait_until_graph_channel_updates_along_path(&node_a, &[&node_a, &node_t1]).await;
    wait_until_graph_channel_updates_along_path(&node_t1, &[&node_t1, &node_x12, &node_t2]).await;
    wait_until_graph_channel_updates_along_path(&node_t2, &[&node_t2, &node_x23, &node_t3]).await;
    wait_until_graph_channel_updates_along_path(&node_t3, &[&node_t3, &node_x34, &node_t4]).await;
    wait_until_graph_channel_updates_along_path(&node_t4, &[&node_t4, &node_x45, &node_t5]).await;

    // Trampoline forwarding will call into routing/path finding.
    reset_find_path_call_count_for_tests();

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
                TrampolineHop::new(node_t3.get_public_key()),
                TrampolineHop::new(node_t4.get_public_key()),
                TrampolineHop::new(node_t5.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    node_a.wait_until_success(res.unwrap().payment_hash).await;

    // At minimum: payer builds route to first trampoline (1) + each trampoline forwards (5).
    let calls = find_path_call_count_for_tests();
    assert!(
        calls >= 6,
        "expected at least 6 path-find calls (payer + 5 trampolines), got {calls}"
    );
}

#[tokio::test]
async fn test_trampoline_routing_single_trampoline_hop_succeeds_with_long_public_prefix_path() {
    init_tracing();

    // Only the last hop is private; all prior channels are public.
    // Use a longer public prefix than `test_trampoline_routing_single_trampoline_hop_succeeds_with_public_prefix_path`.
    //
    // A --(public)--> T1 --(public)--> X12 --(public)--> T2 --(public)--> X23 --(public)--> T3
    //  --(public)--> X34 --(public)--> T4 --(public)--> X45 --(public)--> T5 --(private)--> C
    //
    // We should only need to specify a single trampoline hop: [T5].
    // Important: keep `max_fee_amount` reasonable; in trampoline routing it also contributes to
    // the amount sent to the first trampoline (forwarding fee budget).
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((5, 6), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((6, 7), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((7, 8), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((8, 9), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true),
            ((9, 10), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), false),
        ],
        11,
    )
    .await;

    let [node_a, node_t1, node_x12, node_t2, node_x23, node_t3, node_x34, node_t4, node_x45, node_t5, node_c] =
        nodes.try_into().expect("11 nodes");

    // Capture initial balances for all nodes
    let all_nodes = [
        &node_a, &node_t1, &node_x12, &node_t2, &node_x23, &node_t3, &node_x34, &node_t4,
        &node_x45, &node_t5, &node_c,
    ];
    let balances_before = capture_balances(&all_nodes, &channels);

    // Ensure A knows T5 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t5).await;

    // Ensure A has the public graph needed to route to T5.
    wait_until_graph_channel_updates_along_directed_path(
        &node_a,
        &[
            &node_a, &node_t1, &node_x12, &node_t2, &node_x23, &node_t3, &node_x34, &node_t4,
            &node_x45, &node_t5,
        ],
    )
    .await;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(100_000),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t5.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let response = res.unwrap();
    let payment_hash = response.payment_hash;

    node_a.wait_until_success(payment_hash).await;

    // Capture balances after payment and calculate changes
    let balances_after = capture_balances(&all_nodes, &channels);
    let balance_changes = calculate_balance_changes(&balances_before, &balances_after);

    // Verify balance changes
    debug!("Balance changes for all nodes:");
    for (idx, (node, changes)) in all_nodes.iter().zip(balance_changes.iter()).enumerate() {
        debug!(
            "  Node {}: pubkey={:?}, changes={:?}",
            idx,
            node.get_public_key(),
            changes
        );
    }

    // Node A (sender) should have paid amount + fee
    let node_a_total_paid: i128 = balance_changes[0]
        .iter()
        .filter(|&&c| c < 0)
        .sum::<i128>()
        .abs();
    debug!("Node A total paid: {}", node_a_total_paid);
    assert_eq!(
        node_a_total_paid as u128,
        amount + response.fee,
        "Node A should pay amount + fee"
    );

    // Node C (receiver) should have received exactly the amount
    let node_c_total_received: i128 = balance_changes[10].iter().filter(|&&c| c > 0).sum();
    debug!("Node C total received: {}", node_c_total_received);
    assert_eq!(
        node_c_total_received as u128, amount,
        "Node C should receive exactly the amount"
    );

    // All intermediate nodes should have earned fees (net positive or zero)
    for (idx, changes) in balance_changes.iter().enumerate().skip(1).take(9) {
        let net_change: i128 = changes.iter().sum();
        debug!("Node {} net change: {}", idx, net_change);
        assert!(
            net_change >= 0,
            "Intermediate node {} should not lose money",
            idx
        );
    }
}

#[tokio::test]
async fn test_trampoline_routing_four_hops_with_public_paths_between_trampolines() {
    init_tracing();

    // A --(public)--> T1 --(public)--> X --(public)--> T2 --(private)--> T3 --(public)--> Y --(public)--> T4 --(private)--> C
    // The forward hops between trampolines can be routed over public multi-hop paths (T1->T2 and T3->T4).
    // The last hop to the recipient is private, so A cannot build a direct route from the gossip graph.
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((3, 4), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
            ((4, 5), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((5, 6), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((6, 7), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        8,
    )
    .await;

    let [node_a, node_t1, node_x, node_t2, node_t3, node_y, node_t4, node_c] =
        nodes.try_into().expect("8 nodes");

    // Capture initial balances for all nodes
    let all_nodes = [
        &node_a, &node_t1, &node_x, &node_t2, &node_t3, &node_y, &node_t4, &node_c,
    ];
    let balances_before = capture_balances(&all_nodes, &channels);

    // Wait until A learns T1 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;

    // Wait until A has the public channel update to reach the first trampoline.
    // (Channel announcements alone are not sufficient for path finding.)
    wait_until_graph_channel_has_update(&node_a, &node_a, &node_t1).await;

    // Wait until T1 has the public graph needed to route to T2 via X.
    wait_until_node_has_public_channels_at_least(&node_t1, 3).await;

    // Wait until T3 has the public graph needed to route to T4 via Y.
    wait_until_node_has_public_channels_at_least(&node_t3, 2).await;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(1000),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t2.get_public_key()),
                TrampolineHop::new(node_t4.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let response = res.unwrap();
    let payment_hash = response.payment_hash;

    node_a.wait_until_success(payment_hash).await;

    // Capture balances after payment and calculate changes
    let balances_after = capture_balances(&all_nodes, &channels);
    let balance_changes = calculate_balance_changes(&balances_before, &balances_after);

    // Verify balance changes
    debug!("Balance changes for all nodes:");
    for (idx, (node, changes)) in all_nodes.iter().zip(balance_changes.iter()).enumerate() {
        debug!(
            "  Node {}: pubkey={:?}, changes={:?}",
            idx,
            node.get_public_key(),
            changes
        );
    }

    // Node A (sender) should have paid amount + fee
    let node_a_total_paid: i128 = balance_changes[0]
        .iter()
        .filter(|&&c| c < 0)
        .sum::<i128>()
        .abs();
    debug!("Node A total paid: {}", node_a_total_paid);
    assert_eq!(
        node_a_total_paid as u128,
        amount + response.fee,
        "Node A should pay amount + fee"
    );

    // Node C (receiver) should have received exactly the amount
    let node_c_total_received: i128 = balance_changes[7].iter().filter(|&&c| c > 0).sum();
    debug!("Node C total received: {}", node_c_total_received);
    assert_eq!(
        node_c_total_received as u128, amount,
        "Node C should receive exactly the amount"
    );

    // All intermediate nodes should have earned fees (net positive or zero)
    for (idx, changes) in balance_changes.iter().enumerate().skip(1).take(6) {
        let net_change: i128 = changes.iter().sum();
        debug!("Node {} net change: {}", idx, net_change);
        assert!(
            net_change >= 0,
            "Intermediate node {} should not lose money",
            idx
        );
    }

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    // first try without specifying right trampoline hops
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(1000),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t4.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(1000),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t2.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    node_a.wait_until_failed(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_forwarding_prefers_better_channel_private_vs_public() {
    init_tracing();

    // A --(public)--> T1 --(public, expensive)--> C
    //               \-> (private, cheap) ------> C
    // Force trampoline routing via [T1]. When T1 forwards to C it should prefer the cheaper
    // channel regardless of whether it is private/public.
    let (nodes, channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    a_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    // Public channel is intentionally expensive.
                    a_tlc_fee_proportional_millionths: Some(1_000_000_000),
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: false,
                    node_a_funding_amount: MIN_RESERVED_CKB + 100000,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    // Private channel is cheap.
                    a_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
        ],
        3,
        None,
    )
    .await;

    let [node_a, node_t1, node_c] = nodes.try_into().expect("3 nodes");

    let channel_a_t1 = channels[0];
    let channel_t1_c_public = channels[1];
    let channel_t1_c_private = channels[2];

    // Wait until A learns T1 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;

    // Ensure A has a usable public channel update to reach the first trampoline.
    wait_until_graph_channel_has_update(&node_a, &node_a, &node_t1).await;

    // Create an invoice on C that explicitly allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    // Snapshot balances on both candidate T1->C channels so we can assert which one was used.
    let public_local_before = node_t1.get_local_balance_from_channel(channel_t1_c_public);
    let private_local_before = node_t1.get_local_balance_from_channel(channel_t1_c_private);

    // Keep the fee budget small enough that the expensive public channel is not feasible;
    // forwarding must choose the cheaper private channel.
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t1.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_success(res.unwrap().payment_hash).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Assert: the public channel was NOT used, and the private channel WAS used.
    // (If the public channel had been selected, it would show a balance change on T1.)
    let public_local_after = node_t1.get_local_balance_from_channel(channel_t1_c_public);
    let private_local_after = node_t1.get_local_balance_from_channel(channel_t1_c_private);
    assert_eq!(
        public_local_after, public_local_before,
        "public channel should not be used (channel_a_t1={channel_a_t1:?}, channel_t1_c_public={channel_t1_c_public:?}, channel_t1_c_private={channel_t1_c_private:?})"
    );
    assert!(
        private_local_after < private_local_before,
        "private channel should be used (channel_t1_c_private={channel_t1_c_private:?})"
    );
    assert!(
        private_local_before.saturating_sub(private_local_after) >= amount,
        "private channel local balance should decrease by at least the payment amount"
    );
}

#[tokio::test]
async fn test_trampoline_private_channel_basic() {
    init_tracing();

    // A --(private)--> B --(public)--> C --(public)--> D
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
        ],
        4,
    )
    .await;

    let [node_a, _node_b, _node_c, node_d] = nodes.try_into().expect("3 nodes");

    let res = node_a
        .send_payment_keysend(&node_d, 1_000, false)
        .await
        .unwrap();

    eprintln!("payment sent with hash: {:?}", res.payment_hash);
    node_a.wait_until_success(res.payment_hash).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn test_trampoline_routing_connect_two_networks() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true)],
        2,
    )
    .await;

    let [_node_a, mut node_b] = nodes.try_into().expect("3 nodes");

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
        ],
        3,
    )
    .await;

    let [mut node_d, _node_e, node_f] = nodes.try_into().expect("3 nodes");
    let node_b_nodes = node_b.get_network_nodes().await;

    assert!(!node_b_nodes.iter().any(|n| n.node_id == node_d.pubkey));

    // now create a private channel for node_b and node_d
    node_b.connect_to(&mut node_d).await;

    // ChannelAnnouncement verification requires node_b's chain actor to know the funding txs
    // for the remote cluster's channels. In this test each cluster has its own mock chain,
    // so we explicitly sync those funding txs to node_b.
    let remote_funding_txs = node_d.channels_tx_map.values().copied().collect::<Vec<_>>();
    for tx_hash in remote_funding_txs {
        if let Some(tx) = node_d.get_transaction_view_from_hash(tx_hash).await {
            let _ = node_b.submit_tx(tx).await;
        }
    }

    // Wait for gossip to merge the two graphs.
    for _ in 0..40 {
        let node_b_channels = node_b.get_network_channels().await;
        let node_b_nodes = node_b.get_network_nodes().await;
        if node_b_nodes.len() >= 5 && node_b_channels.len() >= 3 {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let _res = create_channel_with_nodes(
        &mut node_b,
        &mut node_d,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let node_b_channels = node_b.get_network_channels().await;
    let node_b_nodes = node_b.get_network_nodes().await;

    // node_b nodes contains node_d
    assert!(node_b_nodes.iter().any(|n| n.node_id == node_d.pubkey));

    assert_eq!(node_b_nodes.len(), 5);
    assert_eq!(node_b_channels.len(), 4);

    let payment = node_b
        .send_payment_keysend(&node_f, 1_000, false)
        .await
        .unwrap();
    node_b.wait_until_success(payment.payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_forwarding_respects_tlc_expiry_limit_from_payload() {
    init_tracing();

    // A --(public)--> T1 --(private)--> X --(private)--> C
    // A can only route to T1; T1 must forward over 2 private hops.
    // Set a tight `tlc_expiry_limit` that is sufficient for A->T1, but insufficient for T1->X->C.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        4,
    )
    .await;

    let [node_a, node_t1, _node_x, node_c] = nodes.try_into().expect("4 nodes");

    // Ensure payer can route to the first trampoline.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;
    wait_until_graph_channel_has_update(&node_a, &node_a, &node_t1).await;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    // Tight limit: large enough for payer's trampoline slack (final + 1*DEFAULT), but too small
    // for T1 to reach C over 2 hops (final + 2*DEFAULT).
    let tight_limit = DEFAULT_FINAL_TLC_EXPIRY_DELTA + DEFAULT_TLC_EXPIRY_DELTA + 1;

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(100_000),
            tlc_expiry_limit: Some(tight_limit),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t1.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    node_a.wait_until_failed(payment_hash).await;
    let payment_res = node_a.get_payment_result(payment_hash).await;
    assert!(payment_res.failed_error.is_some());
}

#[tokio::test]
async fn test_trampoline_error_wrapping_propagates_to_payer() {
    init_tracing();

    // A --(public)--> T1 --(public)--> T2 --(private)--> C
    // Build a trampoline-routed payment but make the final recipient fail (missing invoice),
    // then assert the payer sees the trampoline failure wrapper.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        4,
    )
    .await;

    let [node_a, node_t1, node_t2, node_c] = nodes.try_into().expect("4 nodes");

    // Wait until A learns T1 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;

    // Wait until A learns enough public channels to reach the first trampoline.
    wait_until_node_has_public_channels_at_least(&node_a, 2).await;

    // Build an invoice for C but do NOT insert it on C, so the payment fails at the final
    // recipient (beyond the trampoline boundary) and must be wrapped for the payer.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    // don't insert invoice on node_c
    // node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    node_a.wait_until_failed(payment_hash).await;

    let payment_res = node_a.get_payment_result(payment_hash).await;
    assert!(payment_res.failed_error.is_some());
}

#[tokio::test]
async fn test_trampoline_forwarding_fee_insufficient_due_to_rate_cap() {
    init_tracing();

    // A --(public, cheap)--> T1 --(public, expensive)--> C
    // Payer's max_fee_amount is clamped by max_fee_rate, causing T1's forwarding fee check to fail.
    let usable_cap = 20_000_000;
    let common_funding = MIN_RESERVED_CKB + usable_cap;

    let channels_params = vec![
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        (
            (1, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(10000),
                ..Default::default()
            },
        ),
    ];

    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 3, None).await;
    let [node_a, node_t1, node_c] = nodes.try_into().expect("3 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;
    wait_until_node_has_public_channels_at_least(&node_a, 2).await;

    let amount: u128 = 2000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t1.get_public_key())]),
            max_fee_amount: Some(10_000_000),
            max_fee_rate: Some(5),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    node_a.wait_until_failed(payment_hash).await;
    let payment_res = node_a.get_payment_result(payment_hash).await;
    let failed_error = payment_res.failed_error.unwrap_or_default();
    debug!("Payment failed error: {failed_error}");
    assert!(failed_error.contains("FeeInsufficient"));
}

#[tokio::test]
async fn test_trampoline_forwarding_fee_insufficient_manual_packet() {
    init_tracing();
    // A -- B. We test B.
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true)],
        2,
    )
    .await;

    // Deconstruct nodes
    let _node_a = &nodes[0];
    let node_b = &nodes[1];
    let channel_ab = channels[0];

    let secp = Secp256k1::new();
    let payment_hash = gen_rand_sha256_hash();

    // 1. Construct Trampoline Onion for B
    // B should forward 1000 to some final target.
    let (_sk, pk) = secp.generate_keypair(&mut rand::thread_rng());
    let final_target: Pubkey = pk.into();

    let forward_payload = TrampolineHopPayload::Forward {
        next_node_id: final_target,
        amount_to_forward: 1000,
        build_max_fee_amount: Some(0),
        tlc_expiry_delta: 144,
        tlc_expiry_limit: 5000,
        max_parts: None,
        hash_algorithm: HashAlgorithm::Sha256,
    };

    // Path: [B, Final].
    let payloads = vec![
        forward_payload,
        TrampolineHopPayload::Final {
            final_amount: 1000,
            final_tlc_expiry_delta: 144,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let path = vec![node_b.pubkey, final_target];

    let session_key = Privkey::from_slice(&[2u8; 32]);
    let trampoline_onion_bytes = TrampolineOnionPacket::create(
        session_key,
        path,
        payloads,
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create onion")
    .into_bytes();

    // 2. Mock incoming packet stats
    // Incoming amount 900 < 1000
    let peeled_packet = PeeledPaymentOnionPacket {
        current: CurrentPaymentHopData {
            amount: 900,
            expiry: 5000,
            payment_preimage: None,
            hash_algorithm: HashAlgorithm::Sha256,
            funding_tx_hash: Default::default(),
            custom_records: None,
            trampoline_onion: Some(trampoline_onion_bytes),
        },
        shared_secret: [0u8; 32],
        next: None,
    };

    let (sender, receiver) = oneshot::channel();
    let command = NetworkActorCommand::SendPaymentOnionPacket(
        SendOnionPacketCommand {
            peeled_onion_packet: peeled_packet,
            previous_tlc: Some(PrevTlcInfo::new(channel_ab, 1, 0)),
            payment_hash,
            attempt_id: None,
        },
        RpcReplyPort::from(sender),
    );

    node_b
        .network_actor
        .send_message(NetworkActorMessage::Command(command))
        .expect("send command");

    // 3. Assert Error
    let res = receiver.await.expect("recv result");
    match res {
        Err(tlc_err) => {
            assert_eq!(tlc_err.error_code, TlcErrorCode::FeeInsufficient);
        }
        Ok(_) => panic!("Should have failed with FeeInsufficient"),
    }
}

#[tokio::test]
async fn test_trampoline_forwarding_fee_insufficient_equal_amount() {
    init_tracing();
    // A -- B. We test B.
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true)],
        2,
    )
    .await;

    // Deconstruct nodes
    let node_b = &nodes[1];
    let channel_ab = channels[0];

    let secp = Secp256k1::new();
    let payment_hash = gen_rand_sha256_hash();

    // Construct Trampoline Onion for B
    let (_sk, pk) = secp.generate_keypair(&mut rand::thread_rng());
    let final_target: Pubkey = pk.into();

    let forward_payload = TrampolineHopPayload::Forward {
        next_node_id: final_target,
        amount_to_forward: 1000,
        build_max_fee_amount: Some(0),
        tlc_expiry_delta: 144,
        tlc_expiry_limit: 5000,
        max_parts: None,
        hash_algorithm: HashAlgorithm::Sha256,
    };

    // Path: [B, Final].
    let payloads = vec![
        forward_payload,
        TrampolineHopPayload::Final {
            final_amount: 1000,
            final_tlc_expiry_delta: 144,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let path = vec![node_b.pubkey, final_target];

    let session_key = Privkey::from_slice(&[2u8; 32]);
    let trampoline_onion_bytes = TrampolineOnionPacket::create(
        session_key,
        path,
        payloads,
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create onion")
    .into_bytes();

    // Incoming amount equals amount_to_forward -> should be treated as insufficient fee
    let peeled_packet = PeeledPaymentOnionPacket {
        current: CurrentPaymentHopData {
            amount: 1000,
            expiry: 5000,
            payment_preimage: None,
            hash_algorithm: HashAlgorithm::Sha256,
            funding_tx_hash: Default::default(),
            custom_records: None,
            trampoline_onion: Some(trampoline_onion_bytes),
        },
        shared_secret: [0u8; 32],
        next: None,
    };

    let (sender, receiver) = oneshot::channel();
    let command = NetworkActorCommand::SendPaymentOnionPacket(
        SendOnionPacketCommand {
            peeled_onion_packet: peeled_packet,
            previous_tlc: Some(PrevTlcInfo::new(channel_ab, 1, 0)),
            payment_hash,
            attempt_id: None,
        },
        RpcReplyPort::from(sender),
    );

    node_b
        .network_actor
        .send_message(NetworkActorMessage::Command(command))
        .expect("send command");

    let res = receiver.await.expect("recv result");
    match res {
        Err(tlc_err) => {
            assert_eq!(tlc_err.error_code, TlcErrorCode::FeeInsufficient);
        }
        Ok(_) => panic!("Should have failed with FeeInsufficient"),
    }
}

#[tokio::test]
async fn test_trampoline_routing_loop_failure_insufficient_fee() {
    init_tracing();

    // A -- B -- C -- D -- E
    // A uses B -> C -> D as trampoline hops to pay E.
    // Path: A -> B -> C -> D -> E.
    // Test expects failure because max_fee_amount is set to 0.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((2, 3), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((3, 4), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
        ],
        5,
    )
    .await;

    let [node_a, node_b, node_c, node_d, node_e] = nodes.try_into().expect("5 nodes");

    // Ensure A has full graph info to validate trampoline supports
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;
    wait_until_node_supports_trampoline_routing(&node_a, &node_c).await;
    wait_until_node_supports_trampoline_routing(&node_a, &node_d).await;
    wait_until_node_supports_trampoline_routing(&node_a, &node_e).await;

    let amount = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_e.get_public_key().into())
        .build()
        .expect("build invoice");

    node_e.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            // Set fee budget to 0, which should be insufficient for 3 trampoline hops
            max_fee_amount: Some(0),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_b.get_public_key()),
                TrampolineHop::new(node_c.get_public_key()),
                TrampolineHop::new(node_d.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;

    // Should fail locally at A during route/fee calculation
    assert!(res.is_err());
    let err_msg = res.err().unwrap().to_string();
    assert!(err_msg.contains("max_fee_amount too low") || err_msg.contains("fee"));
    // Match graph.rs error
}

#[tokio::test]
async fn test_trampoline_routing_retry_with_intermediate_failure() {
    init_tracing();

    // Node indices:
    // A: 0
    // B: 1 (Trampoline)
    // C: 2 (Receiver)
    // P1: 3 (Path 1 - Cheap but Broken)
    // P2: 4 (Path 2 - Expensive but Working)

    let usable_cap = 20_000_000;
    let common_funding = MIN_RESERVED_CKB + usable_cap;

    let channels_params = vec![
        // A -> B
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                ..Default::default()
            },
        ),
        // B -> P1 (Low Fee)
        (
            (1, 3),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // P1 -> C (Low Fee)
        (
            (3, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // B -> P2 (High Fee)
        (
            (1, 4),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(10000),
                ..Default::default()
            },
        ),
        // P2 -> C (Low Fee)
        (
            (4, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
    ];

    let (nodes, _channels_map) =
        create_n_nodes_network_with_params(&channels_params, 5, None).await;
    let [node_a, node_b, node_c, node_p1, _node_p2] = nodes.try_into().expect("5 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;
    wait_until_node_has_public_channels_at_least(&node_b, 5).await;

    // Drain P1 -> C
    let drain_amount = usable_cap - 1000;

    let preimage_drain = gen_rand_sha256_hash();
    let invoice_drain = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(drain_amount))
        .payment_preimage(preimage_drain)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("drain invoice");
    node_c.insert_invoice(invoice_drain.clone(), Some(preimage_drain));

    let res = node_p1
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice_drain.to_string()),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    node_p1.wait_until_success(res.unwrap().payment_hash).await;

    // Trampoline Payment A -> B -> C
    // Amount 2000 (Greater than 1000 left on P1->C)
    // B should try B->P1->C first (cost 0), fail, then retry B->P2->C.

    let amount_expr = 2000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount_expr))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("expr invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            max_fee_amount: Some(1000000),
            max_fee_rate: Some(10),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_two_hops_both_retry_success() {
    init_tracing();
    // Topology: A -> T1 -> T2 -> C
    // T1 -> T2 has 2 paths: via P1 (drained/fail), via P2 (ok)
    // T2 -> C has 2 paths: via Q1 (drained/fail), via Q2 (ok)

    // Indices:
    // A: 0
    // T1: 1
    // T2: 2
    // C: 3
    // P1: 4
    // P2: 5
    // Q1: 6
    // Q2: 7

    let usable_cap = 20_000_000;
    let common_funding = MIN_RESERVED_CKB + usable_cap;

    let channels_params = vec![
        // A -> T1
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                ..Default::default()
            },
        ),
        // T1 -> P1 (Cheap, to be drained)
        (
            (1, 4),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // P1 -> T2
        (
            (4, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // T1 -> P2 (Expensive, working)
        (
            (1, 5),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(10000), // Expensive
                ..Default::default()
            },
        ),
        // P2 -> T2
        (
            (5, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // T2 -> Q1 (Cheap, to be drained)
        (
            (2, 6),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // Q1 -> C
        (
            (6, 3),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // T2 -> Q2 (Expensive, working)
        (
            (2, 7),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(10000), // Expensive
                ..Default::default()
            },
        ),
        // Q2 -> C
        (
            (7, 3),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
    ];

    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 8, None).await;
    let [node_a, node_t1, node_t2, node_c, node_p1, _node_p2, node_q1, _node_q2] =
        nodes.try_into().expect("8 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;
    wait_until_node_supports_trampoline_routing(&node_a, &node_t2).await;

    // We need to wait for public channel updates. With 8 nodes, propagation might take a moment.
    wait_until_node_has_public_channels_at_least(&node_t1, 9).await; // 9 total channels in network
    wait_until_node_has_public_channels_at_least(&node_t2, 9).await;

    // Drain P1 (T1 -> P1 -> T2 path)
    let drain_amount = usable_cap - 1000;

    // Drain P1->T2
    {
        let preimage = gen_rand_sha256_hash();
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(drain_amount))
            .payment_preimage(preimage)
            .payee_pub_key(node_t2.get_public_key().into())
            .build()
            .unwrap();
        node_t2.insert_invoice(invoice.clone(), Some(preimage));
        let res = node_p1
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                ..Default::default()
            })
            .await;
        assert!(res.is_ok());
        node_p1.wait_until_success(res.unwrap().payment_hash).await;
    }

    // Drain Q1->C (T2 -> Q1 -> C path)
    {
        let preimage = gen_rand_sha256_hash();
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(drain_amount))
            .payment_preimage(preimage)
            .payee_pub_key(node_c.get_public_key().into())
            .build()
            .unwrap();
        node_c.insert_invoice(invoice.clone(), Some(preimage));
        let res = node_q1
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                ..Default::default()
            })
            .await;
        assert!(res.is_ok());
        node_q1.wait_until_success(res.unwrap().payment_hash).await;
    }

    // Now send actual payment A -> T1 -> T2 -> C.
    // Amount 2000.
    // T1 will try T1->P1->T2. P1->T2 has 1000 < 2000. Fail.
    // T1 should retry T1->P2->T2. works.
    // T2 receives via P2.
    // T2 will try T2->Q1->C. Q1->C has 1000 < 2000. Fail.
    // T2 should retry T2->Q2->C. works.

    let amount = 2000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .unwrap();
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
            ]),
            max_fee_amount: Some(10_000_000), // Enough for expensive paths
            max_fee_rate: Some(1000),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_mid_failure_propagates_back() {
    init_tracing();
    // Topology: A -> T1 -> T2 -> C
    // T1->T2: 1 path (Direct or indirect, doesn't matter, assume working)
    // T2->C: 2 paths (Q1, Q2), both drained.

    // Indices:
    // A: 0
    // T1: 1
    // T2: 2
    // C: 3
    // Q1: 4
    // Q2: 5

    let usable_cap = 20_000_000;
    let common_funding = MIN_RESERVED_CKB + usable_cap;

    let channels_params = vec![
        // A -> T1
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                ..Default::default()
            },
        ),
        // T1 -> T2 (Direct for simplicity here)
        (
            (1, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                ..Default::default()
            },
        ),
        // T2 -> Q1
        (
            (2, 4),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // Q1 -> C
        (
            (4, 3),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                ..Default::default()
            },
        ),
        // T2 -> Q2
        (
            (2, 5),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                a_tlc_fee_proportional_millionths: Some(0),
                ..Default::default()
            },
        ),
        // Q2 -> C
        (
            (5, 3),
            ChannelParameters {
                public: true,
                node_a_funding_amount: common_funding,
                node_b_funding_amount: common_funding,
                ..Default::default()
            },
        ),
    ];

    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 6, None).await;
    let [node_a, node_t1, node_t2, node_c, node_q1, node_q2] = nodes.try_into().expect("6 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t1).await;
    wait_until_node_supports_trampoline_routing(&node_a, &node_t2).await;
    wait_until_node_has_public_channels_at_least(&node_t2, 6).await;

    // Drain Q1->C
    {
        let drain_amount = usable_cap - 1000;
        let preimage = gen_rand_sha256_hash();
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(drain_amount))
            .payment_preimage(preimage)
            .payee_pub_key(node_c.get_public_key().into())
            .build()
            .unwrap();
        node_c.insert_invoice(invoice.clone(), Some(preimage));
        let res = node_q1
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                ..Default::default()
            })
            .await;
        assert!(res.is_ok());
        node_q1.wait_until_success(res.unwrap().payment_hash).await;
    }

    // Drain Q2->C
    {
        let drain_amount = usable_cap - 1000;
        let preimage = gen_rand_sha256_hash();
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(drain_amount))
            .payment_preimage(preimage)
            .payee_pub_key(node_c.get_public_key().into())
            .build()
            .unwrap();
        node_c.insert_invoice(invoice.clone(), Some(preimage));
        let res = node_q2
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                ..Default::default()
            })
            .await;
        assert!(res.is_ok());
        node_q2.wait_until_success(res.unwrap().payment_hash).await;
    }

    // Payment A -> T1 -> T2 -> C. Amount 2000.
    // T2 fails to find path to C (or finds it but fails at runtime).
    // T2 should report error back to T1, T1 back to A.

    let amount = 2000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .unwrap();
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t1.get_public_key()),
                TrampolineHop::new(node_t2.get_public_key()),
            ]),
            max_fee_amount: Some(10_000_000),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    // Wait for failure
    let payment_hash = res.unwrap().payment_hash;
    node_a.wait_until_failed(payment_hash).await;
    let payment_res = node_a.get_payment_result(payment_hash).await;
    assert!(payment_res.failed_error.is_some());
}

#[tokio::test]
async fn test_trampoline_routing_concurrent_payments() {
    init_tracing();
    // Topology:
    // A -> T -> B
    // C -> T -> D
    // Four distinct payments passing through T concurrently.

    let channels_params = vec![
        // A -> T
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        // T -> B
        (
            (1, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        // C -> T
        (
            (3, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        // T -> D
        (
            (1, 4),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
    ];

    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 5, None).await;
    let [node_a, node_t, node_b, node_c, node_d] = nodes.try_into().expect("5 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t).await;
    wait_until_node_supports_trampoline_routing(&node_c, &node_t).await;
    wait_until_node_has_public_channels_at_least(&node_t, 4).await;
    wait_until_node_has_public_channels_at_least(&node_a, 4).await;
    wait_until_node_has_public_channels_at_least(&node_c, 4).await;

    // Prepare Payment 1: A -> T -> B
    let amount1 = 1000;
    let preimage1 = gen_rand_sha256_hash();
    let invoice1 = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount1))
        .payment_preimage(preimage1)
        .payee_pub_key(node_b.get_public_key().into())
        .build()
        .unwrap();
    node_b.insert_invoice(invoice1.clone(), Some(preimage1));

    // Prepare Payment 2: C -> T -> D
    let amount2 = 2000;
    let preimage2 = gen_rand_sha256_hash();
    let invoice2 = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount2))
        .payment_preimage(preimage2)
        .payee_pub_key(node_d.get_public_key().into())
        .build()
        .unwrap();
    node_d.insert_invoice(invoice2.clone(), Some(preimage2));

    // Execute concurrently
    let pay1_fut = node_a.send_payment(SendPaymentCommand {
        invoice: Some(invoice1.to_string()),
        trampoline_hops: Some(vec![TrampolineHop::new(node_t.get_public_key())]),
        max_fee_amount: Some(amount1),
        ..Default::default()
    });

    let pay2_fut = node_c.send_payment(SendPaymentCommand {
        invoice: Some(invoice2.to_string()),
        trampoline_hops: Some(vec![TrampolineHop::new(node_t.get_public_key())]),
        max_fee_amount: Some(amount2),
        ..Default::default()
    });

    let (res1, res2) = tokio::join!(pay1_fut, pay2_fut);

    assert!(res1.is_ok());
    assert!(res2.is_ok());

    let hash1 = res1.unwrap().payment_hash;
    let hash2 = res2.unwrap().payment_hash;

    // Wait for both
    let wait1 = node_a.wait_until_success(hash1);
    let wait2 = node_c.wait_until_success(hash2);

    tokio::join!(wait1, wait2);
}

#[tokio::test]
async fn test_trampoline_routing_no_path_found() {
    init_tracing();
    // A -> T.
    // B exists but is disconnected from T.
    // A -> T -> B.

    let channels_params = vec![
        // A -> T
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
    ];

    // 3 nodes: A, T, B. B is isolated.
    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 3, None).await;
    let [node_a, node_t, node_b] = nodes.try_into().expect("3 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t).await;

    let amount = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_b.get_public_key().into())
        .build()
        .unwrap();
    // Don't insert invoice on B? Actually B doesn't need to be online for T to fail pathfinding.
    // But if we want to be correct, B can have it.
    node_b.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(1000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t.get_public_key())]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_a.wait_until_failed(payment_hash).await;
    let result = node_a.get_payment_result(payment_hash).await;
    let err = result.failed_error.unwrap();
    // Likely "RouteNotFound" or similar wrapping.
    debug!("Payment failed as expected with: {:?}", err);
}

#[tokio::test]
async fn test_trampoline_routing_race_same_invoice() {
    init_tracing();

    let channels_params = vec![
        (
            (0, 1), // A -> T
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        (
            (2, 1), // B -> T
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        (
            (1, 3), // T -> C
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
    ];

    // 4 nodes: A(0), T(1), B(2), C(3)
    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 4, None).await;
    let [node_a, node_t, node_b, node_c] = nodes.try_into().expect("4 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t).await;
    wait_until_node_supports_trampoline_routing(&node_b, &node_t).await;

    // C creates invoice
    let amount = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .unwrap();
    node_c.insert_invoice(invoice.clone(), Some(preimage));
    let invoice_str = invoice.to_string();

    debug!("Invoice created: {}", invoice_str);

    // Helper to wait for outcome
    async fn get_outcome(node: &NetworkNode, payment_hash: Hash256) -> String {
        let start = std::time::Instant::now();
        loop {
            if start.elapsed().as_secs() > 10 {
                return "timeout".to_string();
            }
            let res = node.get_payment_result(payment_hash).await;
            match res.status {
                PaymentStatus::Success => return "success".to_string(),
                PaymentStatus::Failed => return "failure".to_string(),
                _ => {}
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    let t_pk = node_t.get_public_key();

    // A pays
    let invoice_a = invoice_str.clone();
    let future_a = async {
        let res = node_a
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice_a),
                max_fee_amount: Some(amount),
                trampoline_hops: Some(vec![TrampolineHop::new(t_pk)]),
                ..Default::default()
            })
            .await;

        match res {
            Ok(pc) => get_outcome(&node_a, pc.payment_hash).await,
            Err(_) => "send_error".to_string(),
        }
    };

    // B pays
    let invoice_b = invoice_str.clone();
    let future_b = async {
        let res = node_b
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice_b),
                max_fee_amount: Some(amount),
                trampoline_hops: Some(vec![TrampolineHop::new(t_pk)]),
                ..Default::default()
            })
            .await;

        match res {
            Ok(pc) => get_outcome(&node_b, pc.payment_hash).await,
            Err(_) => "send_error".to_string(),
        }
    };

    let (outcome_a, outcome_b) = tokio::join!(future_a, future_b);

    debug!("A outcome: {}, B outcome: {}", outcome_a, outcome_b);

    // One must succeed, one must fail
    // Possible states: (success, failure) or (failure, success)
    let success =
        (if outcome_a == "success" { 1 } else { 0 }) + (if outcome_b == "success" { 1 } else { 0 });
    let failure =
        (if outcome_a == "failure" { 1 } else { 0 }) + (if outcome_b == "failure" { 1 } else { 0 });

    assert_eq!(success, 1, "Exactly one should succeed");
    assert_eq!(failure, 1, "Exactly one should fail");
}

#[tokio::test]
async fn test_trampoline_routing_failure_invalid_payment_secret() {
    init_tracing();
    // A -> T -> B
    // A uses an invoice with incorrect payment secret for B.
    // B should reject the payment.

    let channels_params = vec![
        (
            (0, 1), // A -> T
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        (
            (1, 2), // T -> B
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
    ];

    let (nodes, _) = create_n_nodes_network_with_params(&channels_params, 3, None).await;
    let [node_a, node_t, node_b] = nodes.try_into().expect("3 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_t).await;
    wait_until_node_supports_trampoline_routing(&node_b, &node_t).await;

    // 1. Create correct invoice on B with explicit secret
    let amount = 1000;
    let preimage = gen_rand_sha256_hash();
    let secret_real = gen_rand_sha256_hash();
    let invoice_real = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payment_secret(secret_real)
        .payee_pub_key(node_b.get_public_key().into())
        .build()
        .unwrap();
    node_b.insert_invoice(invoice_real.clone(), Some(preimage));

    // 2. Create fake invoice (same preimage/hash, different secret)
    let secret_fake = gen_rand_sha256_hash();
    let invoice_fake = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payment_secret(secret_fake)
        .payee_pub_key(node_b.get_public_key().into())
        .build()
        .unwrap();

    // Verify secrets are different
    assert_ne!(secret_real, secret_fake);

    // 3. A sends using fake invoice
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice_fake.to_string()),
            max_fee_amount: Some(amount),
            trampoline_hops: Some(vec![TrampolineHop::new(node_t.get_public_key())]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    // 4. Expect failure
    node_a.wait_until_failed(payment_hash).await;
    let result = node_a.get_payment_result(payment_hash).await;
    let err = result.failed_error.expect("should fail");

    debug!("Payment failed as expected: {:?}", err);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_trampoline_node_restart() {
    init_tracing();

    // A --(public)--> B --(private)--> C
    // Note: Use large amounts to ensure capacity.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 1000000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 1000000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, mut node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait until A learns B (public channel).
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    // Create an invoice on C.
    // Use a large timeout so we have time to restart B.
    let amount: u128 = 10000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .expiry_time(Duration::from_secs(3600)) // 1 hour
        .build()
        .expect("build invoice");

    // We insert invoice but NOT the preimage yet.
    // This allows C to receive the HTLC but hold it (unable to settle).
    let _ = node_c.store.insert_invoice(invoice.clone(), None);

    // Initial Payment A -> B -> C
    let payment_command = SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        max_fee_amount: Some(5000),
        trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
        ..Default::default()
    };

    let res = node_a.send_payment(payment_command).await;
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    debug!("Payment started: {:?}", payment_hash);

    // Helper to wait for C to have a pending received TLC.
    async fn wait_for_received_tlc(node: &NetworkNode, payment_hash: Hash256) {
        let now = std::time::Instant::now();
        while now.elapsed().as_secs() < 10 {
            let mut found = false;
            // Accessing hold tlcs for the node (final node)
            for (ph, _) in node.store.get_node_hold_tlcs() {
                if ph == payment_hash {
                    found = true;
                    break;
                }
            }
            if found {
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        panic!("Timed out waiting for received TLC on node C");
    }

    wait_for_received_tlc(&node_c, payment_hash).await;
    debug!("Node C received HTLC. Stopping Node B.");

    // Restart B
    node_b.restart().await;
    debug!("Node B restarted.");

    // Wait for channels to be ready/active again.
    // We can assume after connect_to, channel re-establishment kicks in.
    let start = std::time::Instant::now();
    loop {
        let channels = node_b.store.get_channel_states(None);
        let ready_count = channels
            .iter()
            .filter(|(_, _, state)| {
                matches!(*state, crate::fiber::channel::ChannelState::ChannelReady)
            })
            .count();
        if ready_count == 2 {
            break;
        }
        if start.elapsed().as_secs() > 10 {
            panic!(
                "Timeout waiting for channels ready. Expected 2, found {}",
                ready_count
            );
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    debug!("Inserting preimage to Node C to finish payment.");
    // Now give C the preimage.
    node_c.store.insert_preimage(payment_hash, preimage);

    // Trigger C to settle.
    node_c
        .network_actor
        .send_message(NetworkActorMessage::Command(
            NetworkActorCommand::SettleTlcSet(payment_hash, None),
        ))
        .expect("Failed to send settle command");

    debug!("Waiting for success on Node A...");
    // Wait for A to succeed.
    node_a.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_forward_invalid_onion_payload_missing_context() {
    init_tracing();

    // Create 2 nodes with 1 channel so we have a valid previous channel for forwarding check.
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true)],
        2,
    )
    .await;
    let node = &nodes[0];
    let next_node_pubkey = gen_rand_fiber_public_key();
    let payment_hash = gen_rand_sha256_hash();

    fn gen_rand_session_key() -> crate::fiber::types::Privkey {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 32];
        rng.fill(&mut key);
        key.into()
    }

    // 1. Construct a Trampoline Onion with Forward payload but no next hop (single hop).
    let hop_data = TrampolineHopPayload::Forward {
        next_node_id: next_node_pubkey,
        amount_to_forward: 1000,
        build_max_fee_amount: Some(1000),
        tlc_expiry_delta: 100,
        tlc_expiry_limit: 5000,
        max_parts: None,
        hash_algorithm: HashAlgorithm::Sha256,
    };

    let session_key = gen_rand_session_key();
    let secp = Secp256k1::new();
    let trampoline_packet = TrampolineOnionPacket::create(
        session_key,
        vec![node.pubkey],
        vec![hop_data],
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("build trampoline");

    // 2. Prepare the PeeledPaymentOnionPacket
    let current_hop = CurrentPaymentHopData {
        amount: 2000,
        expiry: 5000,
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::Sha256,
        funding_tx_hash: Default::default(),
        custom_records: None,
        trampoline_onion: Some(trampoline_packet.into_bytes()),
    };

    let peeled_onion = PeeledPaymentOnionPacket {
        current: current_hop,
        next: None,
        shared_secret: [0u8; 32],
    };

    let prev_tlc = PrevTlcInfo {
        prev_channel_id: channels[0],
        prev_tlc_id: 1, // arbitrary
        forwarding_fee: 0,
        shared_secret: None,
    };

    let command = SendOnionPacketCommand {
        peeled_onion_packet: peeled_onion,
        previous_tlc: Some(prev_tlc),
        payment_hash,
        attempt_id: Some(1),
    };

    // 3. Send the command to NetworkActor
    let (tx, rx) = tokio::sync::oneshot::channel();
    let port = ractor::RpcReplyPort::from(tx);
    let msg =
        NetworkActorMessage::Command(NetworkActorCommand::SendPaymentOnionPacket(command, port));

    node.network_actor.send_message(msg).expect("send message");

    let res = rx.await.expect("receive reply");

    assert!(res.is_err());
    let err = res.err().unwrap();

    // Expect InvalidOnionPayload because next onion is missing (it's the last hop) but payload is Forward.
    assert_eq!(err.error_code(), TlcErrorCode::InvalidOnionPayload);
}

#[tokio::test]
async fn test_trampoline_forward_invalid_amount_in_onion_packet() {
    init_tracing();

    // Create 2 nodes with 1 channel so we have a valid previous channel for forwarding check.
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true)],
        2,
    )
    .await;
    let node = &nodes[0];
    let next_node_pubkey = gen_rand_fiber_public_key();
    let payment_hash = gen_rand_sha256_hash();

    fn gen_rand_session_key() -> crate::fiber::types::Privkey {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 32];
        rng.fill(&mut key);
        key.into()
    }

    // 1. Construct a Trampoline Onion with Forward payload but no next hop (single hop).
    let hop_data = TrampolineHopPayload::Forward {
        next_node_id: next_node_pubkey,
        amount_to_forward: 1000,
        build_max_fee_amount: Some(1000),
        tlc_expiry_delta: 100,
        tlc_expiry_limit: 5000,
        max_parts: None,
        hash_algorithm: HashAlgorithm::Sha256,
    };

    let session_key = gen_rand_session_key();
    let secp = Secp256k1::new();
    let trampoline_packet = TrampolineOnionPacket::create(
        session_key,
        vec![node.pubkey],
        vec![hop_data],
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("build trampoline");

    // 2. Prepare the PeeledPaymentOnionPacket
    let current_hop = CurrentPaymentHopData {
        amount: 1100, // Invalid amount to build max_fee_amount
        expiry: 5000,
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::Sha256,
        funding_tx_hash: Default::default(),
        custom_records: None,
        trampoline_onion: Some(trampoline_packet.into_bytes()),
    };

    let peeled_onion = PeeledPaymentOnionPacket {
        current: current_hop,
        next: None,
        shared_secret: [0u8; 32],
    };

    let prev_tlc = PrevTlcInfo {
        prev_channel_id: channels[0],
        prev_tlc_id: 1, // arbitrary
        forwarding_fee: 0,
        shared_secret: None,
    };

    let command = SendOnionPacketCommand {
        peeled_onion_packet: peeled_onion,
        previous_tlc: Some(prev_tlc),
        payment_hash,
        attempt_id: Some(1),
    };

    // 3. Send the command to NetworkActor
    let (tx, rx) = tokio::sync::oneshot::channel();
    let port = ractor::RpcReplyPort::from(tx);
    let msg =
        NetworkActorMessage::Command(NetworkActorCommand::SendPaymentOnionPacket(command, port));

    node.network_actor.send_message(msg).expect("send message");

    let res = rx.await.expect("receive reply");

    assert!(res.is_err());
    let err = res.err().unwrap();

    // Expect InvalidOnionPayload because next onion is missing (it's the last hop) but payload is Forward.
    assert_eq!(err.error_code(), TlcErrorCode::InvalidOnionPayload);
}

#[tokio::test]
async fn test_trampoline_routing_mpp_last_hop() {
    init_tracing();

    // A --(public)--> B --(private, 2 channels)--> C
    // A sends to B (trampoline), B uses MPP to send to C.
    // Capacity A->B is large enough.
    // Capacity B->C is split across 2 channels, need MPP to fulfill large payment.

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + 10_000_000_000, MIN_RESERVED_CKB),
                true,
            ),
            (
                (1, 2),
                (MIN_RESERVED_CKB + 1000000000, MIN_RESERVED_CKB),
                false,
            ),
            (
                (1, 2),
                (MIN_RESERVED_CKB + 1000000000, MIN_RESERVED_CKB),
                false,
            ),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let node_b_channels = node_b.store.get_channel_states(None);
    debug!(
        "Node B: {:?} channels: {:?}",
        node_b.pubkey, node_b_channels
    );

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Use a larger amount than single channel capacity (1000000000) to force MPP
    let amount: u128 = 1500000000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .allow_trampoline_routing(true)
        .allow_mpp(true)
        .payee_pub_key(node_c.get_public_key().into())
        .description("mpp trampoline".to_string())
        .payment_secret(gen_rand_sha256_hash())
        .build();
    debug!("Built invoice: {:?}", invoice);
    let invoice = invoice.expect("build invoice");

    error!("node_c pubkey: {:?}", node_c.get_public_key());
    node_c.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    // Node A sends to C via B (Trampoline)
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    node_a.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_invoice_not_allow_mpp_will_fail() {
    init_tracing();

    // A --(public)--> B --(private, 2 channels)--> C
    // A sends to B (trampoline), B uses MPP to send to C.
    // Capacity A->B is large enough.
    // Capacity B->C is split across 2 channels, need MPP to fulfill large payment.

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + 10_000_000_000, MIN_RESERVED_CKB),
                true,
            ),
            (
                (1, 2),
                (MIN_RESERVED_CKB + 1000000000, MIN_RESERVED_CKB),
                false,
            ),
            (
                (1, 2),
                (MIN_RESERVED_CKB + 1000000000, MIN_RESERVED_CKB),
                false,
            ),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let node_b_channels = node_b.store.get_channel_states(None);
    debug!(
        "Node B: {:?} channels: {:?}",
        node_b.pubkey, node_b_channels
    );

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Use a larger amount than single channel capacity (1000000000) to force MPP
    let amount: u128 = 1500000000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .allow_trampoline_routing(true)
        .allow_mpp(false) // NOT allowing MPP
        .payee_pub_key(node_c.get_public_key().into())
        .payment_secret(gen_rand_sha256_hash())
        .build();
    debug!("Built invoice: {:?}", invoice);
    let invoice = invoice.expect("build invoice");

    error!("node_c pubkey: {:?}", node_c.get_public_key());
    node_c.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    // Node A sends to C via B (Trampoline)
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(18030000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    node_a.wait_until_failed(payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_mpp_intermediate_hop_will_fail() {
    init_tracing();

    // A --(public)--> B --(private, 2 channels)--> C --(private)--> D
    // A -> B (Trampoline) -> C (Trampoline) -> D
    // Payment from B to C requires MPP.

    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + 10_000_000_000, MIN_RESERVED_CKB),
                true,
            ),
            (
                (1, 2),
                (MIN_RESERVED_CKB + 1000000000, MIN_RESERVED_CKB),
                false,
            ),
            (
                (1, 2),
                (MIN_RESERVED_CKB + 1000000000, MIN_RESERVED_CKB),
                false,
            ),
            (
                (2, 3),
                (MIN_RESERVED_CKB + 3000000000, MIN_RESERVED_CKB),
                false,
            ),
        ],
        4,
    )
    .await;

    let [node_a, node_b, node_c, node_d] = nodes.try_into().expect("4 nodes");

    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    let amount: u128 = 1500000000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_d.get_public_key().into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .description("mpp trampoline intermediate".to_string())
        .build()
        .expect("build invoice");
    node_d.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    // A specifies B and C as trampoline hops.
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_b.get_public_key()),
                TrampolineHop::new(node_c.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    node_a.wait_until_failed(payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_dry_run_basic() {
    init_tracing();

    // A --(public)--> B --(private)--> C
    // Test that dry_run mode works correctly with trampoline routing.
    let (nodes, channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Record initial balances
    let node_a_initial = node_a.get_local_balance_from_channel(channels[0]);
    let node_b_initial_a_side = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_initial_c_side = node_b.get_local_balance_from_channel(channels[1]);
    let node_c_initial = node_c.get_local_balance_from_channel(channels[1]);

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    // Create an invoice on C.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");

    // First, do a dry run to check if the payment can be made
    let dry_run_res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(251),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            dry_run: true,
            ..Default::default()
        })
        .await;

    assert!(dry_run_res.is_ok(), "Dry run should succeed");
    let dry_run_response = dry_run_res.unwrap();

    // Verify dry run response contains route information
    assert_eq!(dry_run_response.status, PaymentStatus::Created);

    debug!("Dry run response: {:?}", dry_run_response);

    assert!(
        !dry_run_response.routers.is_empty(),
        "Dry run should return route information"
    );

    // For trampoline routing, fee should be calculated correctly
    // The fee includes both trampoline service fee and routing fee
    assert_eq!(dry_run_response.fee, 251);

    // Verify that no payment session was created for dry run
    // (We don't need to check the payment_hash since dry_run doesn't persist)

    // Now do the actual payment to verify the route works
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let actual_res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(251),
            max_fee_rate: Some(1000),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            dry_run: false,
            ..Default::default()
        })
        .await;

    assert!(actual_res.is_ok(), "Actual payment should succeed");
    let actual_payment_hash = actual_res.unwrap().payment_hash;

    node_a.wait_until_success(actual_payment_hash).await;

    // Verify the actual payment was processed successfully
    let final_payment = node_a.get_payment_result(actual_payment_hash).await;
    debug!("Final payment result: {:?}", final_payment);
    assert_eq!(final_payment.status, PaymentStatus::Success);
    assert_eq!(final_payment.fee, 251);

    // Verify balances after payment
    // Node A should have paid amount + fee = 1000 + 251 = 1251
    let node_a_final = node_a.get_local_balance_from_channel(channels[0]);
    let node_a_paid = node_a_initial - node_a_final;
    assert_eq!(node_a_paid, amount + 251, "Node A should pay amount + fee");
    debug!(
        "Node A paid: {} (amount: {}, fee: {})",
        node_a_paid, amount, 251
    );

    // Node C should have received exactly the amount (1000)
    let node_c_final = node_c.get_local_balance_from_channel(channels[1]);
    let node_c_received = node_c_final - node_c_initial;
    assert_eq!(
        node_c_received, amount,
        "Node C should receive exactly the amount"
    );
    debug!("Node C received: {}", node_c_received);

    // Node B (trampoline node) should have earned the fee
    // B's balance on A side should increase by what A paid
    let node_b_final_a_side = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_received_from_a = node_b_final_a_side - node_b_initial_a_side;
    assert_eq!(
        node_b_received_from_a,
        amount + 251,
        "Node B should receive amount + fee from A"
    );

    // B's balance on C side should decrease by what C received
    let node_b_final_c_side = node_b.get_local_balance_from_channel(channels[1]);
    let node_b_paid_to_c = node_b_initial_c_side - node_b_final_c_side;
    assert_eq!(
        node_b_paid_to_c, amount,
        "Node B should pay exactly amount to C"
    );

    // Node B's net gain should be the fee (received from A - paid to C)
    let node_b_net_gain = node_b_received_from_a - node_b_paid_to_c;
    assert_eq!(node_b_net_gain, 251, "Node B should earn the fee");
    debug!(
        "Node B earned fee: {} (received: {}, paid: {})",
        node_b_net_gain, node_b_received_from_a, node_b_paid_to_c
    );
}

#[tokio::test]
async fn test_trampoline_routing_dry_run_get_default_fee() {
    init_tracing();

    // A --(public)--> B --(private)--> C
    // Test that dry_run mode works correctly with trampoline routing.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait until A learns B supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node_a, &node_b).await;

    // Create an invoice on C.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .build()
        .expect("build invoice");

    // First, do a dry run to check if the payment can be made
    let dry_run_res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            dry_run: true,
            ..Default::default()
        })
        .await;

    assert!(dry_run_res.is_ok(), "Dry run should succeed");
    let dry_run_response = dry_run_res.unwrap();

    // Verify dry run response contains route information
    assert_eq!(dry_run_response.status, PaymentStatus::Created);

    debug!("Dry run response: {:?}", dry_run_response);

    assert!(
        !dry_run_response.routers.is_empty(),
        "Dry run should return route information"
    );

    // For trampoline routing, fee should be calculated correctly
    // The fee includes both trampoline service fee and routing fee
    let dry_run_fee = dry_run_response.fee;
    debug!("Dry run fee: {}", dry_run_fee);
    assert!(dry_run_fee > 0, "Fee should be greater than 0");

    // Verify that no payment session was created for dry run
    // (We don't need to check the payment_hash since dry_run doesn't persist)

    // Now do the actual payment using the dry_run fee as max_fee_amount
    // The dry_run fee should be sufficient for the actual payment
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let actual_res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(dry_run_fee),
            trampoline_hops: Some(vec![TrampolineHop::new(node_b.get_public_key())]),
            dry_run: false,
            ..Default::default()
        })
        .await;

    assert!(actual_res.is_ok(), "Actual payment should succeed");
    let actual_payment_hash = actual_res.unwrap().payment_hash;

    node_a.wait_until_success(actual_payment_hash).await;

    // Verify the actual payment was processed successfully
    let final_payment = node_a.get_payment_result(actual_payment_hash).await;
    debug!("Final payment result: {:?}", final_payment);
    assert_eq!(final_payment.status, PaymentStatus::Success);
    assert_eq!(final_payment.fee, dry_run_fee);
}

#[tokio::test]
async fn test_trampoline_mpp_with_oneway() {
    init_tracing();

    let amount_1000_ckb = 1000 * 100_000_000;
    let amount_5000_ckb = 5000 * 100_000_000;

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            // fiber1 -> fiber2: 1000 CKB, one-way, private
            (
                (0, 1),
                ChannelParameters {
                    public: false,
                    one_way: true,
                    node_a_funding_amount: amount_1000_ckb,
                    node_b_funding_amount: amount_1000_ckb,
                    ..Default::default()
                },
            ),
            // fiber1 -> fiber2: 1000 CKB, public
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: amount_1000_ckb,
                    node_b_funding_amount: amount_1000_ckb,
                    ..Default::default()
                },
            ),
            // fiber2 -> fiber3: 1000 CKB, private
            (
                (1, 2),
                ChannelParameters {
                    public: false,
                    node_a_funding_amount: amount_1000_ckb,
                    node_b_funding_amount: amount_1000_ckb,
                    ..Default::default()
                },
            ),
            // fiber2 -> fiber4: 1000 CKB, public
            (
                (1, 3),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: amount_1000_ckb,
                    node_b_funding_amount: amount_1000_ckb,
                    ..Default::default()
                },
            ),
            // fiber2 -> fiber3: 1000 CKB, public
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: amount_1000_ckb,
                    node_b_funding_amount: amount_1000_ckb,
                    ..Default::default()
                },
            ),
            // fiber3 -> fiber4: 1000 CKB, public
            (
                (2, 3),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: amount_1000_ckb,
                    node_b_funding_amount: amount_1000_ckb,
                    ..Default::default()
                },
            ),
            // fiber1 -> fiber4: 5000 CKB, public
            (
                (0, 3),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: amount_5000_ckb,
                    node_b_funding_amount: amount_5000_ckb,
                    ..Default::default()
                },
            ),
        ],
        4,
        None,
    )
    .await;

    let [node1, node2, _node3, node4] = nodes.try_into().expect("4 nodes");

    // Wait until node1 learns node2 supports trampoline routing.
    wait_until_node_supports_trampoline_routing(&node1, &node2).await;

    async fn run_with_hash_algo(
        node1: &NetworkNode,
        node2: &NetworkNode,
        node4: &NetworkNode,
        hash_algo: HashAlgorithm,
    ) {
        let invoice_amount = 1001 * 10_000_000;
        let preimage = gen_rand_sha256_hash();
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(invoice_amount))
            .payment_preimage(preimage)
            .payment_secret(gen_rand_sha256_hash())
            .hash_algorithm(hash_algo)
            .payee_pub_key(node4.get_public_key().into())
            .allow_mpp(true)
            .allow_trampoline_routing(true)
            .build()
            .expect("build invoice");
        node4.insert_invoice(invoice.clone(), Some(preimage));

        let res = node1
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                trampoline_hops: Some(vec![TrampolineHop::new(node2.get_public_key())]),
                ..Default::default()
            })
            .await;

        assert!(
            res.is_ok(),
            "Payment should be initiated successfully: {:?}",
            res.err()
        );
        let payment_hash = res.unwrap().payment_hash;
        node1.wait_until_success(payment_hash).await;
    }

    run_with_hash_algo(&node1, &node2, &node4, HashAlgorithm::CkbHash).await;
    run_with_hash_algo(&node1, &node2, &node4, HashAlgorithm::Sha256).await;
}
