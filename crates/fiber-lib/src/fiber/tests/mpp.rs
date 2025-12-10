use secp256k1::Secp256k1;
use std::{collections::HashMap, time::Duration};
use tracing::debug;

use crate::{
    create_n_nodes_network_with_params,
    fiber::{
        channel::{
            AddTlcCommand, ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, TLCId,
        },
        config::{CKB_SHANNONS, DEFAULT_TLC_EXPIRY_DELTA, PAYMENT_MAX_PARTS_LIMIT},
        features::FeatureVector,
        hash_algorithm::HashAlgorithm,
        network::{DebugEvent, SendPaymentCommand, USER_CUSTOM_RECORDS_MAX_INDEX},
        payment::{AttemptStatus, PaymentStatus},
        types::{
            BasicMppPaymentData, Hash256, PaymentHopData, PeeledPaymentOnionPacket, RemoveTlcReason,
        },
        NetworkActorCommand, NetworkActorMessage, PaymentCustomRecords,
    },
    gen_rand_secp256k1_public_key, gen_rand_sha256_hash, gen_rpc_config,
    invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder},
    now_timestamp_as_millis_u64,
    rpc::invoice::NewInvoiceParams,
    test_utils::{
        create_n_nodes_network, establish_channel_between_nodes, init_tracing, ChannelParameters,
        NetworkNode, MIN_RESERVED_CKB,
    },
    NetworkServiceEvent, HUGE_CKB_AMOUNT,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_basic_two_channels_one_time() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let res = node_0.send_mpp_payment(&node_1, 20000000000, Some(2)).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    let invoice = node_1.get_invoice_status(&payment_hash).unwrap();
    assert_eq!(invoice, CkbInvoiceStatus::Open);

    eprintln!("begin to wait for payment: {} success ...", payment_hash);

    node_0.wait_until_success(payment_hash).await;
    let find_path_count = node_0
        .get_payment_find_path_count(payment_hash)
        .await
        .unwrap();
    eprintln!("find_path_count: {}", find_path_count);
    assert_eq!(find_path_count, 4);

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

    let invoice = node_1.get_invoice_status(&payment_hash).unwrap();
    eprintln!("invoice status: {:?}", invoice);
    assert_eq!(invoice, CkbInvoiceStatus::Paid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [_node_0, node_1] = nodes.try_into().expect("2 nodes");
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .build();

    let error = ckb_invoice.unwrap_err().to_string();
    assert!(
        error.contains("Payment secret is required for MPP payments"),
        "should contain error about payment secret: {}",
        error
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0.send_mpp_payment(&node_1, 10000000000, Some(0)).await;
    assert!(res.is_err(), "should fail because max_parts is 0");

    let res = node_0.send_mpp_payment(&node_1, 10000000000, Some(1)).await;
    assert!(res.is_err(), "should fail because max_parts is 1");

    let res = node_0
        .send_mpp_payment(&node_1, 10000000000, Some(PAYMENT_MAX_PARTS_LIMIT + 1))
        .await;
    assert!(
        res.is_err(),
        "should fail because max_parts is greater than limit"
    );
    assert!(res
        .unwrap_err()
        .contains("invalid max_parts, value should be in range"));

    let res = node_0
        .send_mpp_payment(&node_1, 10000000000, Some(PAYMENT_MAX_PARTS_LIMIT))
        .await;
    assert!(res.is_ok(), "should succeed with max_parts equal to limit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0.send_mpp_payment(&node_1, 10000, Some(3)).await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts_count());
    assert_eq!(payment_session.attempts_count(), 1);
    assert_eq!(payment_session.retry_times(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_amount_3_splits() {
    init_tracing();

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
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0.send_mpp_payment(&node_1, 30000000000, Some(3)).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let res = node_0.send_mpp_payment(&node_1, 15000000000, Some(2)).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, _node_1, node_2] = nodes.try_into().expect("2 nodes");

    let res = node_0.send_mpp_payment(&node_2, 100000, Some(2)).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
        let [node_0, _node_1, node_2] = nodes.try_into().expect("2 nodes");

        let res = node_0.send_mpp_payment(&node_2, amount, max_parts).await;
        if expect_status == "success" {
            node_0.wait_until_success(res.unwrap().payment_hash).await;
        } else {
            assert!(res.is_err(), "should fail to build payment");
        }
    }

    test_with_params(300000, Some(2), "fail").await;
    test_with_params(300000, Some(3), "fail").await;
    test_with_params(290000, Some(3), "success").await;
    test_with_params(290000, None, "success").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
        let [node_0, _node_1, node_2] = nodes.try_into().expect("2 nodes");

        let res = node_0.send_mpp_payment(&node_2, amount, max_parts).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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

    // fee_rate is too high, timeout
    assert!(res.is_err(), "should fail to build payment");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
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
    let record = BasicMppPaymentData::new(payment_secret, 20000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some()
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    // wait tlc 2 is removed
    while source_node
        .get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id))
        .is_some()
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_set_with_insufficient_total_amount() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
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
    // set total amount to 20000000000, but pay only 10000000000
    let record = BasicMppPaymentData::new(payment_secret, 20000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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

    // timeout hold tlc after 3 seconds
    let channel_id = channels[0];
    let tlc_id = add_tlc_result_1.tlc_id;
    node_1
        .network_actor
        .send_after(Duration::from_secs(3), move || {
            NetworkActorMessage::Command(NetworkActorCommand::TimeoutHoldTlc(
                payment_hash,
                channel_id,
                tlc_id,
            ))
        });

    // because tlc is not fulfilled, it should be removed after 5 seconds instead of settling
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_none()
    {
        // wait for tlc to be added
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some_and(|tlc| tlc.removed_reason.is_none())
    {
        // wait for tlc to be removed
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // tlc should be removed after 5 seconds
    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;

    debug!("now tlc removed reason: {:?}", tlc_result);
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // balance should not change
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_set_with_only_1_tlc() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
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
    let record = BasicMppPaymentData::new(payment_secret, 10000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some()
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_set_with_only_1_tlc_without_payment_data() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
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

    let packet = PeeledPaymentOnionPacket::create(
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some()
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
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
    let record = BasicMppPaymentData::new(payment_secret, 10000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_set_total_amount_should_be_consistent() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
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

    // Tlc 1 is set to 20000000000, but tlc2 is set to 20000000001
    // both should valid since there are greater than invoice amount
    // but payment will fail because the total_amount is inconsistent
    // tlc1 records
    let mut custom_records_1 = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(payment_secret, 20000000000);
    record.write(&mut custom_records_1);

    // tlc2 records
    let mut custom_records_2 = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(payment_secret, 20000000001);
    record.write(&mut custom_records_2);

    let build_packet = |custom_records: PaymentCustomRecords| {
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

        PeeledPaymentOnionPacket::create(
            source_node.get_private_key().clone(),
            hops_infos.clone(),
            Some(payment_hash.as_ref().to_vec()),
            &secp,
        )
        .expect("create peeled packet")
    };

    let packet_1 = build_packet(custom_records_1);
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
                        onion_packet: packet_1.next.clone(),
                        shared_secret: packet_1.shared_secret,
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

    let packet_2 = build_packet(custom_records_2);
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
                        onion_packet: packet_2.next.clone(),
                        shared_secret: packet_2.shared_secret,
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

    // wait tlc 1 is fail
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some_and(|tlc| tlc.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    // wait tlc 2 is fail
    while source_node
        .get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id))
        .is_some_and(|tlc| tlc.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
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
    let record = BasicMppPaymentData::new(gen_rand_sha256_hash(), 20000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_set_timeout_1_of_2() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();

    // notice we request 30000000000, but send 20000000000
    // so the tlc 1 will be timeout, but tlc 2 will be hold
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(30000000000))
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
    let record = BasicMppPaymentData::new(payment_secret, 30000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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
    while node_1.store.get_payment_hold_tlcs(payment_hash).is_empty() {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

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

    // wait tlc2 is hold
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // timeout tlc 1, but not 2
    for hold_tlc in node_1.store.get_payment_hold_tlcs(payment_hash) {
        if hold_tlc.channel_id == channels[0] && hold_tlc.tlc_id == add_tlc_result_1.tlc_id {
            ractor::cast!(
                node_1.network_actor,
                NetworkActorMessage::Command(NetworkActorCommand::TimeoutHoldTlc(
                    payment_hash,
                    hold_tlc.channel_id,
                    hold_tlc.tlc_id,
                ))
            )
            .expect("node alive");
        }
    }

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

    // timeout tlc2
    for hold_tlc in node_1.store.get_payment_hold_tlcs(payment_hash) {
        if hold_tlc.channel_id == channels[1] && hold_tlc.tlc_id == add_tlc_result_2.tlc_id {
            ractor::cast!(
                node_1.network_actor,
                NetworkActorMessage::Command(NetworkActorCommand::TimeoutHoldTlc(
                    payment_hash,
                    hold_tlc.channel_id,
                    hold_tlc.tlc_id,
                ))
            )
            .expect("node alive");
        }
    }

    // wait until tlc 2 is timeout
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // check channels again
    ractor::cast!(
        node_1.network_actor,
        NetworkActorMessage::Command(NetworkActorCommand::CheckChannels)
    )
    .expect("node alive");

    // ensure check channels is done
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // check tlc 2
    let tlc2 = source_node.get_tlc(channels[1], TLCId::Offered(add_tlc_result_2.tlc_id));
    // tlc 2 is timeout
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // check received tlcs should be fail
    let tlc2 = node_1.get_tlc(channels[1], TLCId::Received(add_tlc_result_2.tlc_id));
    // tlc 2 is timeout
    assert!(matches!(
        tlc2.unwrap().removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
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
    let record = BasicMppPaymentData::new(payment_secret, 20000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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
    while node_1.store.get_payment_hold_tlcs(payment_hash).is_empty() {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // timeout hold tlc
    for hold_tlc in node_1.store.get_payment_hold_tlcs(payment_hash) {
        ractor::cast!(
            node_1.network_actor,
            NetworkActorMessage::Command(NetworkActorCommand::TimeoutHoldTlc(
                payment_hash,
                hold_tlc.channel_id,
                hold_tlc.tlc_id,
            ))
        )
        .expect("node alive");
    }

    // sleep enough time to timeout hold tlc
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // check channels
    ractor::cast!(
        node_1.network_actor,
        NetworkActorMessage::Command(NetworkActorCommand::CheckChannels)
    )
    .expect("node alive");

    // ensure check channels is done
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &node_0;
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

    let packet = PeeledPaymentOnionPacket::create(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
        let [node_0, _node_1, node_2] = nodes.try_into().expect("ok nodes");
        let res = node_0
            .send_mpp_payment_with_dry_run_option(&node_2, amount, None, true)
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
    test_dryrun_with_network(0, None, None).await;
    test_dryrun_with_network(300000, None, None).await;
    test_dryrun_with_network(300000 - 200, Some(4), Some(301)).await;
    test_dryrun_with_network(300000 - 300, Some(3), Some(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
        let [node_0, node_1, _node_2] = nodes.try_into().expect("ok nodes");

        let res = node_0
            .send_mpp_payment_with_dry_run_option(&node_1, amount, None, true)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
        let [node_0, _node_1, node_2] = nodes.try_into().expect("ok nodes");
        let res = node_0
            .send_mpp_payment_with_dry_run_option(&node_2, amount, None, true)
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
            debug!("send payment: {:?} failed: {:?}", amount, res);
            assert!(res.is_err());
        }
    }

    // too small
    test_dryrun_with_network(0, None, None).await;
    test_dryrun_with_network(1000, Some(1), Some(0)).await;
    test_dryrun_with_network(500000, Some(1), Some(0)).await;
    test_dryrun_with_network(300000, Some(1), None).await;
    test_dryrun_with_network(500000 + 5, Some(2), Some(1)).await;
    test_dryrun_with_network(600000 + 5, Some(3), Some(101)).await;
    test_dryrun_with_network(700000 + 5, Some(4), Some(201)).await;
    test_dryrun_with_network(800000, None, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_1, 300000, None, true)
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
        .send_mpp_payment(&node_1, 300000, None)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_1, 300000, None, true)
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
        .send_mpp_payment(&node_1, 300000, None)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_1, 300000, None, true)
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
        .send_mpp_payment(&node_1, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_failed(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, node_1] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_1, 300000, None, true)
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
        .send_mpp_payment(&node_1, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let payment_res = node_0.get_payment_result(payment_hash).await;
    assert_eq!(payment_res.routers.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    let [node_0, _node_1, node_2] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_2, 30000, None, true)
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
        .send_mpp_payment(&node_2, 30000, None)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_will_success_with_middle_hop_capacity_not_enough() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 3, MIN_RESERVED_CKB + 304000)),
            ((1, 2), (MIN_RESERVED_CKB + 103000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 103000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 103000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("ok nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_2, 300000, None, true)
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
        .send_mpp_payment(&node_2, 300000, None)
        .await
        .expect("send mpp payment");

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    let attempts = payment_session.attempts().collect::<Vec<_>>();
    assert!(!attempts.iter().any(|a| a
        .last_error
        .clone()
        .unwrap_or_default()
        .contains("HoldTlcTimeout")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_will_success_with_same_payment_after_restarted() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 103000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 103000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 103000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, mut node_1, mut node_2] = nodes.try_into().expect("ok nodes");

    let target_node = &mut node_2;
    let amount = 300000;
    let target_pubkey = target_node.get_public_key();
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build()
        .expect("build invoice success");

    target_node.insert_invoice(ckb_invoice.clone(), Some(preimage));

    node_1.stop().await;
    debug!("node_1 stopped");
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            amount: ckb_invoice.amount,
            ..Default::default()
        })
        .await;

    if let Ok(res) = res {
        assert_eq!(res.routers.len(), 3);
        debug!("begin to wait for payment failure: {:?}", res.payment_hash);
        node_0.wait_until_failed(res.payment_hash).await;
        debug!("node_0 payment failed, res: {:?}", res);
    } else {
        // send payment failed, which may happen
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    // restart node_1
    node_1.start().await;
    debug!("node_1 restarted");
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    // the remove_tlc may come after the node_1 restarted,
    // this may comes from the background task of node_1
    // so we need to clear the history of node_0
    node_0.clear_history().await;
    // retry the payment
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            amount: ckb_invoice.amount,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let res = res.unwrap();
    debug!("retry payment success: {:?}", res.payment_hash);
    node_0.wait_until_success(res.payment_hash).await;
    assert_eq!(res.routers.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_set_without_invoice_should_not_be_accepted() {
    init_tracing();

    async fn inner_test_with_payment_hash_and_preimage(preimage: Hash256, payment_hash: Hash256) {
        // we send a tlc with payment data, but without invoice (keysend)
        // the node is expected to not settle the tlc
        let (nodes, channels) = create_n_nodes_network(
            &[
                ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
                ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ],
            2,
        )
        .await;
        let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
        let source_node = &mut node_0;
        let target_pubkey = node_1.pubkey;

        let payment_secret = gen_rand_sha256_hash();
        let hash_algorithm = HashAlgorithm::CkbHash;

        let secp = Secp256k1::new();
        let mut custom_records = PaymentCustomRecords::default();
        let record = BasicMppPaymentData::new(payment_secret, 20000000000);
        record.write(&mut custom_records);
        let hops_infos = vec![
            PaymentHopData {
                amount: 10000000000,
                expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                next_hop: Some(target_pubkey),
                funding_tx_hash: Hash256::default(),
                hash_algorithm,
                payment_preimage: Some(preimage),
                custom_records: Some(custom_records.clone()),
            },
            PaymentHopData {
                amount: 10000000000,
                expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                next_hop: None,
                funding_tx_hash: Hash256::default(),
                hash_algorithm,
                payment_preimage: Some(preimage),
                custom_records: Some(custom_records.clone()),
            },
        ];

        let packet = PeeledPaymentOnionPacket::create(
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

        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        // wait tlc 1 is removed
        while source_node
            .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
            .is_some_and(|tlc| tlc.removed_reason.is_none())
        {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        let tlc = source_node
            .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
            .expect("tlc");
        assert!(matches!(
            tlc.removed_reason,
            Some(RemoveTlcReason::RemoveTlcFail(..))
        ));

        let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
        let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
        assert_eq!(node_0_balance, 10000000000);
        assert_eq!(node_1_balance, 0);
    }

    // incorrect payment hash
    inner_test_with_payment_hash_and_preimage(gen_rand_sha256_hash(), gen_rand_sha256_hash()).await;

    // paired preimage and payment hash
    let preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
    inner_test_with_payment_hash_and_preimage(preimage, payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_with_invoice_removed_from_last_hop() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, mut node_2] = nodes.try_into().expect("ok nodes");

    let target_node = &mut node_2;
    let amount = 300000;
    let target_pubkey = target_node.get_public_key();
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build()
        .expect("build invoice success");

    //target_node.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            amount: ckb_invoice.amount,
            ..Default::default()
        })
        .await
        .unwrap();

    node_0.wait_until_failed(res.payment_hash).await;
    debug!("node_0 payment failed, res: {:?}", res);

    assert_eq!(res.routers.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_tlc_with_invoice_not_allow_mpp_should_not_be_accepted() {
    init_tracing();

    // we send a tlc with payment data, but without invoice (keysend)
    // the node is expected to not settle the tlc
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &node_0;
    let target_pubkey = node_1.pubkey;

    let payment_secret = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");
    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(payment_secret, 20000000000);
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

    let packet = PeeledPaymentOnionPacket::create(
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some_and(|tlc| tlc.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .expect("tlc");
    assert!(matches!(
        tlc.removed_reason,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_basic_two_channels_send_each_other_multiple_time() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    for _i in 0..2 {
        let res = node_0.send_mpp_payment(&node_1, 20000000000, Some(2)).await;

        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_0.wait_until_success(payment_hash).await;

        let payment_session = node_0.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());

        tokio::time::sleep(Duration::from_secs(2)).await;

        let res = node_1.send_mpp_payment(&node_0, 20000000000, Some(2)).await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_1.wait_until_success(payment_hash).await;

        let payment_session = node_1.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let res = node_0.send_mpp_payment(&node_1, 20000000000, Some(2)).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_three_channels_send_each_other_multiple_time() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + 1000 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (0, 1),
                (MIN_RESERVED_CKB + 1000 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (0, 1),
                (MIN_RESERVED_CKB + 1000 * 100000000, MIN_RESERVED_CKB),
            ),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    for _i in 0..4 {
        let res = node_0
            .send_mpp_payment(&node_1, 2100 * 100000000, None)
            .await;

        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_0.wait_until_success(payment_hash).await;

        let payment_session = node_0.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());

        tokio::time::sleep(Duration::from_secs(1)).await;

        let res = node_1
            .send_mpp_payment(&node_0, 2100 * 100000000, None)
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_1.wait_until_success(payment_hash).await;

        let payment_session = node_1.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let node_0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 1000 * 100000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = node_0.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 1000 * 100000000);
    assert_eq!(node_1_balance, 0);

    let node_0_balance = node_0.get_local_balance_from_channel(channels[2]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[2]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 1000 * 100000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_with_two_one_two_network() {
    init_tracing();

    // build a network with 8 nodes, and 9 channels
    let (nodes, _channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (0, 2),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (1, 3),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (2, 3),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (3, 4),
                (MIN_RESERVED_CKB + 3000 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (4, 5),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (4, 6),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (5, 7),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
            (
                (6, 7),
                (MIN_RESERVED_CKB + 1100 * 100000000, MIN_RESERVED_CKB),
            ),
        ],
        8,
    )
    .await;

    let [node_0, _node_1, _node_2, _node_3, _node_4, _node_5, _node_6, node_7] =
        nodes.try_into().expect("8 nodes");

    let res = node_0
        .send_mpp_payment(&node_7, 2000 * 100000000, None)
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_with_generated_invoice() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
        ],
        2,
        Some(gen_rpc_config()),
    )
    .await;

    let too_large_amount_invoice = nodes[1]
        .gen_invoice(NewInvoiceParams {
            amount: 20000000001,
            payment_preimage: Some(gen_rand_sha256_hash()),
            allow_mpp: Some(true),
            ..Default::default()
        })
        .await
        .invoice_address;

    let result = nodes[0]
        .send_payment(SendPaymentCommand {
            invoice: Some(too_large_amount_invoice),
            ..Default::default()
        })
        .await;
    assert!(result.is_err());

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let ok_invoice = nodes[1]
        .gen_invoice(NewInvoiceParams {
            amount: 20000000000,
            payment_preimage: Some(gen_rand_sha256_hash()),
            allow_mpp: Some(true),
            ..Default::default()
        })
        .await
        .invoice_address;

    let result = nodes[0]
        .send_payment(SendPaymentCommand {
            invoice: Some(ok_invoice),
            ..Default::default()
        })
        .await
        .expect("send payment");

    nodes[0].wait_until_success(result.payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_with_need_fee() {
    init_tracing();

    // with MPP, we can send a payment successfully
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB)),
            ((0, 3), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 3), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((3, 2), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, node_2, _node_3] = nodes.try_into().expect("2 nodes");

    // query the path without MPP, it will fail with no path found
    let res = node_0
        .send_payment_keysend(&node_2, 10000000000, true)
        .await;
    eprintln!("query res: {:?}", res);
    assert!(res.unwrap_err().to_string().contains("no path found"));

    let res = node_0
        .send_mpp_payment(&node_2, 10000000000, Some(16))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_can_not_find_path_with_max_parts() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    // let res = node_0.send_mpp_payment(&node_1, 50000000000, Some(4)).await;
    // eprintln!("query res: {:?}", res);

    // assert!(res.is_err());
    // let error = res.unwrap_err().to_string();
    // assert!(error.contains("Failed to build enough routes"));

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = node_0.send_mpp_payment(&node_1, 50000000000, Some(5)).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    eprintln!("second query res: {:?}", res);

    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn test_send_payment_custom_records_not_in_range() {
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &node_0;
    let target_pubkey = node_1.pubkey;

    let data: HashMap<_, _> = vec![(
        BasicMppPaymentData::CUSTOM_RECORD_KEY,
        "hello".to_string().into_bytes(),
    )]
    .into_iter()
    .collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await;

    let error = res.unwrap_err().to_string();
    assert!(error.contains("custom_records key should in range 0 ~ 65535"));

    let data: HashMap<_, _> = vec![(
        USER_CUSTOM_RECORDS_MAX_INDEX,
        "hello".to_string().into_bytes(),
    )]
    .into_iter()
    .collect();
    let custom_records = PaymentCustomRecords { data };
    let payment = source_node
        .send_mpp_payment_with_command(
            &node_1,
            20000,
            SendPaymentCommand {
                custom_records: Some(custom_records.clone()),
                max_parts: Some(2),
                ..Default::default()
            },
        )
        .await;

    eprintln!("payment: {:?}", payment);

    assert!(payment.is_ok());
    let payment_hash = payment.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_can_not_find_path_filter_target_node_features() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_1, 20000000000, Some(2), true)
        .await;
    eprintln!("query res: {:?}", res);

    let feature = FeatureVector::new();
    node_1.update_node_features(feature).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node_0.expect_event(|event| {
            matches!(event, NetworkServiceEvent::DebugEvent(DebugEvent::Common(msg)) if msg.contains("Received gossip message updates"))
        })
        .await;

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_1, 20000000000, Some(2), true)
        .await;
    eprintln!("query res: {:?}", res);

    let error = res.unwrap_err().to_string();
    assert!(error.contains("MPP is not supported by the target node"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_fail_on_total_amount_not_match() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let target_pubkey = node_2.get_public_key();
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(15000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build()
        .expect("build invoice success");

    let mut modified_ckb_invoice = ckb_invoice.clone();
    modified_ckb_invoice.amount = Some(15001);

    node_2.insert_invoice(modified_ckb_invoice, Some(preimage));

    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            invoice: Some(ckb_invoice.to_string()),
            amount: Some(15000),
            max_parts: Some(2),
            dry_run: false,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_0
        .expect_debug_event("after on_remove_tlc_event session_status: Inflight")
        .await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    eprintln!("now payment session status: {:?}", payment_session.status);

    node_0
        .expect_debug_event("after on_remove_tlc_event session_status: Failed")
        .await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    eprintln!("now payment session status: {:?}", payment_session.status);

    node_0.wait_until_failed(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.attempts_count(), 2);
    assert!(payment_session
        .attempts()
        .all(|x| x.status == AttemptStatus::Failed
            && x.last_error == Some("IncorrectOrUnknownPaymentDetails".to_string())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_can_not_find_path_filter_middle_node_features() {
    async fn test_node_feature(update_node_index: usize) {
        init_tracing();
        let (nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
                (
                    (1, 2),
                    (
                        MIN_RESERVED_CKB + (100 + 2) * CKB_SHANNONS as u128,
                        MIN_RESERVED_CKB,
                    ),
                ),
                (
                    (1, 2),
                    (
                        MIN_RESERVED_CKB + (100 + 2) * CKB_SHANNONS as u128,
                        MIN_RESERVED_CKB,
                    ),
                ),
            ],
            3,
        )
        .await;
        let [mut node_0, mut node_1, mut node_2] = nodes.try_into().expect("2 nodes");

        let res = node_0
            .send_mpp_payment_with_dry_run_option(
                &node_2,
                200 * CKB_SHANNONS as u128,
                Some(2),
                true,
            )
            .await;
        eprintln!("query res: {:?}", res);
        assert!(res.is_ok());

        let update_node = match update_node_index {
            0 => &node_0,
            1 => &node_1,
            2 => &node_2,
            _ => panic!("Invalid node index"),
        };

        let feature = FeatureVector::new();
        update_node.update_node_features(feature).await;

        for node in [&mut node_0, &mut node_1, &mut node_2] {
            node.expect_event(|event| {
                matches!(event, NetworkServiceEvent::DebugEvent(DebugEvent::Common(msg)) if msg.contains("Received gossip message updates"))
            })
            .await;
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let res = node_0
            .send_mpp_payment_with_dry_run_option(
                &node_2,
                200 * CKB_SHANNONS as u128,
                Some(2),
                true,
            )
            .await;
        eprintln!("query res: {:?}", res);

        assert!(res.is_err());
    }

    test_node_feature(0).await;
    test_node_feature(1).await;
    test_node_feature(2).await;
}

#[tokio::test]
async fn test_send_mpp_with_large_min_tlc_value_in_channel() {
    init_tracing();
    // FIXME: the best find_path for MPP will find a path with 2 channels,
    // but the current implementation is a greedy algorithm,
    // it will find a path with 1000 CKB, then another path with 1 CKB
    // which is less than the min_tlc_value of the channel,
    // so the payment will fail with "no path found"
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: 1000 * CKB_SHANNONS as u128 + MIN_RESERVED_CKB,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_min_value: Some(10 * CKB_SHANNONS as u128),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: 1000 * CKB_SHANNONS as u128 + MIN_RESERVED_CKB,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_min_value: Some(10 * CKB_SHANNONS as u128),
                    ..Default::default()
                },
            ),
        ],
        2,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0
        .send_mpp_payment(&node_1, 1001 * CKB_SHANNONS as u128, Some(2))
        .await;

    debug!("res: {:?}", res);
    let error = res.unwrap_err();
    assert!(error.contains("no path found"));

    let res = node_0
        .send_mpp_payment(&node_1, 1010 * CKB_SHANNONS as u128, Some(2))
        .await;

    debug!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    eprintln!("begin to wait for payment: {} success ...", payment_hash);
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_send_mpp_with_reverse_node_send_back() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB)),
            ((0, 3), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 3), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((3, 2), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, node_2, _node_3] = nodes.try_into().expect("4 nodes");

    // node 0 send to node 2 with 30000000000 CKB
    for _ in 0..3 {
        let res = node_0
            .send_mpp_payment(&node_2, 10000000000, Some(16))
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_0.wait_until_success(payment_hash).await;
    }

    // node 0 does not have enough balance to send node 2 now
    let res = node_0
        .send_mpp_payment(&node_2, 20000000000, Some(16))
        .await;
    eprintln!("res: {:?}", res);
    assert!(res
        .unwrap_err()
        .contains("Failed to build enough routes for MPP payment"));

    // now node 2 send back to node 0 20000000000 CKB
    for _ in 0..2 {
        let res = node_2
            .send_mpp_payment(&node_0, 10000000000, Some(16))
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_2.wait_until_success(payment_hash).await;
    }

    // now node 0 should have enough balance to send node 2 again
    let res = node_0
        .send_mpp_payment(&node_2, 10000000000, Some(16))
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    eprintln!("begin to wait for payment: {} success ...", payment_hash);
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_3_nodes_pay_self() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 66000, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (MIN_RESERVED_CKB + 11000, MIN_RESERVED_CKB)),
            ((2, 3), (MIN_RESERVED_CKB + 11000, MIN_RESERVED_CKB)),
            ((2, 3), (MIN_RESERVED_CKB + 11000, MIN_RESERVED_CKB)),
            // path_find will first try this direction, since it's with larger capacity
            // but the middle hops for this direction don't have enough capacity
            // so path finding should try another direction and succeed
            ((3, 0), (MIN_RESERVED_CKB + 76000, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, _node_3] = nodes.try_into().expect("4 nodes");

    let amount = 30000;
    let target_pubkey = node_0.get_public_key();
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build()
        .expect("build invoice success");

    node_0.insert_invoice(ckb_invoice.clone(), Some(preimage));
    let command = SendPaymentCommand {
        max_parts: Some(3),
        invoice: Some(ckb_invoice.to_string()),
        allow_self_payment: true,
        ..Default::default()
    };
    let res = node_0.send_payment(command).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_send_mpp_respect_min_tlc_value() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: 50000 + MIN_RESERVED_CKB,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: 10100 + MIN_RESERVED_CKB,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_min_value: Some(9000), // with a min_tlc value
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: 10100 + MIN_RESERVED_CKB,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_min_value: Some(9000), // with a min_tlc value
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: 10100 + MIN_RESERVED_CKB,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_min_value: Some(9000), // with a min_tlc value
                    ..Default::default()
                },
            ),
        ],
        3,
        None,
    )
    .await;

    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let res = node_0.send_payment_keysend(&node_2, 10000, true).await;
    assert!(res.is_ok());

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_2, 28000, Some(3), true)
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_err());

    let res = node_0
        .send_mpp_payment_with_dry_run_option(&node_2, 30000, None, true)
        .await;
    debug!("res: {:?}", res);
    assert!(res.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_can_retry() {
    init_tracing();

    // we have 4 channels in the middle, but we disable a channel quite,
    // Basic MPP will retry the payment with the other channels and succeed
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((2, 3), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let res = node_0.send_mpp_payment(&node_3, 30000000000, Some(3)).await;
    node_1.disable_channel_stealthy(channels[3]).await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    let attempts = payment_session.attempts();

    let mut retry_times: Vec<_> = attempts.map(|x| x.tried_times).collect();
    retry_times.sort();
    assert_eq!(retry_times, vec![1, 1, 2]);
}

#[tokio::test]
async fn test_send_payment_3_nodes_failed_last_hop() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, mut node_1, mut node_2] = nodes.try_into().expect("3 nodes");
    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(msg)) => {
                msg.starts_with("Channel is now ready")
            }
            _ => false,
        })
        .await;

    node_2.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let payment = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(100),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    node_0
        .wait_until_failed(payment.unwrap().payment_hash)
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node_2.start().await;

    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(msg)) => {
                msg.starts_with("Channel is now ready")
            }
            _ => false,
        })
        .await;

    node_0.clear_history().await;

    let payment = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(100),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    debug!("payment: {:?}", payment);
    node_0
        .wait_until_success(payment.unwrap().payment_hash)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_tlc_expiry_soon_first_hop() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .payee_pub_key(node_1.pubkey.into())
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = node_0
        .send_payment(SendPaymentCommand {
            amount: Some(10000),
            keysend: Some(true),
            target_pubkey: Some(node_1.pubkey),
            ..Default::default()
        })
        .await
        .expect("send payment success")
        .payment_hash;

    node_0.wait_until_success(payment_hash).await;
    let mut payment_session = node_0.get_payment_session(payment_hash).unwrap();

    let hash_algorithm = HashAlgorithm::CkbHash;
    let funding_tx_hash = node_0.get_channel_funding_tx(&channels[0]).unwrap();
    let hops_infos = vec![
        PaymentHopData {
            amount: 1100,
            expiry: now_timestamp_as_millis_u64() + 1, // too soon
            next_hop: Some(node_1.pubkey),
            funding_tx_hash,
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 1000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Default::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
    ];

    payment_session.status = PaymentStatus::Created;
    payment_session.last_error = None;
    let mut attempt = payment_session.attempts().next().cloned().unwrap();
    attempt.status = AttemptStatus::Retrying;
    attempt.last_error = None;
    attempt.route_hops = hops_infos;
    attempt.tried_times = 0;
    attempt.last_error = Some("".to_string());
    node_0.update_payment_session(payment_session);
    node_0.update_payment_attempt(attempt.clone());

    node_0
        .retry_send_payment(payment_hash, Some(attempt.id))
        .await;
    node_0.wait_until_failed(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    let attempts: Vec<_> = payment_session.attempts().collect();
    println!("attempts: {:?}", attempts);
    assert!(payment_session
        .last_error
        .unwrap()
        .contains("The tlc expiry soon"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_tlc_expiry_soon() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    let target_pubkey = node_2.pubkey;

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .build()
        .expect("build invoice success");

    node_2.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = node_0
        .send_payment(SendPaymentCommand {
            amount: Some(10000),
            keysend: Some(true),
            target_pubkey: Some(target_pubkey),
            ..Default::default()
        })
        .await
        .expect("send payment success")
        .payment_hash;

    node_0.wait_until_success(payment_hash).await;
    let mut payment_session = node_0.get_payment_session(payment_hash).unwrap();
    let attempts: Vec<_> = payment_session.attempts().collect();
    let route = attempts[0].route_hops.clone();
    for x in route {
        eprintln!("debug router: {:?}", x);
    }

    let hash_algorithm = HashAlgorithm::CkbHash;
    let funding_tx_hash = node_0.get_channel_funding_tx(&channels[0]).unwrap();
    let hops_infos = vec![
        PaymentHopData {
            amount: 1100,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(node_1.pubkey),
            funding_tx_hash,
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 1000,
            expiry: now_timestamp_as_millis_u64() + 1, // too soon
            next_hop: Some(node_2.pubkey),
            funding_tx_hash: node_1.get_channel_funding_tx(&channels[1]).unwrap(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 1000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: node_1.get_channel_funding_tx(&channels[1]).unwrap(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
    ];

    payment_session.status = PaymentStatus::Created;
    payment_session.last_error = None;
    let mut attempt = payment_session.attempts().next().cloned().unwrap();
    attempt.status = AttemptStatus::Retrying;
    attempt.last_error = None;
    attempt.route_hops = hops_infos;
    attempt.tried_times = 0;
    attempt.last_error = Some("".to_string());
    node_0.update_payment_session(payment_session);
    node_0.update_payment_attempt(attempt.clone());

    node_0
        .retry_send_payment(payment_hash, Some(attempt.id))
        .await;
    node_0.wait_until_failed(payment_hash).await;
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    let attempts: Vec<_> = payment_session.attempts().collect();
    println!("attempts: {:?}", attempts);
    assert!(payment_session
        .last_error
        .unwrap()
        .contains("IncorrectTlcExpiry"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_send_each_other_with_no_fee() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 1000 * 100000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_fee_proportional_millionths: Some(0),
                    b_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 1000 * 100000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_fee_proportional_millionths: Some(0),
                    b_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 2000 * 100000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_fee_proportional_millionths: Some(0),
                    b_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
        ],
        3,
        None,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    for _i in 0..2 {
        let res = node_0
            .send_mpp_payment(&node_2, 2000 * 100000000, None)
            .await;

        let payment_hash = res.unwrap().payment_hash;
        node_0.wait_until_success(payment_hash).await;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let res = node_2
            .send_mpp_payment(&node_0, 2000 * 100000000, None)
            .await;

        let payment_hash = res.unwrap().payment_hash;
        node_2.wait_until_success(payment_hash).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "need to modify MILLI_SECONDS_PER_EPOCH and MIN_TLC_EXPIRY_DELTA to run this test"]
async fn test_send_mpp_send_each_other_expire_soon() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 1000 * 100000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_fee_proportional_millionths: Some(0),
                    b_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 1000 * 100000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_fee_proportional_millionths: Some(0),
                    b_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 2000 * 100000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    a_tlc_fee_proportional_millionths: Some(0),
                    b_tlc_fee_proportional_millionths: Some(0),
                    ..Default::default()
                },
            ),
        ],
        3,
        None,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    for _i in 0..2 {
        let res = node_0
            .send_mpp_payment(&node_2, 2000 * 100000000, None)
            .await;

        let payment_hash = res.unwrap().payment_hash;
        node_0.wait_until_success(payment_hash).await;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let res = node_2
            .send_mpp_payment(&node_0, 2000 * 100000000, None)
            .await;

        let payment_hash = res.unwrap().payment_hash;
        node_2.wait_until_success(payment_hash).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_mpp_with_node_not_in_graph() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let wrong_target_pubkey = gen_rand_secp256k1_public_key();
    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .payee_pub_key(wrong_target_pubkey)
        .build()
        .expect("build invoice success");

    node_2.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = node_0
        .send_payment(SendPaymentCommand {
            max_parts: Some(10),
            invoice: Some(ckb_invoice.to_string()),
            ..Default::default()
        })
        .await;

    let error = payment_hash.unwrap_err().to_string();
    assert!(error.contains("no path found"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_find_path_perf() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    // let result = node_0
    //     .send_mpp_payment(&node_2, 100000 * 30, Some(10))
    //     .await;

    // assert!(result.is_err());
    // let find_path_count = node_0.get_payment_path_count_sum().await;
    // eprintln!("now find_path_count: {:?}", find_path_count);
    // assert_eq!(find_path_count, 1);

    let result = node_0.send_mpp_payment(&node_2, 90000 * 2, Some(10)).await;
    assert!(result.is_ok());
    let payment_hash = result.unwrap().payment_hash;
    let find_path_count = node_0.get_payment_find_path_count(payment_hash).await;
    eprintln!("haha find_path_count: {:?}", find_path_count);
    assert!(find_path_count.is_some());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
}
