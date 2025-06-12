use crate::{
    fiber::{config::PAYMENT_MAX_PARTS_LIMIT, network::SendPaymentCommand},
    gen_rand_sha256_hash,
    invoice::{Currency, InvoiceBuilder},
    test_utils::{
        create_n_nodes_network, establish_channel_between_nodes, init_tracing, ChannelParameters,
        NetworkNode, MIN_RESERVED_CKB,
    },
};

#[tokio::test]
async fn test_send_mpp_basic_two_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let res = node_0
        .send_mpp_payment(&mut node_1, 20000000000, Some(2))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    eprintln!("begin to wait for payment: {} success ...", payment_hash);
    node_0.wait_until_success(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts().len());

    let node_0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);

    let node_0_balance = node_0.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_send_mpp_will_not_enabled_if_not_set_allow_mpp() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
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
        .payment_secret(gen_rand_sha256_hash())
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
    assert!(res.is_err(), "should fail because allow_mpp is not set");
    // no path found since mpp is not enabled
    assert!(res.unwrap_err().contains("no path found"));
}

#[tokio::test]
async fn test_send_mpp_without_payment_secret_will_fail() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
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
        .allow_mpp(true)
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
    assert!(
        res.is_err(),
        "should fail because payment secret is missing"
    );
    assert!(res
        .unwrap_err()
        .contains("payment secret is required for multi-path payment"));
}

#[tokio::test]
async fn test_send_mpp_with_invalid_max_parts_will_fail() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0
        .send_mpp_payment(&mut node_1, 10000000000, Some(0))
        .await;
    assert!(res.is_err(), "should fail because max_parts is 0");

    let res = node_0
        .send_mpp_payment(&mut node_1, 10000000000, Some(1))
        .await;
    assert!(res.is_err(), "should fail because max_parts is 1");

    let res = node_0
        .send_mpp_payment(&mut node_1, 10000000000, Some(PAYMENT_MAX_PARTS_LIMIT + 1))
        .await;
    assert!(
        res.is_err(),
        "should fail because max_parts is greater than limit"
    );
    assert!(res
        .unwrap_err()
        .contains("invalid max_parts, value should be in range"));

    let res = node_0
        .send_mpp_payment(&mut node_1, 10000000000, Some(PAYMENT_MAX_PARTS_LIMIT))
        .await;
    assert!(res.is_ok(), "should succeed with max_parts equal to limit");
}

#[tokio::test]
async fn test_send_mpp_amount_choose_single_path() {
    init_tracing();

    // we enable basic mpp, but the amount is small enough to choose a single path
    // so the mpp will not be used actually
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0.send_mpp_payment(&mut node_1, 1000, Some(3)).await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts().len());
    assert_eq!(payment_session.attempts().len(), 1);
    assert_eq!(payment_session.retry_times(), 1);
}

#[tokio::test]
async fn test_send_mpp_amount_3_splits() {
    init_tracing();

    // FIXME: our path-finding algorithm should handle this case better
    // currently, it will try to split the payment into 3 parts, but it will fail
    // we should split the payment into 3 parts with equally amount
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0
        .send_mpp_payment(&mut node_1, 30000000000, Some(3))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(
        &payment_session.status,
        &payment_session.attempts().len(),
        &payment_session.retry_times()
    );
    assert_eq!(payment_session.attempts().len(), 3);
    assert_eq!(payment_session.retry_times(), 3);
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
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let res = node_0
        .send_mpp_payment(&mut node_1, 15000000000, Some(2))
        .await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts().len());

    // Spent the half of amount in the first channel
    let node_0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 5000000000);
    assert_eq!(node_1_balance, 5000000000);

    // Spent the half of amount in the second channel
    let node_0_balance = node_0.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_send_mpp_amount_split_with_more_channels() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, mut node_2] = nodes.try_into().expect("2 nodes");

    let res = node_0.send_mpp_payment(&mut node_2, 100000, Some(2)).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_send_mpp_amount_split_with_last_channels() {
    init_tracing();

    async fn test_with_params(amount: u128, max_parts: Option<u64>, expect_status: &str) {
        let (nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (MIN_RESERVED_CKB + 400000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ],
            3,
        )
        .await;
        let [node_0, _node_1, mut node_2] = nodes.try_into().expect("2 nodes");

        let res = node_0
            .send_mpp_payment(&mut node_2, amount, max_parts)
            .await;
        if expect_status == "success" {
            node_0.wait_until_success(res.unwrap().payment_hash).await;
        } else {
            node_0.wait_until_failed(res.unwrap().payment_hash).await;
        }
    }

    test_with_params(300000, Some(2), "fail").await;
    test_with_params(300000, Some(3), "fail").await;
    test_with_params(290000, Some(3), "success").await;
    test_with_params(290000, None, "success").await;
}

#[tokio::test]
async fn test_send_mpp_amount_split_with_one_extra_direct_channel() {
    init_tracing();

    async fn test_with_params(
        amount: u128,
        max_parts: Option<u64>,
        expect_status: &str,
        expect_attempts_count: usize,
    ) {
        let (nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (MIN_RESERVED_CKB + 400000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((0, 2), (MIN_RESERVED_CKB + 400000, MIN_RESERVED_CKB)),
            ],
            3,
        )
        .await;
        let [node_0, _node_1, mut node_2] = nodes.try_into().expect("2 nodes");

        let res = node_0
            .send_mpp_payment(&mut node_2, amount, max_parts)
            .await;
        let payment_hash = res.unwrap().payment_hash;
        if expect_status == "success" {
            node_0.wait_until_success(payment_hash).await;
        } else {
            node_0.wait_until_failed(payment_hash).await;
        }
        let payment_session = node_0.get_payment_session(payment_hash).unwrap();
        assert_eq!(payment_session.attempts().len(), expect_attempts_count);
    }

    test_with_params(400000, Some(2), "success", 1).await;
    test_with_params(300000, Some(2), "success", 1).await;
    // need to split into 2 parts
    test_with_params(400001, Some(2), "success", 2).await;
    test_with_params(800000, None, "fail", 5).await;
    test_with_params(700000 - 5000, Some(4), "success", 4).await;
}

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
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
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

    let payment_hash = res.payment_hash;
    // fee_rate is too high
    node_0.wait_until_failed(payment_hash).await;
}
