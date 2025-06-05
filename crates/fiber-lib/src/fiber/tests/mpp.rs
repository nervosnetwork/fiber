use crate::{
    fiber::{
        network::SendPaymentCommand,
        tests::test_utils::{
            create_n_nodes_network, establish_channel_between_nodes, init_tracing,
            ChannelParameters, NetworkNode, MIN_RESERVED_CKB,
        },
    },
    gen_rand_sha256_hash,
    invoice::{Currency, InvoiceBuilder},
};

#[tokio::test]
async fn test_send_mpp() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let res = source_node
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            amount: ckb_invoice.amount,
            max_parts: Some(2),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    eprintln!("begin to wait for payment: {} success ...", payment_hash);
    source_node.wait_until_success(payment_hash).await;

    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts().len());

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_send_mpp_amount_split() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(15000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let res = source_node
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            amount: ckb_invoice.amount,
            max_parts: Some(2),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    let pss = source_node.get_payment_session(payment_hash).unwrap();
    dbg!(&pss.status, &pss.attempts().len());

    // Spent the half of amount in the first channel
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 2500000000);
    assert_eq!(node_1_balance, 7500000000);

    // Spent the half of amount in the second channel
    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 2500000000);
    assert_eq!(node_1_balance, 7500000000);
}

#[ignore]
#[tokio::test]
async fn test_send_mpp_fee_rate() {
    init_tracing();
    let [mut node_0, mut node_1, mut node_2] = NetworkNode::new_n_interconnected_nodes().await;

    let (_new_channel_id, funding_tx_hash_0) = establish_channel_between_nodes(
        &mut node_0,
        &mut node_1,
        ChannelParameters {
            public: true,
            node_a_funding_amount: MIN_RESERVED_CKB + 1_000_000_000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            a_tlc_fee_proportional_millionths: Some(1_000_000),
            b_tlc_fee_proportional_millionths: Some(2_000_000),
            ..Default::default()
        },
    )
    .await;
    let funding_tx_0 = node_0
        .get_transaction_view_from_hash(funding_tx_hash_0)
        .await
        .expect("get funding tx");
    node_2.submit_tx(funding_tx_0).await;

    let (_new_channel_id, funding_tx_hash_1) = establish_channel_between_nodes(
        &mut node_1,
        &mut node_2,
        ChannelParameters {
            public: true,
            node_a_funding_amount: MIN_RESERVED_CKB + 1_000_000_000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            a_tlc_fee_proportional_millionths: Some(3_000_000),
            b_tlc_fee_proportional_millionths: Some(4_000_000),
            ..Default::default()
        },
    )
    .await;
    let funding_tx_1 = node_1
        .get_transaction_view_from_hash(funding_tx_hash_1)
        .await
        .expect("get funding tx");
    node_0.submit_tx(funding_tx_1).await;

    let (_new_channel_id, funding_tx_hash_2) = establish_channel_between_nodes(
        &mut node_0,
        &mut node_2,
        ChannelParameters {
            public: true,
            node_a_funding_amount: MIN_RESERVED_CKB + 1_000_000_000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            a_tlc_fee_proportional_millionths: Some(3_000_000),
            b_tlc_fee_proportional_millionths: Some(4_000_000),
            ..Default::default()
        },
    )
    .await;
    let funding_tx_2 = node_0
        .get_transaction_view_from_hash(funding_tx_hash_2)
        .await
        .expect("get funding tx");
    node_0.submit_tx(funding_tx_2).await;

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // pay to invoice
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1_500_000_000))
        .payment_preimage(preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build()
        .expect("build invoice success");

    node_2.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            amount: ckb_invoice.amount,
            max_parts: Some(2),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    // assert!(res.fee > 0);
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    assert!(res.fee > 0);
    let nodes = res.router.nodes;
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[2].amount, 187500000);
    assert_eq!(nodes[1].amount, 187500000);
    // The fee is 10_000_000 * 3_000_000 (fee rate) / 1_000_000 = 30_000_000
    assert_eq!(nodes[0].amount, 750000000);

    // let res = node_2.send_payment_keysend(&node_0, 1_000_000, false).await;
    // assert!(res.is_ok(), "Send payment failed: {:?}", res);
    // let res = res.unwrap();
    // assert!(res.fee > 0);
    // let nodes = res.router.nodes;
    // assert_eq!(nodes.len(), 3);
    // assert_eq!(nodes[2].amount, 1_000_000);
    // assert_eq!(nodes[1].amount, 1_000_000);
    // // The fee is 1_000_000 * 2_000_000 (fee rate) / 1_000_000 = 2_000_000
    // assert_eq!(nodes[0].amount, 3_000_000);

    // let payment_hash = res.payment_hash;
    // node_2.wait_until_success(payment_hash).await;
}
