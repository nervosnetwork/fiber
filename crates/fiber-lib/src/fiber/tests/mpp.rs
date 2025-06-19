use secp256k1::Secp256k1;

use crate::{
    fiber::{
        channel::{
            AddTlcCommand, ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, TLCId,
        },
        config::{DEFAULT_HOLD_TLC_TIMEOUT, DEFAULT_TLC_EXPIRY_DELTA, PAYMENT_MAX_PARTS_LIMIT},
        hash_algorithm::HashAlgorithm,
        network::SendPaymentCommand,
        types::{Hash256, PaymentDataRecord, PaymentHopData, PeeledOnionPacket, RemoveTlcReason},
        NetworkActorCommand, NetworkActorMessage, PaymentCustomRecords,
    },
    gen_rand_sha256_hash,
    invoice::{Currency, InvoiceBuilder},
    now_timestamp_as_millis_u64,
    test_utils::{
        create_n_nodes_network, establish_channel_between_nodes, init_tracing, ChannelParameters,
        NetworkNode, MIN_RESERVED_CKB,
    },
    HUGE_CKB_AMOUNT,
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
    dbg!(&payment_session.status, &payment_session.attempts_count());

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

    let res = node_0.send_mpp_payment(&mut node_1, 10000, Some(3)).await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts_count());
    assert_eq!(payment_session.attempts_count(), 1);
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
        &payment_session.attempts_count(),
        &payment_session.retry_times()
    );
    assert_eq!(payment_session.attempts_count(), 3);
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
    dbg!(&payment_session.status, &payment_session.attempts_count());

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
        if expect_status == "build_error" {
            assert!(res.is_err(), "should fail to build payment");
            return;
        }
        let payment_hash = res.unwrap().payment_hash;
        if expect_status == "success" {
            node_0.wait_until_success(payment_hash).await;
        } else {
            node_0.wait_until_failed(payment_hash).await;
        }
        let payment_session = node_0.get_payment_session(payment_hash).unwrap();
        assert_eq!(payment_session.attempts_count(), expect_attempts_count);
    }

    test_with_params(400000, Some(2), "success", 1).await;
    test_with_params(300000, Some(2), "success", 1).await;
    // need to split into 2 parts
    test_with_params(400001, Some(2), "success", 2).await;
    test_with_params(800000, None, "build_error", 5).await;
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

#[tokio::test]
async fn test_mpp_tlc_set() {
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
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    let record = PaymentDataRecord::new(payment_secret, 20000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
    ];

    let packet = PeeledOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create peeled packet");

    let _add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    let _add_tlc_result_2 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_mpp_tlc_set_total_amount_mismatch() {
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
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    // the total amount should be 20000000000, but we set 10000000000 here
    let record = PaymentDataRecord::new(payment_secret, 10000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
    ];

    let packet = PeeledOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    let add_tlc_result_2 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // check offered tlcs should be fail
    let tlc1 = source_node.get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id));
    let tlc2 = source_node.get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id));
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // check received tlcs should be fail
    let tlc1 = node_1.get_tlc(channels[0], TLCId::Received(add_tlc_result_1.tlc_id));
    let tlc2 = node_1.get_tlc(channels[1], TLCId::Received(add_tlc_result_2.tlc_id));
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // balance should not change
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_mpp_tlc_set_payment_secret_mismatch() {
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
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    // set the payment secret to a random value
    let record = PaymentDataRecord::new(gen_rand_sha256_hash(), 20000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
    ];

    let packet = PeeledOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    let add_tlc_result_2 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // check offered tlcs should be fail
    let tlc1 = source_node.get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id));
    let tlc2 = source_node.get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id));
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // check received tlcs should be fail
    let tlc1 = node_1.get_tlc(channels[0], TLCId::Received(add_tlc_result_1.tlc_id));
    let tlc2 = node_1.get_tlc(channels[1], TLCId::Received(add_tlc_result_2.tlc_id));
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // balance should not change
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_mpp_tlc_set_timeout() {
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
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    let record = PaymentDataRecord::new(payment_secret, 20000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
    ];

    let packet = PeeledOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    // wait until tlc is hold
    while node_1.store.get_hold_tlcs(payment_hash).is_empty() {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // sleep enough time to timeout hold tlc
    tokio::time::sleep(tokio::time::Duration::from_millis(
        DEFAULT_HOLD_TLC_TIMEOUT + 500,
    ))
    .await;

    // check channels
    ractor::cast!(
        source_node.network_actor,
        NetworkActorMessage::Command(NetworkActorCommand::CheckChannels)
    )
    .expect("node alive");

    // ensure check channels is done
    tokio::time::sleep(tokio::time::Duration::from_millis(4000)).await;

    let add_tlc_result_2 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // check offered tlcs should be fail
    let tlc1 = source_node.get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id));
    let tlc2 = source_node.get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id));
    // tlc 1 is timeout
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    // tlc 2 is still in hold
    assert!(tlc2.unwrap().removed_reason.is_none());

    // check received tlcs should be fail
    let tlc1 = node_1.get_tlc(channels[0], TLCId::Received(add_tlc_result_1.tlc_id));
    let tlc2 = node_1.get_tlc(channels[1], TLCId::Received(add_tlc_result_2.tlc_id));
    // tlc 1 is timeout
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    // tlc 2 is still in hold
    assert!(tlc2.unwrap().removed_reason.is_none());

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_mpp_tlc_set_without_payment_data() {
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
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    // We leave payment_data in the custom_records as none
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
    ];

    let packet = PeeledOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    let add_tlc_result_2 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // check offered tlcs should be fail
    let tlc1 = source_node.get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id));
    let tlc2 = source_node.get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id));
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // check received tlcs should be fail
    let tlc1 = node_1.get_tlc(channels[0], TLCId::Received(add_tlc_result_1.tlc_id));
    let tlc2 = node_1.get_tlc(channels[1], TLCId::Received(add_tlc_result_2.tlc_id));
    assert!(matches!(
        tlc1.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // balance should not change
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_send_mpp_dry_run_will_be_ok_with_single_path() {
    init_tracing();

    async fn test_dryrun_with_network(
        amount: u128,
        expect_routers_count: Option<usize>,
        expected_fee: Option<u128>,
    ) {
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
        let [node_0, _node_1, mut node_2] = nodes.try_into().expect("ok nodes");
        let res = node_0
            .send_mpp_payment_with_dry_run_option(&mut node_2, amount, None, true)
            .await;

        if let Some(count) = expect_routers_count {
            assert!(res.is_ok(), "Send payment failed: {:?}", res);
            let query_res = res.unwrap();
            assert_eq!(query_res.routers.len(), count);
            let fee = query_res.fee;
            let total_amount: u128 = query_res.routers.iter().map(|r| r.receiver_amount()).sum();
            assert_eq!(total_amount, amount,);
            if let Some(expect_fee) = expected_fee {
                assert_eq!(fee, expect_fee);
            }
        } else {
            assert!(res.is_err());
        }
    }

    // too small
    test_dryrun_with_network(5000, None, None).await;
    test_dryrun_with_network(300000, None, None).await;
    test_dryrun_with_network(300000 - 200, None, None).await;
    test_dryrun_with_network(300000 - 300, Some(3), Some(300)).await;
}

#[tokio::test]
async fn test_send_mpp_direct_channels_dry_run() {
    init_tracing();

    async fn test_dryrun_with_network(amount: u128, expect_routers_count: Option<usize>) {
        let (nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ],
            3,
        )
        .await;
        let [node_0, mut node_1, _node_2] = nodes.try_into().expect("ok nodes");

        let res = node_0
            .send_mpp_payment_with_dry_run_option(&mut node_1, amount, None, true)
            .await;

        if let Some(count) = expect_routers_count {
            assert!(res.is_ok(), "Send payment failed: {:?}", res);
            let query_res = res.unwrap();
            assert_eq!(query_res.routers.len(), count);
            let fee = query_res.fee;
            assert_eq!(fee, 0);
        } else {
            assert!(res.is_err());
        }
    }

    test_dryrun_with_network(100000 - 1, Some(1)).await;
    test_dryrun_with_network(100000, Some(1)).await;
    test_dryrun_with_network(100001, Some(2)).await;
    test_dryrun_with_network(200000, Some(2)).await;
    test_dryrun_with_network(200001, Some(3)).await;
    test_dryrun_with_network(300000, Some(3)).await;
    test_dryrun_with_network(300001, None).await;
}

#[tokio::test]
async fn test_send_mpp_dry_run_single_path_mixed_with_multiple_paths() {
    init_tracing();

    async fn test_dryrun_with_network(
        amount: u128,
        expect_routers_count: Option<usize>,
        expected_fee: Option<u128>,
    ) {
        let (nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (MIN_RESERVED_CKB + 400000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
                ((0, 2), (MIN_RESERVED_CKB + 500000, MIN_RESERVED_CKB)),
            ],
            3,
        )
        .await;
        let [node_0, _node_1, mut node_2] = nodes.try_into().expect("ok nodes");
        let res = node_0
            .send_mpp_payment_with_dry_run_option(&mut node_2, amount, None, true)
            .await;

        if let Some(count) = expect_routers_count {
            assert!(res.is_ok(), "Send payment failed: {:?}", res);
            let query_res = res.unwrap();
            assert_eq!(query_res.routers.len(), count);
            let fee = query_res.fee;
            let total_amount: u128 = query_res.routers.iter().map(|r| r.receiver_amount()).sum();
            assert_eq!(total_amount, amount,);
            if let Some(expect_fee) = expected_fee {
                assert_eq!(fee, expect_fee);
            }
        } else {
            assert!(res.is_err());
        }
    }

    // too small
    test_dryrun_with_network(1000, None, None).await;
    test_dryrun_with_network(500000, Some(1), Some(0)).await;
    test_dryrun_with_network(300000, Some(1), None).await;
    test_dryrun_with_network(500000 + 5, Some(2), Some(10)).await;
    test_dryrun_with_network(600000 + 5, Some(3), Some(101)).await;
    test_dryrun_with_network(700000 + 5, Some(4), Some(201)).await;
    test_dryrun_with_network(800000, None, None).await;
}

#[tokio::test]
async fn test_send_mpp_will_succeed_with_retry_first_hops() {
    init_tracing();

    // we disable a channel stealthy, and then retry the payment,
    // the payment should succeed with rebuild a path successfully
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&mut node_1, 300000, None, true)
        .await;

    let query_res = res.unwrap();
    assert_eq!(query_res.routers.len(), 3);
    dbg!(&query_res.routers);

    let used_channels = node_0
        .routers_used_channels(&query_res.routers, &channels)
        .await;

    dbg!(&used_channels);

    let first_used_channel = used_channels.first().unwrap();
    node_0.disable_channel_stealthy(*first_used_channel).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = node_0
        .send_mpp_payment(&mut node_1, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_res = node_0.get_payment_result(payment_hash).await;
    let used_channels = node_0
        .routers_used_channels(&payment_res.routers, &channels)
        .await;
    assert_eq!(used_channels.len(), 3);
    assert!(
        !used_channels.contains(first_used_channel),
        "First used channel should not be used after retry"
    );
}

#[tokio::test]
async fn test_send_mpp_will_succeed_with_retry_2_channels() {
    init_tracing();

    // we disable two channels stealthy, and then retry the payment,
    // the payment should succeed with rebuild a path successfully
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&mut node_1, 300000, None, true)
        .await;

    let query_res = res.unwrap();
    assert_eq!(query_res.routers.len(), 3);
    dbg!(&query_res.routers);

    let used_channels = node_0
        .routers_used_channels(&query_res.routers, &channels)
        .await;

    dbg!(&used_channels);

    let first_used_channel = used_channels.first().unwrap();
    node_0.disable_channel_stealthy(*first_used_channel).await;
    let last_used_channel = used_channels.first().unwrap();
    node_0.disable_channel_stealthy(*last_used_channel).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = node_0
        .send_mpp_payment(&mut node_1, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_res = node_0.get_payment_result(payment_hash).await;
    let used_channels = node_0
        .routers_used_channels(&payment_res.routers, &channels)
        .await;
    assert_eq!(used_channels.len(), 3);
    assert!(
        !used_channels.contains(first_used_channel),
        "First used channel should not be used after retry"
    );
}

#[tokio::test]
async fn test_send_mpp_will_fail_with_retry_3_channels() {
    init_tracing();

    // we disable 3 channels stealthy, and then retry the payment,
    // the payment should failed with no path found
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&mut node_1, 300000, None, true)
        .await;

    let query_res = res.unwrap();
    assert_eq!(query_res.routers.len(), 3);
    dbg!(&query_res.routers);

    let used_channels = node_0
        .routers_used_channels(&query_res.routers, &channels)
        .await;

    dbg!(&used_channels);

    for channel in used_channels.iter() {
        node_0.disable_channel_stealthy(*channel).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = node_0
        .send_mpp_payment(&mut node_1, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_failed(payment_hash).await;
}

#[tokio::test]
async fn test_send_mpp_will_success_with_retry_split_channels() {
    init_tracing();

    // we disable the first large channel stealthy, and then retry the payment,
    // the payment should success with retry and split the payment into 3 parts
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 300000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&mut node_1, 300000, None, true)
        .await;

    let query_res = res.unwrap();
    assert_eq!(query_res.routers.len(), 1);
    dbg!(&query_res.routers);

    let used_channels = node_0
        .routers_used_channels(&query_res.routers, &channels)
        .await;

    dbg!(&used_channels);
    assert!(used_channels.contains(&channels[0]));

    node_0.disable_channel_stealthy(channels[0]).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = node_0
        .send_mpp_payment(&mut node_1, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_res = node_0.get_payment_result(payment_hash).await;
    assert_eq!(payment_res.routers.len(), 3);
}

#[tokio::test]
async fn test_send_mpp_will_fail_with_disable_single_path() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)), // disable this path will make payment fail
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT + 10010, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT + 10010, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT + 10010, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, mut node_2] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&mut node_2, 30000, None, true)
        .await;

    let query_res = res.unwrap();
    assert_eq!(query_res.routers.len(), 1);
    dbg!(&query_res.routers);

    let used_channels = node_0
        .routers_used_channels(&query_res.routers, &channels)
        .await;

    dbg!(&used_channels);
    assert!(used_channels.contains(&channels[0]));

    node_0.disable_channel_stealthy(channels[0]).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = node_0
        .send_mpp_payment(&mut node_2, 30000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_failed(payment_hash).await;
    let payment_res = node_0.get_payment_result(payment_hash).await;
    assert_eq!(payment_res.routers.len(), 0);
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();

    let attempts = payment_session.attempts().collect::<Vec<_>>();
    assert_eq!(attempts.len(), 1);
    assert_eq!(payment_session.retry_times(), 2);
}

#[tokio::test]
async fn test_send_mpp_will_success_with_middle_hop_capacity_not_enough() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 3, MIN_RESERVED_CKB + 304000)),
            ((1, 2), (MIN_RESERVED_CKB + 100010, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100010, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100010, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, mut node_2] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&mut node_2, 300000, None, true)
        .await;

    let query_res = res.unwrap();
    assert_eq!(query_res.routers.len(), 1);
    dbg!(&query_res.routers);

    let used_channels = node_0
        .routers_used_channels(&query_res.routers, &channels)
        .await;

    dbg!(&used_channels);
    assert!(used_channels.contains(&channels[1]));

    let res = node_0
        .send_mpp_payment(&mut node_2, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    // FIXME(yukang): how to make this payment success?
    node_0.wait_until_failed(payment_hash).await;
}
