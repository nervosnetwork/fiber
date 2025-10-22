#![allow(clippy::needless_range_loop)]
use crate::fiber::builtin_records::BasicMppPaymentData;
use crate::fiber::channel::*;
use crate::fiber::config::DEFAULT_FINAL_TLC_EXPIRY_DELTA;
use crate::fiber::config::DEFAULT_TLC_EXPIRY_DELTA;
use crate::fiber::config::DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS;
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::config::MIN_TLC_EXPIRY_DELTA;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::*;
use crate::fiber::payment::*;
use crate::fiber::types::*;
use crate::fiber::NetworkActorCommand;
use crate::fiber::NetworkActorMessage;
use crate::gen_rand_fiber_public_key;
use crate::gen_rand_sha256_hash;
use crate::invoice::CkbInvoice;
use crate::invoice::Currency;
use crate::invoice::InvoiceBuilder;
use crate::now_timestamp_as_millis_u64;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::invoice::NewInvoiceParams;
use crate::tasks::cancel_tasks_and_wait_for_completion;
use crate::test_utils::init_tracing;
use crate::tests::test_utils::*;
use crate::NetworkServiceEvent;
use ckb_types::packed::Script;
use ckb_types::{core::tx_pool::TxStatus, packed::OutPoint};
use ractor::call;
use secp256k1::Secp256k1;
use std::collections::HashMap;
use std::collections::HashSet;
use std::panic;
use std::time::Duration;
use std::time::SystemTime;
use tracing::debug;
use tracing::error;
use tracing::info;

#[tokio::test]
async fn test_send_payment_custom_records() {
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

    let data: HashMap<_, _> = vec![
        (1, "hello".to_string().into_bytes()),
        (2, "world".to_string().into_bytes()),
    ]
    .into_iter()
    .collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_final_status(payment_hash).await;

    assert_eq!(
        source_node.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );
    let got_custom_records = node_1
        .get_payment_custom_records(&payment_hash)
        .expect("custom records");
    assert_eq!(got_custom_records, custom_records);
}

#[tokio::test]
async fn test_send_payment_custom_records_with_limit_error() {
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

    let long_value = "a".repeat(MAX_CUSTOM_RECORDS_SIZE + 1);
    let data: HashMap<_, _> = vec![(1, long_value.into_bytes())].into_iter().collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await;

    let err = res.unwrap_err().to_string();
    assert!(err.contains("the sum size of custom_records's value can not more than"));

    // normal case
    let long_value = "a".repeat(1024 * 2);
    let data: HashMap<_, _> = vec![(1, long_value.into_bytes())].into_iter().collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;
    let got_custom_records = node_1
        .get_payment_custom_records(&payment_hash)
        .expect("custom records");
    assert_eq!(got_custom_records, custom_records);
}

// This test will send two payments from node_0 to node_1, the first payment will run
// with dry_run, the second payment will run without dry_run. Both payments will be successful.
// But only one payment balance will be deducted from node_0.
#[tokio::test]
async fn test_send_payment_for_direct_channel_and_dry_run() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/359

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let channel = channels[0];
    let source_node = &node_0;

    let res = source_node
        .send_payment_keysend(&node_1, 10000000000, true)
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = source_node
        .send_payment_keysend(&node_1, 10000000000, false)
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    let node_0_balance = source_node.get_local_balance_from_channel(channel);
    let node_1_balance = node_1.get_local_balance_from_channel(channel);

    // A -> B: 10000000000 use the first channel
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_send_payment_prefer_newer_channels() {
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

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // We are using the second (newer) channel, so the first channel's balances are unchanged.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    // We are using the second (newer) channel, so the second channel's balances are changed.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_send_payment_prefer_channels_with_larger_balance() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            // These two channels have the same overall capacity, but the second channel has more balance for node_0.
            (
                (0, 1),
                (MIN_RESERVED_CKB + 5000000000, MIN_RESERVED_CKB + 5000000000),
            ),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(5000000000),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // We are using the second channel (with larger balance), so the first channel's balances are unchanged.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 5000000000);
    assert_eq!(node_1_balance, 5000000000);

    // We are using the second channel (with larger balance), so the second channel's balances are changed.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 5000000000);
    assert_eq!(node_1_balance, 5000000000);
}

#[tokio::test]
async fn test_send_payment_with_tool_large_fee_and_amount() {
    init_tracing();

    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let target_pubkey = node_1.pubkey;

    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(1),
            max_fee_amount: Some(u128::MAX),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("max_fee_amount"));
}

#[tokio::test]
async fn test_send_payment_fee_rate() {
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

    let res = node_0
        .send_payment_keysend(&node_2, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    assert!(res.fee > 0);
    let nodes = &res.routers[0].nodes;
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[2].amount, 10_000_000);
    assert_eq!(nodes[1].amount, 10_000_000);
    // The fee is 10_000_000 * 3_000_000 (fee rate) / 1_000_000 = 30_000_000
    assert_eq!(nodes[0].amount, 40_000_000);
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;

    let res = node_2.send_payment_keysend(&node_0, 1_000_000, false).await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    assert!(res.fee > 0);
    let nodes = &res.routers[0].nodes;
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[2].amount, 1_000_000);
    assert_eq!(nodes[1].amount, 1_000_000);
    // The fee is 1_000_000 * 2_000_000 (fee rate) / 1_000_000 = 2_000_000
    assert_eq!(nodes[0].amount, 3_000_000);

    let payment_hash = res.payment_hash;
    node_2.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_over_private_channel() {
    async fn test(amount_to_send: u128, is_payment_ok: bool) {
        let (nodes, _channels) = create_n_nodes_network(
            &[((1, 2), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB))],
            3,
        )
        .await;
        let [mut node1, mut node2, node3] = nodes.try_into().expect("3 nodes");

        let (_new_channel_id, _funding_tx) = establish_channel_between_nodes(
            &mut node1,
            &mut node2,
            ChannelParameters {
                public: false,
                node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
                node_b_funding_amount: MIN_RESERVED_CKB,
                ..Default::default()
            },
        )
        .await;

        let source_node = &mut node1;
        let target_pubkey = node3.pubkey;

        let res = source_node
            .send_payment(SendPaymentCommand {
                target_pubkey: Some(target_pubkey),
                amount: Some(amount_to_send),
                keysend: Some(true),
                ..Default::default()
            })
            .await;

        eprintln!("res: {:?}", res);
        if is_payment_ok {
            assert!(res.is_ok());
            source_node
                .wait_until_success(res.unwrap().payment_hash)
                .await;
        } else {
            assert!(res.is_err());
        }
    }

    test(10000000000, true).await;
    test(30000000000, false).await;
}

#[tokio::test]
async fn test_send_payment_for_pay_self() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/362

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((2, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // now node_0 -> node_2 will be ok only with node_1, so the fee is larger than 0
    let res = node_0.send_payment_keysend(&node_2, 60000000, true).await;

    assert!(res.unwrap().fee > 0);

    // node_0 -> node_0 will be ok for dry_run if `allow_self_payment` is true
    let res = node_0.send_payment_keysend_to_self(60000000, false).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    assert_eq!(node_0_balance1, 10000000000 - 60000000 - res.fee);
    assert_eq!(node_0_balance2, 60000000);

    eprintln!(
        "node1 left: {:?}, right: {:?}",
        node_1.get_local_balance_from_channel(channels[0]),
        node_1.get_local_balance_from_channel(channels[1])
    );

    let node_1_new_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_new_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    let node1_fee = (node_1_new_channel0_balance - node_1_channel0_balance)
        - (node_1_channel1_balance - node_1_new_channel1_balance);
    assert!(node1_fee > 0);

    let node2_fee = (node_2_new_channel1_balance - node_2_channel1_balance)
        - (node_2_channel2_balance - node_2_new_channel2_balance);
    assert!(node2_fee > 0);
    assert_eq!(node1_fee + node2_fee, res.fee);

    // node_0 -> node_2 will be ok with direct channel2,
    // since after payself this channel now have enough balance, so the fee is 0
    let res = node_0.send_payment_keysend(&node_2, 60000000, true).await;

    eprintln!("res: {:?}", res);
    assert_eq!(res.unwrap().fee, 0);
}

#[tokio::test]
async fn test_send_payment_for_pay_self_with_two_nodes() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/355

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);

    // node_0 -> node_0 will be ok for dry_run if `allow_self_payment` is true
    let res = node_0.send_payment_keysend_to_self(60000000, false).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[1]);

    assert_eq!(node_0_balance1, 10000000000 - 60000000 - res.fee);
    assert_eq!(node_0_balance2, 60000000);

    let new_node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let new_node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);

    let node1_fee = (new_node_1_channel0_balance - node_1_channel0_balance)
        - (node_1_channel1_balance - new_node_1_channel1_balance);
    eprintln!("fee: {:?}", res.fee);
    assert_eq!(node1_fee, res.fee);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_for_pay_self_with_invoice() {
    init_tracing();
    let (nodes, channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
            (
                (2, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
        ],
        3,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, _node_1, _node_2] = nodes.try_into().expect("3 nodes");

    let old_node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let old_node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    let invoice = node_0
        .gen_invoice(NewInvoiceParams {
            amount: 100,
            description: Some("test invoice".to_string()),
            expiry: None,
            ..Default::default()
        })
        .await;

    // node_0 -> node_0 will be ok for pay_self with invoice
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            amount: None,
            keysend: None,
            allow_self_payment: true,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let node_0_sent = old_node_0_balance1 - node_0.get_local_balance_from_channel(channels[0]);
    let node_0_received = node_0.get_local_balance_from_channel(channels[2]) - old_node_0_balance2;
    let fee = res.fee;
    assert_eq!(
        node_0_sent,
        node_0_received + fee,
        "node_0 balance should be changed by fee only"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_normal_invoice_workflow() {
    init_tracing();
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[(
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        )],
        2,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let invoice = node_1
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("test invoice".to_string()),
            expiry: None,
            ..Default::default()
        })
        .await;

    // node_0 -> node_1 will be ok for normal invoice
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            amount: None,
            keysend: None,
            allow_self_payment: false,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_more_capacity_for_payself() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/362

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // node_0 -> node_0 will be ok if `allow_self_payment` is true
    let res = node_0.send_payment_keysend_to_self(60000000, false).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    eprintln!("fee: {:?}", res.fee);
    // for node0 pay to self, only the fee will be deducted
    assert!(node_0_balance1 + node_0_balance2 == 10000000000 + 10000000000 - res.fee);

    eprintln!(
        "node1 left: {:?}, right: {:?}",
        node_1.get_local_balance_from_channel(channels[0]),
        node_1.get_local_balance_from_channel(channels[1])
    );

    let node_1_new_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_new_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // we may route to self from
    //     node0 -> node1 -> node2 -> node0
    // or  node0 -> node2 -> node1 -> node0
    // so the assertion need to be more complex
    let node1_fee = if node_1_new_channel0_balance > node_1_channel0_balance {
        (node_1_new_channel0_balance - node_1_channel0_balance)
            - (node_1_channel1_balance - node_1_new_channel1_balance)
    } else {
        (node_1_new_channel1_balance - node_1_channel1_balance)
            - (node_1_channel0_balance - node_1_new_channel0_balance)
    };
    assert!(node1_fee > 0);

    let node2_fee = if node_2_new_channel1_balance > node_2_channel1_balance {
        (node_2_new_channel1_balance - node_2_channel1_balance)
            - (node_2_channel2_balance - node_2_new_channel2_balance)
    } else {
        (node_2_new_channel2_balance - node_2_channel2_balance)
            - (node_2_channel1_balance - node_2_new_channel1_balance)
    };
    assert_eq!(node1_fee + node2_fee, res.fee);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_private_channel_hints() {
    async fn test(amount_to_send: u128, is_payment_ok: bool) {
        let (nodes, _channels) = create_n_nodes_network(
            &[((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB))],
            3,
        )
        .await;
        let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

        let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
            &mut node2,
            &mut node3,
            ChannelParameters {
                public: false,
                node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
                node_b_funding_amount: MIN_RESERVED_CKB,
                ..Default::default()
            },
        )
        .await;
        let funding_tx = node2
            .get_transaction_view_from_hash(funding_tx_hash)
            .await
            .expect("get funding tx");

        let outpoint = funding_tx.output_pts_iter().next().unwrap();

        let source_node = &mut node1;
        let target_pubkey = node3.pubkey;

        let res = source_node
            .send_payment(SendPaymentCommand {
                target_pubkey: Some(target_pubkey),
                amount: Some(amount_to_send),
                keysend: Some(true),
                hop_hints: Some(vec![HopHint {
                    pubkey: node2.pubkey,
                    channel_outpoint: outpoint,
                    fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                }]),
                ..Default::default()
            })
            .await;

        assert!(res.is_ok(), "Send payment failed: {:?}", res);
        let res = res.unwrap();
        let payment_hash = res.payment_hash;
        if is_payment_ok {
            source_node.wait_until_success(payment_hash).await;
        } else {
            source_node.wait_until_failed(payment_hash).await;
        }
    }

    test(10000000000, true).await;
    test(30000000000, false).await;
}

#[tokio::test]
async fn test_send_payment_with_too_large_hop_hint_fee_rate() {
    init_tracing();
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (u64::MAX as u128 / 3, MIN_RESERVED_CKB))], 3).await;
    let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: u64::MAX as u128 / 3,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    let outpoint = funding_tx.output_pts_iter().next().unwrap();

    let source_node = &mut node1;
    let target_pubkey = node3.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some((u64::MAX / 4) as u128),
            keysend: Some(true),
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: outpoint,
                fee_rate: u64::MAX, // too large fee rate
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_err(), "Expect send payment failed: {:?}", res);
    assert!(res.unwrap_err().to_string().contains("no path found"));
}

#[tokio::test]
async fn test_send_payment_hophint_for_middle_channels_does_not_work() {
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((2, 3), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node1, mut node2, mut node3, node4] = nodes.try_into().expect("4 nodes");

    // create a private channel between node2 and node3
    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    let private_channel_outpoint = funding_tx.output_pts_iter().next().unwrap();

    let res = node1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node4.pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: private_channel_outpoint.clone(),
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();

    // the router is wrong with node1 -> node2 -> node4
    // the second channel is private_channel_outpoint
    assert_eq!(
        res.routers[0].nodes[1].channel_outpoint,
        private_channel_outpoint
    );
    let payment_hash = res.payment_hash;

    // this router will not payment succeeded
    node1.wait_until_failed(payment_hash).await;
    let res = node1.get_payment_result(payment_hash).await;
    eprintln!("res: {:?}", res);
    assert!(res.failed_error.unwrap().contains("InvalidOnionPayload"));
}

#[tokio::test]
async fn test_send_payment_hophint_for_mixed_channels_with_udt() {
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    public: true,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    public: true,
                    ..Default::default()
                },
            ),
            (
                (2, 3),
                ChannelParameters {
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    public: true, // not a private channel
                    funding_udt_type_script: Some(Script::default()), // a UDT channel
                    ..Default::default()
                },
            ),
        ],
        4,
        None,
    )
    .await;
    let [node1, _node2, node3, node4] = nodes.try_into().expect("4 nodes");

    let channel_outpoint = node3.get_channel_outpoint(&_channels[2]).unwrap();

    let res = node1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node4.pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            // hop hints will be ignored because of find_path can get channel_info
            hop_hints: Some(vec![HopHint {
                pubkey: node3.pubkey,
                channel_outpoint,
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_err());
}

#[tokio::test]
async fn test_send_payment_with_private_channel_hints_fallback() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;

    let outpoint = node2.get_channel_outpoint(&_new_channel_id).unwrap();
    let channel1_outpoint = node1.get_channel_outpoint(&_channels[0]).unwrap();
    let channel2_outpoint = node2.get_channel_outpoint(&_channels[1]).unwrap();

    debug!("channel1 outpoint: {:?}", channel1_outpoint);
    debug!("channel2 outpoint: {:?}", channel2_outpoint);
    debug!("private_channel outpoint: {:?}", outpoint);

    let source_node = &mut node1;
    let target_pubkey = node3.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(30000000000),
            keysend: Some(true),
            allow_self_payment: true,
            custom_records: None,
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: outpoint,
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;

    // the actual capacity of private channel is not enough for this payment
    // will first use the private channel, then send payment retry will fallback to public channel
    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(2))
        .await;
}

#[tokio::test]
async fn test_send_payment_payself_with_private_channel_cycle() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node1, _node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node3,
        &mut node1,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let _funding_tx = node3
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    let source_node = &mut node1;

    let res = source_node
        .send_payment_keysend_to_self(30000000000, false)
        .await;

    assert!(res.is_err());

    let res = source_node
        .send_payment_keysend_to_self(10000000000, false)
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
}

#[tokio::test]
async fn test_send_payment_with_private_multiple_channel_hints_fallback() {
    init_tracing();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    async fn create_channel(
        node2: &mut NetworkNode,
        node3: &mut NetworkNode,
        amount: u128,
    ) -> OutPoint {
        let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
            node2,
            node3,
            ChannelParameters {
                public: false,
                node_a_funding_amount: MIN_RESERVED_CKB + amount,
                node_b_funding_amount: MIN_RESERVED_CKB,
                ..Default::default()
            },
        )
        .await;
        node2
            .get_transaction_view_from_hash(funding_tx_hash)
            .await
            .expect("get funding tx")
            .output_pts_iter()
            .next()
            .unwrap()
    }

    let outpoint1 = create_channel(&mut node2, &mut node3, 20000000000).await;
    let outpoint2 = create_channel(&mut node2, &mut node3, 40000000000).await;

    let source_node = &mut node1;
    let target_pubkey = node3.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(30000000000),
            keysend: Some(true),
            hop_hints: Some(vec![
                HopHint {
                    pubkey: node2.pubkey,
                    channel_outpoint: outpoint1,
                    fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                },
                HopHint {
                    pubkey: node2.pubkey,
                    channel_outpoint: outpoint2,
                    fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                },
            ]),
            ..Default::default()
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retry_times(), 2);
}

#[tokio::test]
async fn test_send_payment_build_router_basic() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let router_nodes: Vec<_> = router.router_hops.iter().map(|x| x.target).collect();
    eprintln!("router_nodes: {:?}", router_nodes);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(router_nodes, vec![node_1.pubkey]);
    assert_eq!(amounts, vec![1]);

    let payment = node_0.send_payment_keysend(&node_2, 1, true).await;
    eprintln!("payment: {:?}", payment);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let router_nodes: Vec<_> = router.router_hops.iter().map(|x| x.target).collect();
    eprintln!("router_nodes: {:?}", router_nodes);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(router_nodes, vec![node_1.pubkey, node_2.pubkey]);
    assert_eq!(amounts, vec![2, 1]);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: gen_rand_fiber_public_key(),
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: gen_rand_fiber_public_key(),
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    assert!(router.is_err());
}

#[tokio::test]
async fn test_send_payment_build_router_multiple_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(amounts, vec![2, 1]);

    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    assert_eq!(
        channel_2_funding_tx,
        router.router_hops[1].channel_outpoint.tx_hash().into(),
    );

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[1]).unwrap();
    let channel_1_outpoint = OutPoint::new(channel_1_funding_tx.into(), 0);
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(channel_1_outpoint),
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(amounts, vec![2, 1]);

    assert_eq!(
        channel_1_funding_tx,
        router.router_hops[1].channel_outpoint.tx_hash().into(),
    );
}

#[tokio::test]
async fn test_send_payment_build_router_pay_self() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_0.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    eprintln!("amounts: {:?}", amounts);
    assert_eq!(amounts, vec![3, 2, 1]);

    let router_nodes: Vec<_> = router.router_hops.iter().map(|x| x.target).collect();
    eprintln!("router_nodes: {:?}", router_nodes);
    assert_eq!(
        router_nodes,
        vec![node_1.pubkey, node_2.pubkey, node_0.pubkey]
    );

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[0]).unwrap();
    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();
    assert_eq!(
        vec![
            channel_1_funding_tx,
            channel_2_funding_tx,
            channel_3_funding_tx
        ],
        router
            .router_hops
            .iter()
            .map(|x| x.channel_outpoint.tx_hash().into())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_send_payment_build_router_amount_range() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 1000, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (MIN_RESERVED_CKB + 1000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1000, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, _] = nodes.try_into().expect("3 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(0), // too small
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(1001), // too large
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(1000), // add 1 as fee is too large for channel balance
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(999), // add 1 as fee is ok for channel balance
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_ok());
    let amounts: Vec<_> = router
        .unwrap()
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();

    assert_eq!(amounts, vec![1000, 999]);
}

#[tokio::test]
async fn test_send_payment_with_route_to_self_with_specified_router() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_0.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    eprintln!("result: {:?}", router);

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    eprintln!("fee: {:?}", res.fee);
    // for node0 pay to self, only the fee will be deducted
    assert!(node_0_balance1 + node_0_balance2 == 10000000000 + 10000000000 - res.fee);

    eprintln!(
        "node1 left: {:?}, right: {:?}",
        node_1.get_local_balance_from_channel(channels[0]),
        node_1.get_local_balance_from_channel(channels[1])
    );

    let node_1_new_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_new_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // node0 can only route to self from
    // node0 -> node2 -> node1 -> node0
    let node1_fee = (node_1_new_channel1_balance - node_1_channel1_balance)
        - (node_1_channel0_balance - node_1_new_channel0_balance);

    assert!(node1_fee > 0);

    let node2_fee = (node_2_new_channel2_balance - node_2_channel2_balance)
        - (node_2_channel1_balance - node_2_new_channel1_balance);

    assert_eq!(node1_fee + node2_fee, res.fee);
}

#[tokio::test]
async fn test_send_payment_with_route_with_invalid_parameters() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 3),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, node_3] = nodes.try_into().expect("3 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_3.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap()
        .router_hops;

    // pay to node_3 with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.clone(),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    node_0.wait_until_success(res.unwrap().payment_hash).await;

    // now we change the fee of the first channel
    let mut copy_router = router.clone();
    copy_router[1].amount_received = copy_router[0].amount_received;
    // pay to node_3 with router will be failed
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: copy_router,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_failed(payment_hash).await;
    let result = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");
    assert_eq!(result.retry_times(), 1);

    // ================================================================
    // now we change the expiry delta in the middle hop
    let mut copy_router = router.clone();
    copy_router[1].incoming_tlc_expiry = copy_router[0].incoming_tlc_expiry;
    // pay to node_3 with router will be failed
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: copy_router,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_failed(payment_hash).await;
    let result = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");
    eprintln!("result: {:?}", result.status);
    assert_eq!(result.attempts_count(), 1);
}

#[tokio::test]
async fn test_send_payment_with_route_will_not_consider_prob() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 90000, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 10000, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [mut node_0, mut node_1, mut node_2] = nodes.try_into().expect("3 nodes");

    let payment = node_0
        .send_payment_keysend(&node_2, 9000, false)
        .await
        .unwrap();
    node_0.wait_until_success(payment.payment_hash).await;

    let payment = node_0.send_payment_keysend(&node_2, 9000, false).await;
    node_0
        .wait_until_failed(payment.unwrap().payment_hash)
        .await;

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(9000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    // we don't consider the probability evaluated result
    assert!(router.is_ok());
    eprintln!("result: {:?}", router);

    // if we specify a channel, we will not consider the probability evaluated result
    // as it's user's responsibility to ensure the channel is available
    let channel_0_funding_tx = node_0.get_channel_funding_tx(&channels[0]).unwrap();
    let channel_0_outpoint = OutPoint::new(channel_0_funding_tx.into(), 0);
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(9000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: Some(channel_0_outpoint.clone()),
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    eprintln!("result: {:?}", router);
    assert!(router.is_ok());

    let router = router.unwrap();
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_failed(payment_hash).await;

    // now we build another router from node_1 to node_2, so the capacity
    // will be enough for the payment in this network, build_router will find correct path
    let (channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node_1,
        &mut node_2,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node_1
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    // all the other nodes submit_tx
    let res = node_0.submit_tx(funding_tx.clone()).await;
    assert!(matches!(res, TxStatus::Committed(..)));
    node_0.add_channel_tx(channel_id, funding_tx_hash);

    wait_for_network_graph_update(&node_0, 3).await;

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(9000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: Some(channel_0_outpoint),
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    eprintln!("result: {:?}", router);
    assert!(router.is_ok());

    let router = router.unwrap();
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_with_router_with_multiple_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            // there are 3 channels from node1 -> node2
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
                },
                HopRequire {
                    pubkey: node_3.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    eprintln!("result: {:?}", router);

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    let payment_session = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");
    eprintln!("payment_session: {:?}", &payment_session);
    let used_channels: Vec<Hash256> = payment_session
        .attempts()
        .next()
        .unwrap()
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();
    eprintln!("used_channels: {:?}", used_channels);
    assert_eq!(used_channels.len(), 4);
    assert_eq!(used_channels[1], channel_3_funding_tx);

    // try channel_2
    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    eprintln!("channel_2_funding_tx: {:?}", channel_2_funding_tx);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_2_funding_tx.into(), 0)),
                },
                HopRequire {
                    pubkey: node_3.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    eprintln!("result: {:?}", router);

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    eprintln!("payment_hash: {:?}", payment_hash);
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    eprintln!("payment_session: {:?}", &payment_session);
    let used_channels: Vec<Hash256> = payment_session
        .attempts()
        .next()
        .unwrap()
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();
    eprintln!("used_channels: {:?}", used_channels);
    assert_eq!(used_channels.len(), 4);
    assert_eq!(used_channels[1], channel_2_funding_tx);

    let wrong_channel_hash = Hash256::from([0u8; 32]);
    // if we specify a wrong funding_tx, the payment will fail
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(OutPoint::new(wrong_channel_hash.into(), 0)),
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router
        .unwrap_err()
        .to_string()
        .contains("PathFind error: no path found"));
}

#[tokio::test]
async fn test_send_payment_two_nodes_with_router_and_multiple_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((0, 1), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((1, 0), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((1, 0), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[1]).unwrap();
    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();
    let old_balance = node_0.get_local_balance_from_channel(channels[1]);
    let old_node1_balance = node_1.get_local_balance_from_channel(channels[3]);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_1_funding_tx.into(), 0)),
                },
                HopRequire {
                    pubkey: node_0.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    let payment_session = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");

    let used_channels: Vec<Hash256> = payment_session
        .attempts()
        .next()
        .unwrap()
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();

    assert_eq!(used_channels.len(), 3);
    assert_eq!(used_channels[0], channel_1_funding_tx);
    assert_eq!(used_channels[1], channel_3_funding_tx);

    node_0.wait_until_success(payment_hash).await;

    let balance = node_0.get_local_balance_from_channel(channels[1]);
    assert_eq!(balance, old_balance - 60000000 - res.fee);

    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_1_balance, 60000000 + res.fee);

    let balance = node_0.get_local_balance_from_channel(channels[3]);
    assert_eq!(balance, 60000000);

    let node_1_balance = node_1.get_local_balance_from_channel(channels[3]);
    assert_eq!(node_1_balance, old_node1_balance - 60000000);
}

#[tokio::test]
async fn test_send_payment_send_with_wrong_hop() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 3),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (3, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        4,
    )
    .await;
    let [node_0, node_1, _node_2, node_3] = nodes.try_into().expect("3 nodes");

    let channel_3_funding_tx = node_3.get_channel_funding_tx(&channels[3]).unwrap();

    // can not build a invalid router from node3 -> node_1
    let router = node_3
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    // build a router from node3 -> node_0
    let router = node_3
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![HopRequire {
                pubkey: node_0.pubkey,
                channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    // pay the above router with node_3 will be ok
    let res = node_3
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops.clone(),
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    node_3.wait_until_success(res.payment_hash).await;

    // pay the above router with node_1 will failed
    let res = node_1
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Failed to send onion packet with error UnknownNextPeer"));
}

#[tokio::test]
async fn test_network_send_payment_randomly_send_each_other() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let mut node_a_sent = 0;
    let mut node_b_sent = 0;
    let mut all_sent = vec![];
    for _i in 1..8 {
        let rand_wait_time = rand::random::<u64>() % 100;
        tokio::time::sleep(tokio::time::Duration::from_millis(rand_wait_time)).await;

        let rand_num = rand::random::<u64>() % 2;
        let amount = rand::random::<u128>() % 10000 + 1;
        eprintln!("generated amount: {}", amount);
        let (source, target) = if rand_num == 0 {
            (&node_a, &node_b)
        } else {
            (&node_b, &node_a)
        };

        let res = source
            .send_payment_keysend(target, amount, false)
            .await
            .expect("send payment success");

        if rand_num == 0 {
            all_sent.push((true, amount, res.payment_hash, res.status));
        } else {
            all_sent.push((false, amount, res.payment_hash, res.status));
        }
    }

    // wait for all payments to be settled
    for (a_send, _, payment_hash, _) in all_sent.iter() {
        let sender = if *a_send { &node_a } else { &node_b };
        sender.wait_until_success(*payment_hash).await;
    }

    for (a_sent, amount, payment_hash, create_status) in all_sent {
        let node = if a_sent { &node_a } else { &node_b };
        let res = node.get_payment_result(payment_hash).await;
        if res.status == PaymentStatus::Success {
            assert!(matches!(
                create_status,
                PaymentStatus::Created | PaymentStatus::Inflight
            ));
            eprintln!(
                "{} payment_hash: {:?} success with amount: {} create_status: {:?}",
                if a_sent { "a -> b" } else { "b -> a" },
                payment_hash,
                amount,
                create_status
            );
            if a_sent {
                node_a_sent += amount;
            } else {
                node_b_sent += amount;
            }
        }
    }

    eprintln!(
        "node_a_old_balance: {}, node_b_old_balance: {}",
        node_a_old_balance, node_b_old_balance
    );
    eprintln!("node_a_sent: {}, node_b_sent: {}", node_a_sent, node_b_sent);
    let new_node_a_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let new_node_b_balance = node_b.get_local_balance_from_channel(new_channel_id);

    eprintln!(
        "new_node_a_balance: {}, new_node_b_balance: {}",
        new_node_a_balance, new_node_b_balance
    );

    assert_eq!(
        node_a_old_balance + node_b_old_balance,
        new_node_a_balance + new_node_b_balance
    );
    assert_eq!(
        new_node_a_balance,
        node_a_old_balance - node_a_sent + node_b_sent
    );
    assert_eq!(
        new_node_b_balance,
        node_b_old_balance - node_b_sent + node_a_sent
    );
}

#[tokio::test]
async fn test_network_three_nodes_two_channels_send_each_other() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    let node_b_old_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_old_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);

    let amount_a_to_c = 60000;
    let res = node_a
        .send_payment_keysend(&node_c, amount_a_to_c, false)
        .await
        .unwrap();
    let payment_hash1 = res.payment_hash;
    let fee1 = res.fee;
    eprintln!("payment_hash1: {:?}", payment_hash1);

    let amount_c_to_a = 50000;
    let res = node_c
        .send_payment_keysend(&node_a, amount_c_to_a, false)
        .await
        .unwrap();

    let payment_hash2 = res.payment_hash;
    let fee2 = res.fee;
    eprintln!("payment_hash2: {:?}", payment_hash2);

    node_a.wait_until_success(payment_hash1).await;
    node_c.wait_until_success(payment_hash2).await;

    let new_node_b_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let new_node_b_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);

    let node_b_fee = new_node_b_balance_channel_0 + new_node_b_balance_channel_1
        - node_b_old_balance_channel_0
        - node_b_old_balance_channel_1;

    eprintln!("node_b_fee: {}", node_b_fee);
    eprintln!("fee1: {}, fee2: {}", fee1, fee2);
    assert_eq!(node_b_fee, fee1 + fee2);
}

#[tokio::test]
async fn test_network_three_nodes_send_each_other() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait for the channel announcement to be broadcasted
    let node_b_old_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_old_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);
    let node_b_old_balance_channel_2 = node_b.get_local_balance_from_channel(channels[2]);
    let node_b_old_balance_channel_3 = node_b.get_local_balance_from_channel(channels[3]);

    eprintln!(
        "node_b_old_balance_channel_0: {}, node_b_old_balance_channel_1: {}",
        node_b_old_balance_channel_0, node_b_old_balance_channel_1
    );
    eprintln!(
        "node_b_old_balance_channel_2: {}, node_b_old_balance_channel_3: {}",
        node_b_old_balance_channel_2, node_b_old_balance_channel_3
    );

    let amount_a_to_c = 60000;
    let res = node_a
        .send_payment_keysend(&node_c, amount_a_to_c, false)
        .await
        .expect("send payment ok");
    let payment_hash1 = res.payment_hash;
    let fee1 = res.fee;
    eprintln!("payment_hash1: {:?}", payment_hash1);

    let amount_c_to_a = 60000;
    let res = node_c
        .send_payment_keysend(&node_a, amount_c_to_a, false)
        .await
        .expect("send payment ok");

    let payment_hash2 = res.payment_hash;
    let fee2 = res.fee;
    eprintln!("payment_hash2: {:?}", payment_hash2);

    node_a.wait_until_success(payment_hash1).await;
    node_c.wait_until_success(payment_hash2).await;

    let new_node_b_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let new_node_b_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);
    let new_node_b_balance_channel_2 = node_b.get_local_balance_from_channel(channels[2]);
    let new_node_b_balance_channel_3 = node_b.get_local_balance_from_channel(channels[3]);

    let node_b_fee = new_node_b_balance_channel_0
        + new_node_b_balance_channel_1
        + new_node_b_balance_channel_2
        + new_node_b_balance_channel_3
        - node_b_old_balance_channel_0
        - node_b_old_balance_channel_1
        - node_b_old_balance_channel_2
        - node_b_old_balance_channel_3;

    eprintln!("node_b_fee: {}", node_b_fee);
    assert_eq!(node_b_fee, fee1 + fee2);
}

#[tokio::test]
async fn test_send_payment_bench_test() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, node_3] = nodes.try_into().expect("3 nodes");

    let mut all_sent = HashSet::new();

    for i in 1..=15 {
        assert!(node_0.get_triggered_unexpected_events().await.is_empty());
        assert!(node_1.get_triggered_unexpected_events().await.is_empty());
        assert!(node_2.get_triggered_unexpected_events().await.is_empty());
        assert!(node_3.get_triggered_unexpected_events().await.is_empty());

        if let Ok(payment) = node_0.send_payment_keysend(&node_3, 100, false).await {
            eprintln!("payment: {:?}", payment);
            all_sent.insert(payment.payment_hash);
            info!("send: {} payment_hash: {:?} sent", i, payment.payment_hash);
        }
    }

    let time = std::time::Instant::now();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        for payment_hash in all_sent.clone().iter() {
            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("got payment: {:?} status: {:?}", payment_hash, status);
            if status == PaymentStatus::Success {
                eprintln!("payment_hash: {:?} success", payment_hash);
                all_sent.remove(payment_hash);
                info!(
                    "payment_hash: {:?} success, left: {:?}",
                    payment_hash,
                    all_sent.len()
                );
            }
        }

        if all_sent.is_empty() {
            break;
        }
        if time.elapsed().as_secs() >= 300 {
            panic!("timeout, not all payments are settled");
        }
    }
}

#[tokio::test]
async fn test_send_payment_three_nodes_wait_succ_bench_test() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let mut all_sent = vec![];

    for i in 1..=10 {
        let payment = node_0
            .send_payment_keysend(&node_2, 1000, false)
            .await
            .unwrap();
        all_sent.push(payment.payment_hash);
        eprintln!(
            "send: {} payment_hash: {:?} sentxx",
            i, payment.payment_hash
        );

        node_0.wait_until_success(payment.payment_hash).await;
    }
}

#[tokio::test]
async fn test_send_payment_three_nodes_send_each_other_bench_test() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let mut all_sent = vec![];

    for i in 1..=5 {
        let payment1 = node_0
            .send_payment_keysend(&node_2, 1000, false)
            .await
            .unwrap();
        all_sent.push(payment1.payment_hash);
        eprintln!("send: {} payment_hash: {:?} sent", i, payment1.payment_hash);

        let payment2 = node_2
            .send_payment_keysend(&node_0, 1000, false)
            .await
            .unwrap();
        all_sent.push(payment2.payment_hash);
        eprintln!("send: {} payment_hash: {:?} sent", i, payment2.payment_hash);

        node_0.wait_until_success(payment1.payment_hash).await;
        node_2.wait_until_success(payment2.payment_hash).await;
    }
}

#[tokio::test]
async fn test_send_payment_three_nodes_send_each_other_no_wait() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let mut all_sent = vec![];
    let node_0_balance = nodes[0].get_local_balance_from_channel(channels[0]);
    let node_2_balance = nodes[2].get_local_balance_from_channel(channels[1]);

    let amount = 100000;
    let mut node_0_sent_fee = 0;
    let mut node_0_sent_amount = 0;
    let mut node_2_sent_fee = 0;
    let mut node_2_sent_amount = 0;
    for _i in 0..4 {
        for _k in 0..3 {
            let payment1 = nodes[0]
                .send_payment_keysend(&nodes[2], amount, false)
                .await
                .unwrap();
            eprintln!(
                "send: {} payment_hash: {:?} sent, fee: {:?}",
                _i, payment1.payment_hash, payment1.fee
            );
            node_0_sent_fee += payment1.fee;
            node_0_sent_amount += amount;
            all_sent.push((0, payment1.payment_hash));
        }

        let payment2 = nodes[2]
            .send_payment_keysend(&nodes[0], amount, false)
            .await
            .unwrap();
        all_sent.push((2, payment2.payment_hash));
        eprintln!(
            "send: {} payment_hash: {:?} sent, fee: {:?}",
            _i, payment2.payment_hash, payment2.fee
        );
        node_2_sent_fee += payment2.fee;
        node_2_sent_amount += amount;
    }

    loop {
        for (node_index, payment_hash) in all_sent.clone().iter() {
            let node = &nodes[*node_index];
            node.wait_until_success(*payment_hash).await;
            all_sent.retain(|x| x.1 != *payment_hash);
        }
        if all_sent.is_empty() {
            break;
        }
    }
    let new_node_0_balance = nodes[0].get_local_balance_from_channel(channels[0]);
    let new_node_2_balance = nodes[2].get_local_balance_from_channel(channels[1]);
    eprintln!(
        "node_0_balance: {}, new_node_0_balance: {}, node_0_sent_amount: {}, node_0_sent_fee: {}",
        node_0_balance, new_node_0_balance, node_0_sent_amount, node_0_sent_fee,
    );
    eprintln!(
        "node_2_balance: {}, new_node_2_balance: {}, node_2_sent_amount: {}, node_2_sent_fee: {}",
        node_2_balance, new_node_2_balance, node_2_sent_amount, node_2_sent_fee
    );
    assert_eq!(
        new_node_0_balance,
        node_0_balance - node_0_sent_fee - 8 * amount
    );
    assert_eq!(
        new_node_2_balance,
        node_2_balance - node_2_sent_fee + 8 * amount
    );
}

#[tokio::test]
async fn test_send_payment_three_nodes_bench_test() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let mut all_sent = HashSet::new();
    let mut node_2_got_fee = 0;
    let mut node1_got_amount = 0;
    let mut node_1_sent_fee = 0;
    let mut node3_got_amount = 0;
    let mut node_3_sent_fee = 0;
    let mut node_2_ch1_sent_amount = 0;
    let mut node_2_ch2_sent_amount = 0;

    let old_node_1_amount = nodes[0].get_local_balance_from_channel(channels[0]);
    let old_node_2_chnnale1_amount = nodes[1].get_local_balance_from_channel(channels[0]);
    let old_node_2_chnnale2_amount = nodes[1].get_local_balance_from_channel(channels[1]);
    let old_node_3_amount = nodes[2].get_local_balance_from_channel(channels[1]);

    for i in 1..=4 {
        let payment1 = nodes[0]
            .send_payment_keysend(&nodes[2], 1000, false)
            .await
            .unwrap();
        all_sent.insert((1, payment1.payment_hash, payment1.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment1.payment_hash);
        node_1_sent_fee += payment1.fee;
        node_2_got_fee += payment1.fee;

        let payment2 = nodes[1]
            .send_payment_keysend(&nodes[2], 1000, false)
            .await
            .unwrap();
        all_sent.insert((2, payment2.payment_hash, payment2.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment2.payment_hash);
        node_2_ch1_sent_amount += 1000;
        node1_got_amount += 1000;

        let payment3 = nodes[1]
            .send_payment_keysend(&nodes[0], 1000, false)
            .await
            .unwrap();
        all_sent.insert((2, payment3.payment_hash, payment3.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment3.payment_hash);
        node_2_ch2_sent_amount += 1000;
        node3_got_amount += 1000;

        let payment4 = nodes[2]
            .send_payment_keysend(&nodes[0], 1000, false)
            .await
            .unwrap();
        all_sent.insert((3, payment4.payment_hash, payment4.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment4.payment_hash);
        assert!(payment4.fee > 0);
        node_3_sent_fee += payment4.fee;
        node_2_got_fee += payment4.fee;
    }

    loop {
        for (node_index, payment_hash, fee) in all_sent.clone().iter() {
            nodes[*node_index - 1]
                .wait_until_success(*payment_hash)
                .await;
            all_sent.remove(&(*node_index, *payment_hash, *fee));
        }
        let res = nodes[0].node_info().await;
        eprintln!("node1 node_info: {:?}", res);
        let res = nodes[1].node_info().await;
        eprintln!("node2 node_info: {:?}", res);
        let res = nodes[2].node_info().await;
        eprintln!("node3 node_info: {:?}", res);
        if all_sent.is_empty() {
            break;
        }
    }

    eprintln!("node_2_got_fee: {}", node_2_got_fee);
    eprintln!("node1_got_amount: {}", node1_got_amount);
    eprintln!("node3_got_amount: {}", node3_got_amount);

    // node1: sent 4 fee to node2, got 4000 from node2
    // node3: sent 4 fee to node2, got 4000 from node2
    // node2: got 8 from node1 and node3, sent 8000 to node1 and node3

    let node_1_amount = nodes[0].get_local_balance_from_channel(channels[0]);
    let node_2_chnnale1_amount = nodes[1].get_local_balance_from_channel(channels[0]);
    let node_2_chnnale2_amount = nodes[1].get_local_balance_from_channel(channels[1]);
    let node_3_amount = nodes[2].get_local_balance_from_channel(channels[1]);

    let node_1_amount_diff = node_1_amount - old_node_1_amount;
    let node_2_chnnale1_amount_diff = old_node_2_chnnale1_amount - node_2_chnnale1_amount;
    let node_2_chnnale2_amount_diff = old_node_2_chnnale2_amount - node_2_chnnale2_amount;
    let node_3_amount_diff = node_3_amount - old_node_3_amount;

    assert_eq!(node_1_amount_diff, node1_got_amount - node_1_sent_fee);
    // got 3996

    assert_eq!(
        node_2_chnnale1_amount_diff,
        node_2_ch1_sent_amount - node_1_sent_fee
    );
    // sent 3996

    assert_eq!(
        node_2_chnnale2_amount_diff,
        node_2_ch2_sent_amount - node_3_sent_fee
    );
    // sent 3996

    assert_eq!(node_3_amount_diff, node3_got_amount - node_3_sent_fee);
    // got 3996
}

#[tokio::test]
async fn test_send_payment_middle_hop_stopped() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        5,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3, mut node_4] = nodes.try_into().expect("5 nodes");

    // dry run node_0 -> node_3 will select  0 -> 4 -> 3
    let res = node_0
        .send_payment_keysend(&node_3, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 1);

    // node_4 stopped
    node_4.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // when node_4 stopped, node 0 learned that channel 0 -> 4 was not available
    // so it will try another path 0 -> 1 -> 2 -> 3
    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 3);

    node_0.wait_until_success(res.payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_middle_hop_stopped_retry_longer_path() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((5, 6), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((6, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        7,
    )
    .await;
    let [node_0, _node_1, mut node_2, mut node_3, _node_4, _node_5, _node_6] =
        nodes.try_into().expect("7 nodes");

    // dry run node_0 -> node_3 will select  0 -> 1 -> 2 -> 3
    let res = node_0
        .send_payment_keysend(&node_3, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 3);
    node_0.expect_router_used_channel(&res, channels[1]).await;

    // node_2 stopped
    node_2.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let res = node_0
        .send_payment_keysend(&node_3, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    // when node_2 stopped, the first try path is still 0 -> 1 -> 2 -> 3
    // so the fee is 3
    assert_eq!(res.fee, 3);
    node_0.expect_router_used_channel(&res, channels[1]).await;

    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 3);

    node_0.wait_until_success(res.payment_hash).await;

    let payment = node_0.get_payment_result(res.payment_hash).await;
    eprintln!("payment: {:?}", payment);

    // payment success with a longer path 0 -> 4 -> 5 -> 6 -> 3
    assert_eq!(payment.fee, 5);
    node_0
        .expect_payment_used_channel(res.payment_hash, channels[5])
        .await;

    // node_3 stopped, payment will fail
    node_3.stop().await;
    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();

    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 5);

    node_0.wait_until_failed(res.payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_max_value_in_flight_in_first_hop() {
    // https://github.com/nervosnetwork/fiber/issues/450

    init_tracing();

    let nodes = NetworkNode::new_interconnected_nodes(2, None).await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let (_channel_id, _funding_tx_hash) = {
        establish_channel_between_nodes(
            &mut node_0,
            &mut node_1,
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                a_max_tlc_value_in_flight: Some(100000000),
                ..Default::default()
            },
        )
        .await
    };

    let res = node_0
        .send_payment_keysend(&node_1, 100000000 + 1, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 0);

    let payment_hash = res.payment_hash;
    node_0.wait_until_failed(payment_hash).await;

    // now we can not send payment with amount 100000000 + 1 with dry_run
    // since there is already payment history data
    let res = node_0
        .send_payment_keysend(&node_1, 100000000 + 1, true)
        .await;
    eprintln!("res: {:?}", res);
    assert!(res.unwrap_err().to_string().contains("no path found"));

    // if we build a nother channel with higher max_value_in_flight
    // we can send payment with amount 100000000 + 1 with this new channel
    let (channel_id, _funding_tx_hash) = {
        establish_channel_between_nodes(
            &mut node_0,
            &mut node_1,
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                a_max_tlc_value_in_flight: Some(100000000 + 2),
                ..Default::default()
            },
        )
        .await
    };

    let res = node_0
        .send_payment_keysend(&node_1, 100000000 + 1, false)
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .expect_payment_used_channel(payment_hash, channel_id)
        .await;
}

#[tokio::test]
async fn test_send_payment_target_hop_stopped() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        5,
    )
    .await;
    let [node_0, _node_1, _node_2, _node_3, mut node_4] = nodes.try_into().expect("5 nodes");

    // dry run node_0 -> node_4 will select  0 -> 1 -> 2 -> 3 -> 4
    let res = node_0
        .send_payment_keysend(&node_4, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 5);

    // node_4 stopped
    node_4.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let res = node_0
        .send_payment_keysend(&node_4, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    // when node_4 stopped, the first try path is still 0 -> 1 -> 2 -> 3 -> 4
    // so the fee is 5
    assert_eq!(res.fee, 5);

    node_0.wait_until_failed(res.payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_middle_hop_balance_is_not_enough() {
    // https://github.com/nervosnetwork/fiber/issues/286
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (MIN_RESERVED_CKB, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("3 nodes");

    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);

    // path is still 0 -> 1 -> 2 -> 3,
    // 2 -> 3 don't have enough balance
    node_0.wait_until_failed(res.payment_hash).await;
    let result = node_0.get_payment_result(res.payment_hash).await;
    assert!(result
        .failed_error
        .expect("got error")
        .contains("PathFind error: no path found"));
}

#[tokio::test]
async fn test_send_payment_middle_hop_update_fee_send_payment_failed() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    // node_2 update fee rate to a higher one, so the payment will fail
    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    let payment_hash = res.payment_hash;

    node_2
        .update_channel_with_command(
            channels[2],
            UpdateCommand {
                enabled: None,
                tlc_expiry_delta: None,
                tlc_minimum_value: None,
                tlc_fee_proportional_millionths: Some(100000),
            },
        )
        .await;

    node_0.wait_until_failed(payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_middle_hop_update_fee_multiple_payments() {
    // https://github.com/nervosnetwork/fiber/issues/480
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();

    for _i in 0..5 {
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], 1000, false)
            .await
            .unwrap();
        all_sent.insert(res.payment_hash);
    }

    nodes[2]
        .update_channel_with_command(
            channels[2],
            UpdateCommand {
                enabled: None,
                tlc_expiry_delta: None,
                tlc_minimum_value: None,
                tlc_fee_proportional_millionths: Some(100000),
            },
        )
        .await;

    loop {
        for i in 0..4 {
            assert!(nodes[i].get_triggered_unexpected_events().await.is_empty());
        }

        for payment_hash in all_sent.clone().iter() {
            nodes[0].wait_until_final_status(*payment_hash).await;
            let status = nodes[0].get_payment_status(*payment_hash).await;
            //eprintln!("got payment: {:?} status: {:?}", payment_hash, status);
            if status == PaymentStatus::Failed || status == PaymentStatus::Success {
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                all_sent.remove(payment_hash);
            }
        }
        if all_sent.is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn test_send_payment_middle_hop_update_fee_should_recovery() {
    // a variant test from
    // https://github.com/nervosnetwork/fiber/issues/480
    // in this test, we will make sure the payment should recovery after the fee is updated by the middle hop
    // there are two channels between node_1 and node_2, they are with the same fee rate
    // path finding will pick the channel with latest time, so channels[2] will be picked
    // but we will update the fee rate of channels[2] to a higher one
    // so the payment will fail, but after the payment failed, the path finding should pick the channels[1] in the next try
    // in the end, all the payments should success
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let mut all_sent = HashSet::new();

    let tx_count = 6;
    for _i in 0..tx_count {
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], 1000, false)
            .await
            .unwrap();
        all_sent.insert(res.payment_hash);
    }

    nodes[1]
        .update_channel_with_command(
            channels[2],
            UpdateCommand {
                enabled: None,
                tlc_expiry_delta: None,
                tlc_minimum_value: None,
                tlc_fee_proportional_millionths: Some(100000),
            },
        )
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let mut succ_count = 0;
    loop {
        for i in 0..4 {
            assert!(nodes[i].get_triggered_unexpected_events().await.is_empty());
        }

        for payment_hash in all_sent.clone().iter() {
            nodes[0].wait_until_final_status(*payment_hash).await;
            let status = nodes[0].get_payment_status(*payment_hash).await;
            if status == PaymentStatus::Success || status == PaymentStatus::Failed {
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                all_sent.remove(payment_hash);
                if status == PaymentStatus::Success {
                    succ_count += 1;
                }
            }
        }
        if all_sent.is_empty() {
            break;
        }
    }

    assert_eq!(succ_count, tx_count);
    let channel_state = nodes[0].get_channel_actor_state(channels[0]);
    assert_eq!(channel_state.get_offered_tlc_balance(), 0);
}

async fn run_complex_network_with_params(
    funding_amount: u128,
    payment_amount_gen: impl Fn() -> u128,
) -> Vec<(Hash256, PaymentStatus)> {
    init_tracing();

    let nodes_num = 6;
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            ((3, 4), (funding_amount, funding_amount)),
            ((4, 5), (funding_amount, funding_amount)),
            ((0, 3), (funding_amount, funding_amount)),
            ((1, 4), (funding_amount, funding_amount)),
            ((2, 5), (funding_amount, funding_amount)),
        ],
        nodes_num,
    )
    .await;

    let mut all_sent = HashSet::new();
    for _k in 0..2 {
        for i in 0..nodes_num {
            let payment_amount = payment_amount_gen();
            let res = nodes[i]
                .send_payment_keysend_to_self(payment_amount, false)
                .await;
            if let Ok(res) = res {
                let payment_hash = res.payment_hash;
                all_sent.insert((i, payment_hash));
            }
        }
    }

    let mut result = vec![];
    loop {
        for i in 0..nodes_num {
            let unexpected_events = nodes[i].get_triggered_unexpected_events().await;
            if !unexpected_events.is_empty() {
                eprintln!("node_{} got unexpected events: {:?}", i, unexpected_events);
                unreachable!("unexpected events");
            }
        }

        for (i, payment_hash) in all_sent.clone().into_iter() {
            nodes[i].wait_until_final_status(payment_hash).await;
            let status = nodes[i].get_payment_status(payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if matches!(status, PaymentStatus::Success | PaymentStatus::Failed) {
                result.push((payment_hash, status));
                all_sent.remove(&(i, payment_hash));
            }
        }
        if all_sent.is_empty() {
            break;
        }
    }

    // make sure all the channels are still workable with small accounts
    for i in 0..nodes_num {
        if let Ok(res) = nodes[i].send_payment_keysend_to_self(500, false).await {
            nodes[i].wait_until_success(res.payment_hash).await;
        }
    }

    result
}

#[tokio::test]
async fn test_send_payment_self_with_two_nodes() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding_amount, funding_amount)),
            ((1, 0), (funding_amount, funding_amount)),
        ],
        2,
    )
    .await;

    let old_balance0 = nodes[0].get_local_balance_from_channel(channels[0]);
    let old_balance1 = nodes[0].get_local_balance_from_channel(channels[1]);
    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    nodes[0].wait_until_success(payment_hash).await;
    let balance0 = nodes[0].get_local_balance_from_channel(channels[0]);
    let balance1 = nodes[0].get_local_balance_from_channel(channels[1]);

    eprintln!("old_balance: {}, new_balance: {}", old_balance0, balance0);
    eprintln!("old_balance1: {}, new_balance1: {}", old_balance1, balance1);
    let fee = old_balance0 + old_balance1 - balance0 - balance1;
    assert_eq!(fee, 1);

    // single edge network payself will fail
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (funding_amount, funding_amount))], 2).await;
    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn test_send_payment_self_with_mixed_channel() {
    // #678, payself with mixed channel got wrong
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_err());

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_ok());
    nodes[0].wait_until_success(res.unwrap().payment_hash).await;

    // all UDT channels
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let _res = nodes[0]
        .assert_send_payment_success(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            udt_type_script: Some(Script::default()),
            ..Default::default()
        })
        .await;
}

#[tokio::test]
async fn test_send_payment_with_invalid_tlc_expiry() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[(
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: funding_amount,
                node_b_funding_amount: funding_amount,
                ..Default::default()
            },
        )],
        2,
        None,
    )
    .await;

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[1].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            tlc_expiry_limit: Some(10), // too small than MIN_TLC_EXPIRY_DELTA
            ..Default::default()
        })
        .await;
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("tlc_expiry_limit is too small"));

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[1].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            tlc_expiry_limit: Some(MIN_TLC_EXPIRY_DELTA + 1), // still too small
            ..Default::default()
        })
        .await;
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("tlc_expiry_limit is too small"));

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[1].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            tlc_expiry_limit: Some(DEFAULT_FINAL_TLC_EXPIRY_DELTA + 1), // Ok now
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    nodes[0].wait_until_success(res.unwrap().payment_hash).await;
}

#[tokio::test]
async fn test_send_payself_with_invalid_tlc_expiry() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    b_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a too large value
                    ..Default::default()
                },
            ),
            (
                (1, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a too large value
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    // no tlc_expiry_limit will also fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            tlc_expiry_limit: Some(DEFAULT_FINAL_TLC_EXPIRY_DELTA),
            ..Default::default()
        })
        .await;

    assert!(res.unwrap_err().to_string().contains("no path found"));
}

#[tokio::test]
async fn test_send_payself_with_single_limit_tlc_expiry() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
            (
                (1, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a large value
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            tlc_expiry_limit: Some(MAX_PAYMENT_TLC_EXPIRY_LIMIT),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_send_payself_with_small_min_tlc_value() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    b_tlc_min_value: Some(100), // a small value
                    ..Default::default()
                },
            ),
            (
                (1, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_min_value: Some(100), // a small value
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    // too small amount will fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(99),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;

    assert!(res.unwrap_err().to_string().contains("no path found"));

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(100),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
}

#[tokio::test]
async fn test_send_payment_with_middle_hop_with_min_tlc_value() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_min_value: Some(100),
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_min_value: Some(50),
                    ..Default::default()
                },
            ),
        ],
        3,
        None,
    )
    .await;

    // too small amount will fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[2].pubkey),
            amount: Some(40),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    // too small amount will fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[2].pubkey),
            amount: Some(60),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    // normal amount will success
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[2].pubkey),
            amount: Some(110),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_send_payment_complex_network_payself_all_succeed() {
    // from issue 475
    // channel amount is enough, so all payments should success
    let res = run_complex_network_with_params(MIN_RESERVED_CKB + 100000000, || 1000).await;
    let failed_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentStatus::Failed)
        .count();

    assert_eq!(failed_count, 0);
}

#[tokio::test]
async fn test_send_payment_complex_network_payself_amount_exceeded() {
    // variant from issue 475
    // the channel amount is not enough, so payments maybe be failed
    let ckb_unit = 100_000_000;
    let res = run_complex_network_with_params(MIN_RESERVED_CKB + 1000 * ckb_unit, || {
        (400_u128 + (rand::random::<u64>() % 100) as u128) * ckb_unit
    })
    .await;

    // some may failed and some may success
    let failed_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentStatus::Failed)
        .count();
    assert!(failed_count > 0);
    let succ_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentStatus::Success)
        .count();
    assert!(succ_count > 0);
}

#[tokio::test]
async fn test_send_payment_with_one_node_stop() {
    // make sure part of the payments will fail, since the node is stopped
    // TLC forwarding will fail and proper error will be returned
    // There is also a probability that RemoveTlc can not be passed backwardly,
    // since the node is stopped, so the payment will be Inflight state.
    init_tracing();

    let (mut nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = nodes[0].send_payment_keysend(&nodes[3], 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = nodes[3].stop().await;
        }
    }

    let mut failed_count = 0;
    let mut check_count = 0;
    while check_count < 100 {
        for payment_hash in all_sent.clone().iter() {
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!("payment_hash: {:?} status: {:?}", payment_hash, res.status);
            if res.status == PaymentStatus::Failed {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }
        }
        check_count += 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if all_sent.is_empty() {
            break;
        }
    }
    assert!(failed_count >= 4);
}

#[tokio::test]
async fn test_send_payment_shutdown_with_force() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = node_0.send_payment_keysend(&node_3, 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = node_3.send_shutdown(channels[2], true).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            node_3
                .send_channel_shutdown_tx_confirmed_event(node_2.peer_id.clone(), channels[2], true)
                .await;
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let channel_actor_state = node_3.get_channel_actor_state(channels[2]);
            assert_eq!(
                channel_actor_state.state,
                ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL)
            );
        }
    }

    // make sure the later payments will fail
    // because network actor will find out the inactive channels and shutdown channel forcefully
    let mut wait_time = 0;
    while wait_time < PEER_CHANNEL_RESPONSE_TIMEOUT + 3 {
        let channel_state = node_2.get_channel_actor_state(channels[2]);
        if channel_state.state == ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL) {
            break;
        } else {
            assert_eq!(channel_state.state, ChannelState::ChannelReady);
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
            wait_time += 1000;
        }
    }
}

#[tokio::test]
async fn test_send_payment_shutdown_channel_actor_may_already_stopped() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    for i in 0..2 {
        let _ = nodes[i].send_shutdown(channels[i], true).await;

        // send multiple shutdown transaction confirmed events
        for _k in 0..5 {
            nodes[i]
                .send_channel_shutdown_tx_confirmed_event(
                    nodes[i + 1].peer_id.clone(),
                    channels[i],
                    true,
                )
                .await;
        }
        let channel_actor_state = nodes[i].get_channel_actor_state(channels[i]);
        assert_eq!(
            channel_actor_state.state,
            ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL)
        );
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
}

#[tokio::test]
async fn test_send_payment_shutdown_cooperative() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = nodes[0].send_payment_keysend(&nodes[3], 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = nodes[3].send_shutdown(channels[2], false).await;
        }
    }

    let mut failed_count = 0;
    let all_tx_count = all_sent.len();
    while !all_sent.is_empty() {
        for payment_hash in all_sent.clone().iter() {
            nodes[0].wait_until_final_status(*payment_hash).await;
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!(
                "payment_hash: {:?} status: {:?} failed_count: {:?}",
                payment_hash, res.status, failed_count
            );
            if res.status == PaymentStatus::Failed || res.status == PaymentStatus::Success {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }
        }
    }
    assert_eq!(failed_count, all_tx_count);

    loop {
        let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
        eprintln!(
            "node_3_channel_actor_state: {:?}",
            node_3_channel_actor_state.state
        );
        let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
        eprintln!(
            "node_2_channel_actor_state: {:?}",
            node_2_channel_actor_state.state
        );
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_cooperative_sender_sent() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let old_node0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let old_node3_balance = node_3.get_local_balance_from_channel(channels[2]);

    let mut all_sent = HashSet::new();
    let tlc_amount = 1000;
    for _i in 0..4 {
        let res = node_0
            .send_payment_keysend(&node_3, tlc_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            all_sent.insert(send_payment_res.payment_hash);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;
    for _i in 0..100 {
        let node_3_channel_actor_state = node_3.get_channel_actor_state(channels[2]);
        if node_3_channel_actor_state
            .tlc_state
            .all_tlcs()
            .next()
            .is_none()
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    loop {
        let res = node_2.send_shutdown(channels[2], false).await;
        if res.is_ok() {
            debug!("send shutdown successfully");
            break;
        }
    }

    let mut succ_count = 0;
    let all_tx_count = all_sent.len();
    let mut count = 0;
    while !all_sent.is_empty() && count < 100 {
        for payment_hash in all_sent.clone().iter() {
            node_0.wait_until_final_status(*payment_hash).await;
            let res = node_0.get_payment_result(*payment_hash).await;
            eprintln!("payment_hash: {:?} status: {:?}", payment_hash, res.status);

            if res.status == PaymentStatus::Success {
                succ_count += 1;
                all_sent.remove(payment_hash);
            }
        }
        count += 1;
    }
    debug!("all_count: {:?} succ_count: {:?}", all_tx_count, succ_count);

    for _i in 0..100 {
        let node_3_channel_actor_state = node_3.get_channel_actor_state(channels[2]);
        eprintln!(
            "node_3_channel_actor_state: {:?}",
            node_3_channel_actor_state.state
        );
        let node_2_channel_actor_state = node_2.get_channel_actor_state(channels[2]);
        eprintln!(
            "node_2_channel_actor_state: {:?}",
            node_2_channel_actor_state.state
        );
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    let node_3_channel_actor_state = node_3.get_channel_actor_state(channels[2]);
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
    let node_2_channel_actor_state = node_2.get_channel_actor_state(channels[2]);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    let new_node0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let new_node3_balance = node_3.get_local_balance_from_channel(channels[2]);
    debug!(
        "node0 send: {} - {} = {}",
        old_node0_balance,
        new_node0_balance,
        old_node0_balance - new_node0_balance
    );
    debug!(
        "node3 recv: {} + {} = {}",
        old_node3_balance,
        new_node3_balance - old_node3_balance,
        new_node3_balance
    );

    assert_eq!(
        old_node0_balance - new_node0_balance,
        (tlc_amount + 3) * succ_count
    );
    assert_eq!(
        new_node3_balance - old_node3_balance,
        tlc_amount * succ_count
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_under_send_each_other() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();
    for _i in 0..5 {
        let rand_amount = 1 + (rand::random::<u64>() % 1000) as u128;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], rand_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            all_sent.insert(send_payment_res.payment_hash);
        }
        let rand_amount = 1 + (rand::random::<u64>() % 1000) as u128;
        let res = nodes[3]
            .send_payment_keysend(&nodes[0], rand_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            all_sent.insert(send_payment_res.payment_hash);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    for _i in 0..20 {
        let res = nodes[3].send_shutdown(channels[2], false).await;
        if res.is_ok() {
            debug!("send shutdown successfully");
            break;
        }
        debug!("shutdown res: {:?}", res);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    for i in 0..30 {
        assert!(nodes[2].get_triggered_unexpected_events().await.is_empty());
        assert!(nodes[3].get_triggered_unexpected_events().await.is_empty());

        let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
        eprintln!(
            "checking {}: node_2_channel_actor_state: {:?} tlc_pending:\n",
            i, node_2_channel_actor_state.state,
        );
        node_2_channel_actor_state.tlc_state.debug();

        let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
        eprintln!(
            "checking { }: node_3_channel_actor_state: {:?} tlc_pending:\n",
            i, node_3_channel_actor_state.state,
        );
        node_3_channel_actor_state.tlc_state.debug();
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    wait_until(|| {
        matches!(
            nodes[3].get_channel_actor_state(channels[2]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    wait_until(|| {
        matches!(
            nodes[2].get_channel_actor_state(channels[2]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

async fn run_shutdown_with_payment_send(sender: usize, receiver: usize) {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let mut node0_sent_payments = HashSet::new();
    for _i in 0..5 {
        let rand_amount = 1 + (rand::random::<u64>() % 1000) as u128;
        let res = nodes[sender]
            .send_payment_keysend(&nodes[receiver], rand_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            node0_sent_payments.insert(send_payment_res.payment_hash);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let _ = nodes[2].send_shutdown(channels[1], false).await;

    // there will be no pending tlcs
    for i in 0..10 {
        assert!(nodes[1].get_triggered_unexpected_events().await.is_empty());
        assert!(nodes[2].get_triggered_unexpected_events().await.is_empty());

        let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
        eprintln!(
            "checking {}: node_1_channel_actor_state: {:?} tlc_pending:\n",
            i, node_1_channel_actor_state.state,
        );
        node_1_channel_actor_state.tlc_state.debug();

        let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[1]);
        eprintln!(
            "checking { }: node_2_channel_actor_state: {:?} tlc_pending:\n",
            i, node_2_channel_actor_state.state,
        );
        node_2_channel_actor_state.tlc_state.debug();
        if !node_1_channel_actor_state.any_tlc_pending()
            && !node_2_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    wait_until(|| {
        matches!(
            nodes[1].get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
    error!("node_1 state: {:?}", node_1_channel_actor_state.state);
    assert_eq!(
        node_1_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    wait_until(|| {
        matches!(
            nodes[2].get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[1]);
    error!("node_2 state: {:?}", node_2_channel_actor_state.state);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_under_single_direction_send() {
    run_shutdown_with_payment_send(1, 2).await;
}

#[tokio::test]
async fn test_shutdown_with_pending_tlc() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;

    // create a new payment hash
    let preimage: [u8; 32] = gen_rand_sha256_hash().as_ref().try_into().unwrap();

    let hash_algorithm = HashAlgorithm::Sha256;
    let digest = hash_algorithm.hash(preimage);
    let add_tlc_result = call!(nodes[0].network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 1000,
                        hash_algorithm,
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive");
    assert!(add_tlc_result.is_ok());
    let res = nodes[0].send_shutdown(channels[0], false).await;
    assert!(res.is_err());

    let res = nodes[1].send_shutdown(channels[0], false).await;
    assert!(res.is_ok());

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_0_channel_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    assert!(node_0_channel_actor_state.any_tlc_pending());

    assert!(matches!(
        node_0_channel_actor_state.state,
        ChannelState::ShuttingDown(ShuttingDownFlags::AWAITING_PENDING_TLCS)
    ));
    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[0]);
    assert!(node_1_channel_actor_state.any_tlc_pending());
    assert!(matches!(
        node_1_channel_actor_state.state,
        ChannelState::ShuttingDown(ShuttingDownFlags::AWAITING_PENDING_TLCS)
    ));

    let remove_tlc_result = call!(nodes[1].network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result.unwrap().tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive");
    assert!(remove_tlc_result.is_ok());

    wait_until(|| {
        matches!(
            nodes[0].get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(..)
        )
    })
    .await;
    let node_0_channel_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    assert_eq!(
        node_0_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    wait_until(|| {
        matches!(
            nodes[1].get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[0]);
    assert_eq!(
        node_1_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

#[tokio::test]
async fn test_send_payment_middle_hop_restart_will_be_ok() {
    async fn inner_run_restart_test(restart_node_index: usize) {
        init_tracing();

        let funding_amount = MIN_RESERVED_CKB + 1000 * 100_000_000;
        let (mut nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (funding_amount, funding_amount)),
                ((1, 2), (funding_amount, funding_amount)),
                ((2, 3), (funding_amount, funding_amount)),
            ],
            4,
        )
        .await;

        let payment_amount = 10 * 100_000_000;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();

        let payment_hash = res.payment_hash;

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Success);

        nodes[restart_node_index].restart().await;

        // wait for the node to be ready after reestablish channel
        tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();
        let payment_hash = res.payment_hash;
        eprintln!("res: {:?}", payment_hash);

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Success);
    }
    for restart_index in 1..=3 {
        let _ = inner_run_restart_test(restart_index).await;
    }
}

#[tokio::test]
async fn test_send_payment_middle_hop_stop_send_payment_then_start() {
    async fn inner_run_restart_test(restart_node_index: usize) {
        init_tracing();

        let funding_amount = MIN_RESERVED_CKB + 1000 * 100_000_000;
        let (mut nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (funding_amount, funding_amount)),
                ((1, 2), (funding_amount, funding_amount)),
                ((2, 3), (funding_amount, funding_amount)),
            ],
            4,
        )
        .await;

        let payment_amount = 10 * 100_000_000;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();

        let payment_hash = res.payment_hash;

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Success);

        nodes[restart_node_index].stop().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();
        let payment_hash = res.payment_hash;
        eprintln!("res: {:?}", payment_hash);

        nodes[0].wait_until_failed(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Failed);

        tokio::time::sleep(tokio::time::Duration::from_millis(4 * 1000)).await;

        // now we start nodes[2], expect the payment will success
        nodes[restart_node_index].start().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

        // after node reconnect, there will be new channel_update, and payment history will
        // process it to clear the old fail records, with time passed, we can send payment with larger amount
        let mut count = 0;
        loop {
            let res = nodes[0]
                .send_payment_keysend(&nodes[3], payment_amount, true)
                .await;

            if res.is_ok() {
                break;
            } else {
                count += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                eprintln!("retry to wait amount increasing: {:?}", count);
            }
        }
    }

    let _ = inner_run_restart_test(2).await;
    let _ = inner_run_restart_test(3).await;
}

#[tokio::test]
async fn test_send_payment_sync_up_new_channel_is_added() {
    init_tracing();

    // create a network with 4 nodes, but only connect with 2 channels
    // node0 -> node1 -> node2  node3
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [mut node_0, mut node_1, mut node_2, mut node_3] = nodes.try_into().expect("4 nodes");

    let payment_amount = 10 * 100_000_000;
    let res = node_0
        .send_payment_keysend(&node_3, payment_amount, true)
        .await;

    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Failed to build route"));

    // now add channel for node_2 and node_3
    let (channel_id, funding_tx_hash) = {
        establish_channel_between_nodes(
            &mut node_2,
            &mut node_3,
            ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
        )
        .await
    };
    let funding_tx = node_2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    // all the other nodes submit_tx
    for node in [&mut node_0, &mut node_1].into_iter() {
        let res = node.submit_tx(funding_tx.clone()).await;
        assert!(matches!(res, TxStatus::Committed(..)));
        node.add_channel_tx(channel_id, funding_tx_hash);
        wait_for_network_graph_update(node, 3).await;
    }

    let res = node_0
        .send_payment_keysend(&node_3, payment_amount, false)
        .await;

    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
// This test is not stable and may fail randomly, so we ignore it for now.
// The root cause is `assert!(node_0.get_triggered_unexpected_events().await.is_empty())` may fail
#[ignore]
async fn test_send_payment_remove_tlc_with_preimage_will_retry() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [mut node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    let mut payments = HashSet::new();

    for i in 0..10 {
        if i % 2 == 0 {
            let amount = rand::random::<u128>() % 1000 + 1;
            let res = node_0
                .send_payment_keysend(&node_2, amount, false)
                .await
                .unwrap();
            payments.insert(res.payment_hash);
            node_0.wait_until_inflight(res.payment_hash).await;
        } else {
            let amount = rand::random::<u128>() % 1000 + 1;
            let res = node_2
                .send_payment_keysend(&node_0, amount, false)
                .await
                .unwrap();
            node_2.wait_until_created(res.payment_hash).await;
        }
    }

    let node1_id = node_1.peer_id.clone();
    let node0_id = node_0.peer_id.clone();
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(node1_id.clone(), PeerDisconnectReason::Requested),
        ))
        .expect("node_a alive");

    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node0_id);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // reconnect node_0 and node_1
    node_0.connect_to_nonblocking(&node_1).await;

    // the CheckChannels in network actor will continue to retry RemoveTlc for tlc already with preimage
    // so all the payments should be succeeded after all
    let started = SystemTime::now();

    loop {
        for payment_hash in payments.clone().iter() {
            assert!(node_0.get_triggered_unexpected_events().await.is_empty());
            assert!(node_1.get_triggered_unexpected_events().await.is_empty());
            assert!(node_2.get_triggered_unexpected_events().await.is_empty());

            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentStatus::Success {
                payments.remove(payment_hash);
            }
        }
        if payments.is_empty() {
            break;
        }
        let elapsed = SystemTime::now()
            .duration_since(started)
            .expect("time passed")
            .as_secs();
        if elapsed > 50 {
            let node0_state = node_0.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node_0_state:", node_0.get_peer_id());
            node0_state.tlc_state.debug();

            let node1_state = node_1.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node1_left_actor_state:", node_1.get_peer_id());
            node1_state.tlc_state.debug();

            let node1_right_state = node_1.get_channel_actor_state(channels[1]);
            eprintln!("peer {:?} node1_right_actor_state:", node_1.get_peer_id());
            node1_right_state.tlc_state.debug();

            let node2_state = node_2.get_channel_actor_state(channels[1]);
            eprintln!("peer {:?} node_2_state:", node_2.get_peer_id());
            node2_state.tlc_state.debug();

            panic!("timeout");
        }
    }
}

#[tokio::test]
#[ignore]
// FIXME: there is a bug in reestablishing channel.
async fn test_send_payment_send_each_other_reestablishing() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let mut payments = HashSet::new();

    for i in 0..20 {
        if i % 2 == 0 {
            let amount = rand::random::<u128>() % 1000 + 1;
            let res = node_0
                .send_payment_keysend(&node_1, amount, false)
                .await
                .unwrap();
            payments.insert(res.payment_hash);
        } else {
            let amount = rand::random::<u128>() % 1000 + 1;
            let _res = node_1
                .send_payment_keysend(&node_0, amount, false)
                .await
                .unwrap();
        }
    }

    let node1_id = node_1.peer_id.clone();
    let node0_id = node_0.peer_id.clone();
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(node1_id.clone(), PeerDisconnectReason::Requested),
        ))
        .expect("node_a alive");

    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node0_id);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // reconnect node_0 and node_1
    node_0.connect_to_nonblocking(&node_1).await;

    // the CheckChannels in network actor will continue to retry RemoveTlc for tlc already with preimage
    // so all the payments should be succeeded after all
    let started = SystemTime::now();

    loop {
        for payment_hash in payments.clone().iter() {
            assert!(node_0.get_triggered_unexpected_events().await.is_empty());
            assert!(node_1.get_triggered_unexpected_events().await.is_empty());

            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentStatus::Success || status == PaymentStatus::Failed {
                payments.remove(payment_hash);
            }
        }
        if payments.is_empty() {
            break;
        }
        let elapsed = SystemTime::now()
            .duration_since(started)
            .expect("time passed")
            .as_secs();
        if elapsed > 50 {
            let node0_state = node_0.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node_0_state:", node_0.get_peer_id());
            node0_state.tlc_state.debug();

            let node1_state = node_1.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node1_left_actor_state:", node_1.get_peer_id());
            node1_state.tlc_state.debug();

            let node1_right_state = node_1.get_channel_actor_state(channels[1]);
            eprintln!("peer {:?} node1_right_actor_state:", node_1.get_peer_id());
            node1_right_state.tlc_state.debug();

            panic!("timeout");
        }
    }
}

#[tokio::test]
async fn test_send_payment_invoice_cancel_multiple_ops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, _node_2] = nodes.try_into().expect("4 nodes");

    let mut payments = HashSet::new();
    let mut invoices: Vec<CkbInvoice> = vec![];

    let target_pubkey = node_0.pubkey;
    let count = 10;
    for _i in 0..count {
        let preimage = gen_rand_sha256_hash();
        let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(100))
            .payment_preimage(preimage)
            .payee_pub_key(target_pubkey.into())
            .build()
            .expect("build invoice success");

        node_0.insert_invoice(ckb_invoice.clone(), Some(preimage));
        invoices.push(ckb_invoice);
    }

    for i in 0..count {
        let invoice = &invoices[i];

        node_0.cancel_invoice(invoice.payment_hash());
        let res = node_0
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                amount: invoice.amount,
                allow_self_payment: true,
                ..Default::default()
            })
            .await
            .unwrap();
        payments.insert(res.payment_hash);
        node_0.wait_until_created(res.payment_hash).await;
    }

    loop {
        for payment_hash in payments.clone().iter() {
            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentStatus::Failed {
                payments.remove(payment_hash);
            }
            assert_ne!(status, PaymentStatus::Success);
        }
        if payments.is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn test_send_payment_no_preimage_invoice_will_make_payment_failed() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("4 nodes");

    let mut payments = HashSet::new();
    let mut invoices: Vec<CkbInvoice> = vec![];

    let count = 2;
    let target_pubkey = node_1.pubkey;
    // Note: the preimages are not stored in db
    for _i in 0..count {
        let preimage = gen_rand_sha256_hash();
        let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(100))
            .payment_preimage(preimage)
            .payee_pub_key(target_pubkey.into())
            .build()
            .expect("build invoice success");

        invoices.push(ckb_invoice);
    }

    for i in 0..count {
        let invoice = &invoices[i];

        let res = node_0
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                amount: invoice.amount,
                target_pubkey: None,
                allow_self_payment: true,
                payment_hash: None,
                final_tlc_expiry_delta: None,
                tlc_expiry_limit: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: None,
                atomic_mpp: None,
                udt_type_script: None,
                dry_run: false,
                hop_hints: None,
                custom_records: None,
            })
            .await
            .unwrap();
        payments.insert(res.payment_hash);
        node_0.wait_until_created(res.payment_hash).await;
    }

    for payment_hash in payments.iter() {
        node_0.wait_until_failed(*payment_hash).await;
    }
}

#[tokio::test]
async fn test_send_payment_with_mixed_channel_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node0, mut node1, mut node2] = nodes.try_into().expect("3 nodes");

    // create a private UDT channel with node_1 and node_2
    let (_new_channel_id, funding_tx) = establish_channel_between_nodes(
        &mut node1,
        &mut node2,
        ChannelParameters {
            public: false,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            funding_udt_type_script: Some(Script::default()), // UDT type
            ..Default::default()
        },
    )
    .await;
    let private_udt_channel = OutPoint::new(funding_tx.into(), 0);

    // get a router from node0 -> node2
    let router = node0
        .build_router(BuildRouterCommand {
            amount: Some(1000),
            hops_info: vec![
                HopRequire {
                    pubkey: node1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    let channel0_outpoint = node0.get_channel_outpoint(&channels[0]).unwrap();
    let channel1_outpoint = node1.get_channel_outpoint(&channels[1]).unwrap();
    assert_eq!(router.router_hops[0].channel_outpoint, channel0_outpoint);
    assert_eq!(router.router_hops[1].channel_outpoint, channel1_outpoint);
    let mut copied_router = router.clone();

    // normal payment will succeed
    let res = node0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    eprintln!("res: {:?}", res);
    node0.wait_until_success(res.unwrap().payment_hash).await;

    // now we manually replace the second channel with the UDT channel
    // the payment will failed with proper error code
    copied_router.router_hops[1].channel_outpoint = private_udt_channel;
    let res = node0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: copied_router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    eprintln!("res: {:?}", res);

    let payment_hash = res.unwrap().payment_hash;
    node0.wait_until_failed(payment_hash).await;
    let payment_res = node0.get_payment_result(payment_hash).await;
    eprintln!("payment_res: {:?}", payment_res);
    assert_eq!(
        payment_res.failed_error.unwrap(),
        "IncorrectOrUnknownPaymentDetails"
    );
    let payment_session = node0.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.attempts_count(), 1);
    assert_eq!(payment_session.retry_times(), 1);
}

#[tokio::test]
async fn test_send_payment_with_first_channel_retry_will_be_ok() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 1), (HUGE_CKB_AMOUNT + 500, HUGE_CKB_AMOUNT)),
            ((0, 1), (HUGE_CKB_AMOUNT + 1000, HUGE_CKB_AMOUNT)), // multiple node0 -> node1 channels
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node0, _node1, node2] = nodes.try_into().expect("3 nodes");

    // disable channels[2], which will be the first time choice of send_payment
    node0.disable_channel_stealthy(channels[2]).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let payment = node0
        .send_payment_keysend(&node2, 1000, false)
        .await
        .unwrap();
    node0
        .expect_router_used_channel(&payment, channels[2])
        .await;
    eprintln!("payment: {:?}", payment);
    node0.wait_until_success(payment.payment_hash).await;
    let payment_session = node0.get_payment_session(payment.payment_hash).unwrap();
    for i in 0..=2 {
        let channel_outpoint = node0.get_channel_outpoint(&channels[i]);
        eprintln!("i channel_outpoint: {:?}", channel_outpoint);
    }
    eprintln!("payment_session router: {:?}", payment_session);

    // node0 will succeeded with another channel
    node0
        .expect_payment_used_channel(payment.payment_hash, channels[1])
        .await;
    assert_eq!(payment_session.retry_times(), 2);
}

#[tokio::test]
#[ignore]
async fn test_send_payment_with_reconnect_two_times() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [mut node0, mut node1] = nodes.try_into().expect("2 nodes");

    for _i in 0..2 {
        let mut payments = HashSet::new();
        for _j in 0..5 {
            let res = node0
                .send_payment_keysend(&node1, 1000, false)
                .await
                .unwrap();
            let payment_hash = res.payment_hash;
            payments.insert(payment_hash);
        }

        // disconnect peer
        let node1_id = node1.peer_id.clone();
        let node0_id = node0.peer_id.clone();
        node0
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::DisconnectPeer(
                    node1_id.clone(),
                    PeerDisconnectReason::Requested,
                ),
            ))
            .expect("node_a alive");

        node1
            .expect_event(|event| match event {
                NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                    assert_eq!(peer_id, &node0_id);
                    true
                }
                _ => false,
            })
            .await;

        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

        // reconnect peer
        node0.connect_to_nonblocking(&node1).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        // wait for the payment to be retried
        for _i in 0..20 {
            assert!(node0.get_triggered_unexpected_events().await.is_empty());
            assert!(node1.get_triggered_unexpected_events().await.is_empty());
            for payment_hash in payments.clone().iter() {
                node0.wait_until_final_status(*payment_hash).await;
                let status = node0.get_payment_status(*payment_hash).await;
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                if status == PaymentStatus::Success || status == PaymentStatus::Failed {
                    payments.remove(payment_hash);
                } else if status == PaymentStatus::Created {
                    // wait for the payment to be retried
                    let payment_session = node0.get_payment_session(*payment_hash).unwrap();
                    eprintln!(
                        "payment_session attempts: {:?}",
                        payment_session.attempts_count()
                    );
                }
            }
            if payments.is_empty() {
                break;
            }
        }
        if !payments.is_empty() {
            panic!("some payments are not finished: {:?}", payments);
        }
    }
}

#[tokio::test]
async fn test_send_payment_pending_count_on_find_path() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding_amount, funding_amount)),
            // we build multiple channels between node_1 and node_2
            ((1, 2), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            // node_2 -> node_3
            ((2, 3), (funding_amount, funding_amount)),
        ],
        4,
    )
    .await;

    let mut payments = HashSet::new();
    let mut channel_stats_map = HashMap::new();
    for i in 0..20 {
        let payment_amount = 10;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();

        let payment_hash = res.payment_hash;
        let second_hop_channel = res.routers[0].nodes[1].channel_outpoint.clone();
        channel_stats_map
            .entry(second_hop_channel)
            .and_modify(|e| *e += 1)
            .or_insert(1);

        eprintln!("i: {:?} payment_hash: {:?}", i, payment_hash);
        payments.insert(payment_hash);
    }

    // assert that the path finding tried multiple middle channels
    let mut used_channel_count = 0;
    for channel in &channels[1..channels.len() - 1] {
        let funding_tx = nodes[0].get_channel_funding_tx(channel).unwrap();
        let channel_outpoint = OutPoint::new(funding_tx.into(), 0);

        let tried_count = channel_stats_map.get(&channel_outpoint).unwrap_or(&0);
        debug!(
            "check channel_outpoint: {:?}, count: {:?}",
            channel_outpoint, tried_count
        );
        if *tried_count > 0 {
            used_channel_count += 1;
        }
    }
    assert!(used_channel_count >= 3);
}

#[tokio::test]
async fn test_send_payment_check_router_always_the_right_one() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        6,
    )
    .await;

    let channel1_funding_tx = nodes[0].get_channel_funding_tx(&channels[0]).unwrap();
    let channel1_outpoint = OutPoint::new(channel1_funding_tx.into(), 0);
    let channel2_funding_tx = nodes[1].get_channel_funding_tx(&channels[1]).unwrap();
    let channel2_outpoint = OutPoint::new(channel2_funding_tx.into(), 0);

    let check_router = |router: &SessionRoute| {
        assert_eq!(router.nodes[0].channel_outpoint, channel1_outpoint);
        assert_eq!(router.nodes[1].channel_outpoint, channel2_outpoint);
    };

    for _i in 0..5 {
        let res = nodes[0]
            .send_payment_keysend(&nodes[2], 100, false)
            .await
            .unwrap();
        check_router(&res.routers[0]);
    }

    let res = nodes[0]
        .send_payment_keysend(&nodes[2], 100, false)
        .await
        .unwrap();
    check_router(&res.routers[0]);
}

#[tokio::test]
async fn test_send_payment_with_reverse_channel_of_capaicity_not_enough() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (16 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (17 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            // path finding algorighm will choose this channel firstly,
            // since it has more capacity than the above two channels,
            // but there capacity from 1->2 is not enough for the payment
            // so the first payment will retry two times,
            // and the following payments will only retry once
            ((2, 1), (18 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;

    let node0_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    eprintln!(
        "node_0: {:?} {:?}",
        node0_actor_state.to_local_amount, node0_actor_state.to_remote_amount
    );

    let node1_actor_state = nodes[1].get_channel_actor_state(channels[0]);
    eprintln!(
        "node_1: {:?} {:?}",
        node1_actor_state.to_local_amount, node1_actor_state.to_remote_amount
    );

    let mut payments = HashSet::new();
    let mut statistic = HashMap::new();

    let count = 5;
    for _i in 0..count {
        let payment = nodes[0].send_payment_keysend(&nodes[2], 1, false).await;
        let payment_hash = payment.unwrap().payment_hash;
        nodes[0].wait_until_success(payment_hash).await;
        let session = nodes[0].get_payment_session(payment_hash).unwrap();
        let retry_times = session.retry_times();
        debug!(
            "payment_hash: {:?} retry_times: {:?}",
            payment_hash, retry_times
        );
        statistic
            .entry(retry_times)
            .and_modify(|e| *e += 1)
            .or_insert(1);
        payments.insert(payment_hash);
    }

    // assert only one payment session will try 2 times
    assert_eq!(statistic[&2], 1);
    assert_eq!(statistic[&1], count - 1);
}

#[tokio::test]
#[ignore]
/// this test now can only run with cargo nextest,
/// since it invoiving a global TOKIO_TASK_TRACKER_WITH_CANCELLATION
async fn test_network_cancel_error_handling() {
    use ractor::registry;
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (13900000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (14000000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((2, 1), (14100000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;

    let all_actors = registry::registered();
    error!("all actors: {:?}", all_actors.len());

    for i in 0..6 {
        let channel_prefix = format!("Channel-{}", i);
        assert!(
            all_actors
                .iter()
                .any(|actor| { actor.starts_with(&channel_prefix) }),
            "Channel actor should be registered with prefix {}",
            channel_prefix
        );
    }

    for i in 0..3 {
        let network_name = format!("network actor at {}", nodes[i].base_dir.to_str());
        assert!(
            registry::where_is(network_name).is_some(),
            "Network actor should be registered"
        );
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    cancel_tasks_and_wait_for_completion().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    for i in 0..3 {
        let network_name = format!("network actor at {}", nodes[i].base_dir.to_str());
        assert!(
            registry::where_is(network_name).is_none(),
            "Network actor should be removed"
        );
    }
    assert!(registry::registered().is_empty());
}

#[tokio::test]
async fn test_send_payment_will_use_sent_amount_for_better_path_finding() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (105 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (105 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (105 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let [node0, _node1, _node2, node3] = nodes.try_into().expect("4 nodes");

    let payment0 = node0
        .send_payment_keysend(&node3, 100, false)
        .await
        .unwrap()
        .payment_hash;
    let payment0_retry_times = node0.get_payment_session(payment0).unwrap().retry_times();
    node0.wait_until_success(payment0).await;
    assert_eq!(payment0_retry_times, 1);

    let payment1 = node0
        .send_payment_keysend(&node3, 100, false)
        .await
        .unwrap()
        .payment_hash;

    node0.wait_until_success(payment1).await;
    let payment1_retry_times = node0.get_payment_session(payment1).unwrap().retry_times();

    // sent_amount only track the amount inflight.
    // so here we will retry the payment once
    assert_eq!(payment1_retry_times, 2);

    let payment2 = node0
        .send_payment_keysend(&node3, 100, false)
        .await
        .unwrap()
        .payment_hash;

    node0.wait_until_success(payment2).await;
    let payment2_retry_times = node0.get_payment_session(payment2).unwrap().retry_times();
    // sent_amount only track the amount inflight.
    assert_eq!(payment2_retry_times, 2);
}

#[tokio::test]
async fn test_send_payment_dry_run_will_not_create_payment_session() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 400000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let payment_hash = gen_rand_sha256_hash();
    let res = node_0
        .send_payment(SendPaymentCommand {
            payment_hash: Some(payment_hash),
            amount: Some(1000),
            dry_run: true,
            target_pubkey: node_3.pubkey.into(),
            ..Default::default()
        })
        .await;
    eprintln!("res: {:?}", res);
    let payment = node_0.get_payment_session(payment_hash);
    assert!(payment.is_none(), "Payment session should not be created");

    let payment_hash = gen_rand_sha256_hash();
    let res = node_0
        .send_payment(SendPaymentCommand {
            payment_hash: Some(payment_hash),
            amount: Some(1000),
            dry_run: true,
            target_pubkey: node_2.pubkey.into(),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok(), "Send payment query failed: {:?}", res);
    let payment = node_0.get_payment_session(payment_hash);
    assert!(payment.is_none(), "Payment session should not be created");
}

#[tokio::test]
async fn test_payment_with_payment_data_record() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
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
        .allow_basic_mpp(true)
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

#[tokio::test]
async fn test_payment_with_insufficient_total_amount() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
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
        .allow_basic_mpp(false)
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

    // timeout hold tlc after 5 seconds
    let channel_id = channels[0];
    let tlc_id = add_tlc_result_1.tlc_id;
    node_1
        .network_actor
        .send_after(Duration::from_secs(5), move || {
            NetworkActorMessage::Command(NetworkActorCommand::TimeoutHoldTlc(
                payment_hash,
                channel_id,
                tlc_id,
            ))
        });

    // because tlc is not fulfilled, it should be removed after 5 seconds instead of settling
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason
        .is_none()
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // tlc should be removed after 5 seconds
    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
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

#[tokio::test]
async fn test_payment_with_wrong_payment_secret() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
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
        .allow_basic_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let wrong_payment_secret = gen_rand_sha256_hash();
    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(wrong_payment_secret, 10000000000);
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
        .is_some_and(|t| t.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_payment_with_insufficient_amount_with_payment_data() {
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
        .allow_basic_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let mut custom_records = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(payment_secret, 9000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 9000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: Some(custom_records.clone()),
        },
        PaymentHopData {
            amount: 9000000000,
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
                        amount: 9000000000,
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
        .is_some_and(|t| t.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_payment_with_insufficient_amount_without_payment_data() {
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
        .allow_basic_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let secp = Secp256k1::new();
    let hops_infos = vec![
        PaymentHopData {
            amount: 9000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 9000000000,
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
                        amount: 9000000000,
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
        .is_some_and(|t| t.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_send_two_node_send_each_other_multiple_time() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    for _i in 0..3 {
        let res = node_0
            .send_payment_keysend(&node_1, 20000000000, false)
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_0.wait_until_success(payment_hash).await;

        let payment_session = node_0.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());

        tokio::time::sleep(Duration::from_secs(1)).await;

        let res = node_1
            .send_payment_keysend(&node_0, 20000000000, false)
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

    let res = node_0
        .send_payment_keysend(&node_1, 20000000000, false)
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
    assert_eq!(node_1_balance, 20000000000);
}

#[tokio::test]
async fn test_network_with_hops_max_number_limit() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((5, 6), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((6, 7), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((7, 8), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((8, 9), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((9, 10), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((10, 11), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((11, 12), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((12, 13), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((13, 14), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((14, 15), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        16,
    )
    .await;

    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[14].pubkey), // can not make a payment with 14 hops
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: false,
            dry_run: false,
            tlc_expiry_limit: Some(DEFAULT_TLC_EXPIRY_DELTA * 12 + DEFAULT_FINAL_TLC_EXPIRY_DELTA), // 13 hops limit
            ..Default::default()
        })
        .await;

    assert!(payment.is_err());

    eprintln!("now test begin to send payment ...");
    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[13].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: false,
            dry_run: false,
            tlc_expiry_limit: Some(DEFAULT_TLC_EXPIRY_DELTA * 12 + DEFAULT_FINAL_TLC_EXPIRY_DELTA), // 13 hops limit
            ..Default::default()
        })
        .await
        .expect("send payment success");
    eprintln!("payment: {:?}", payment);
    nodes[0].wait_until_success(payment.payment_hash).await;

    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[13].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: false,
            dry_run: false,
            tlc_expiry_limit: Some(15 * 24 * 60 * 60 * 1000), // 15 days
            ..Default::default()
        })
        .await;

    assert!(
        payment.is_err(),
        "we can not set a max tlc expiry limit larger than 14 days"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_network_with_relay_remove_will_be_ok() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (mut nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        6,
    )
    .await;

    eprintln!("now test begin to send payment ...");
    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[5].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: false,
            dry_run: false,
            tlc_expiry_limit: Some(DEFAULT_TLC_EXPIRY_DELTA * 10 + DEFAULT_FINAL_TLC_EXPIRY_DELTA),
            ..Default::default()
        })
        .await
        .expect("send payment success");
    eprintln!("payment: {:?}", payment);

    loop {
        let channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
        if !channel_actor_state.tlc_state.offered_tlcs.tlcs.is_empty() {
            nodes[0].stop().await;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_micros(500)).await;
    }

    loop {
        let channel_actor_state = nodes[1].get_channel_actor_state(channels[0]);
        if !channel_actor_state.retryable_tlc_operations.is_empty() {
            eprintln!(
                "channel_actor_state: {:?}",
                channel_actor_state.retryable_tlc_operations
            );
            break;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            eprintln!("channel actor state: {:?}", channel_actor_state.state);
        }
    }

    nodes[0].start().await;
    nodes[0].wait_until_success(payment.payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_with_invalid_amount() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let payment = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_1.pubkey),
            amount: Some(0),
            keysend: Some(true),
            allow_self_payment: true,
            dry_run: true,
            ..Default::default()
        })
        .await;

    debug!("payment: {:?}", payment);
    assert!(payment.is_err());
    let error = payment.unwrap_err();
    assert!(error.contains("amount must be greater than 0"));

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(0),
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    eprintln!("result: {:?}", router);
    let error = router.unwrap_err();
    assert!(error.contains("amount must be greater than 0"));

    let payment = node_0.send_mpp_payment(&node_2, 0, Some(2)).await;

    debug!("payment: {:?}", payment);

    let error = payment.unwrap_err();
    assert!(error.contains("amount must be greater than 0"));
}

#[tokio::test]
async fn test_send_payment_direct_channel_error_from_node_stop() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    node_1.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let payment = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(100),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(payment.unwrap_err().contains("no path found"));
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

#[tokio::test]
async fn test_send_payment_mpp_can_not_be_keysend() {
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
    let node_2_pubkey = node_2.pubkey;
    let res = node_0
        .send_mpp_payment_with_command(
            &node_2,
            100,
            SendPaymentCommand {
                target_pubkey: Some(node_2_pubkey),
                keysend: Some(true),
                allow_self_payment: false,
                dry_run: false,
                ..Default::default()
            },
            MppMode::BasicMpp,
        )
        .await;
    eprintln!("res: {:?}", res);
    assert!(res
        .unwrap_err()
        .contains("keysend payment should not have invoice"));
}
