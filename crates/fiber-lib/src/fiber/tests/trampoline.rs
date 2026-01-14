#![cfg(not(target_arch = "wasm32"))]
use crate::fiber::channel::PrevTlcInfo; // Fixed import
use crate::fiber::config::{DEFAULT_FINAL_TLC_EXPIRY_DELTA, DEFAULT_TLC_EXPIRY_DELTA};
use crate::fiber::features::FeatureVector;
use crate::fiber::graph::*;
use crate::fiber::hash_algorithm::HashAlgorithm; // Fixed import
use crate::fiber::network::{
    NetworkActorCommand,
    NetworkActorMessage, // Fixed import
    SendOnionPacketCommand,
};
use crate::fiber::payment::{SendPaymentCommand, TrampolineHop};
use crate::fiber::types::{
    CurrentPaymentHopData,
    PeeledPaymentOnionPacket,
    Privkey,
    Pubkey,
    TlcErrorCode, // Fixed import
    TrampolineHopPayload,
    TrampolineOnionPacket,
};
use crate::invoice::{Currency, InvoiceBuilder};
use crate::tests::test_utils::*;
use crate::{
    create_channel_with_nodes, gen_rand_sha256_hash, ChannelParameters, HUGE_CKB_AMOUNT,
    MIN_RESERVED_CKB,
};
use ractor::RpcReplyPort;
use secp256k1::Secp256k1;
use tokio::sync::oneshot;

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
    let (nodes, _channel_ids) = create_n_nodes_network_with_visibility(
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
            trampoline_hops: Some(vec![TrampolineHop::new(node_t5.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    node_a.wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_trampoline_routing_four_hops_with_public_paths_between_trampolines() {
    init_tracing();

    // A --(public)--> T1 --(public)--> X --(public)--> T2 --(private)--> T3 --(public)--> Y --(public)--> T4 --(private)--> C
    // The forward hops between trampolines can be routed over public multi-hop paths (T1->T2 and T3->T4).
    // The last hop to the recipient is private, so A cannot build a direct route from the gossip graph.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
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

    let [node_a, node_t1, _node_x, node_t2, node_t3, _node_y, node_t4, node_c] =
        nodes.try_into().expect("8 nodes");

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
            trampoline_hops: Some(vec![
                TrampolineHop::new(node_t2.get_public_key()),
                TrampolineHop::new(node_t4.get_public_key()),
            ]),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_a.wait_until_success(res.unwrap().payment_hash).await;

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
            trampoline_hops: Some(vec![TrampolineHop::new(node_t4.get_public_key())]),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(1000),
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
