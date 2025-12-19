#![cfg(not(target_arch = "wasm32"))]
use crate::fiber::features::FeatureVector;
use crate::fiber::network::SendPaymentCommand;
use crate::invoice::{Currency, InvoiceBuilder};
use crate::tests::test_utils::{create_n_nodes_network_with_visibility, init_tracing};
use crate::{
    create_channel_with_nodes, gen_rand_sha256_hash, ChannelParameters, HUGE_CKB_AMOUNT,
    MIN_RESERVED_CKB,
};

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
    let trampoline_pubkey = node_b.get_public_key();
    for _ in 0..50 {
        let ok = node_a
            .get_network_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id == trampoline_pubkey)
            .is_some_and(|n| n.features.supports_trampoline_routing());
        if ok {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    // ================================================================
    // Create an invoice on C that explicitly allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .allow_trampoline_routing(true)
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(500),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_a.wait_until_success(payment_hash).await;
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
    let trampoline_pubkey = node_b.get_public_key();
    for _ in 0..50 {
        let ok = node_a
            .get_network_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id == trampoline_pubkey)
            .is_some_and(|n| n.features.supports_trampoline_routing());
        if ok {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    // Create an invoice on C that explicitly NOT allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .allow_trampoline_routing(false)
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

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
    // Create an invoice on C that explicitly allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .allow_trampoline_routing(true)
        .build()
        .expect("build invoice");
    node_c.insert_invoice(invoice.clone(), Some(preimage));

    node_a
        .assert_send_payment_success(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;

    // ================================================================
    // Create an invoice on C that explicitly allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.get_public_key().into())
        .allow_trampoline_routing(true)
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
        .allow_trampoline_routing(true)
        .build()
        .expect("build invoice");
    node_f.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
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
        .allow_trampoline_routing(true)
        .build()
        .expect("build invoice");
    node_f.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_b
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    node_b.wait_until_success(res.unwrap().payment_hash).await;
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
