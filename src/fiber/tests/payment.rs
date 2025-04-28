#![allow(clippy::needless_range_loop)]
use super::test_utils::init_tracing;
use crate::fiber::channel::AddTlcCommand;
use crate::fiber::channel::ChannelCommand;
use crate::fiber::channel::ChannelCommandWithId;
use crate::fiber::channel::ChannelState;
use crate::fiber::channel::CloseFlags;
use crate::fiber::channel::RemoveTlcCommand;
use crate::fiber::channel::ShuttingDownFlags;
use crate::fiber::channel::UpdateCommand;
use crate::fiber::config::DEFAULT_TLC_EXPIRY_DELTA;
use crate::fiber::config::DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS;
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::config::MIN_TLC_EXPIRY_DELTA;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::BuildRouterCommand;
use crate::fiber::network::HopHint;
use crate::fiber::network::HopRequire;
use crate::fiber::network::PaymentCustomRecords;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::network::SendPaymentWithRouterCommand;
use crate::fiber::tests::test_utils::*;
use crate::fiber::types::Hash256;
use crate::fiber::types::RemoveTlcFulfill;
use crate::fiber::types::RemoveTlcReason;
use crate::fiber::types::NO_SHARED_SECRET;
use crate::fiber::NetworkActorCommand;
use crate::fiber::NetworkActorMessage;
use crate::gen_rand_fiber_public_key;
use crate::gen_rand_sha256_hash;
use crate::invoice::CkbInvoice;
use crate::invoice::Currency;
use crate::invoice::InvoiceBuilder;
use crate::now_timestamp_as_millis_u64;
use crate::NetworkServiceEvent;
use ckb_types::packed::Script;
use ckb_types::{core::tx_pool::TxStatus, packed::OutPoint};
use ractor::call;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::SystemTime;
use tracing::debug;
use tracing::error;

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

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

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
    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let payment_hash = res.unwrap().payment_hash;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
    };
    let res = call!(source_node.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);
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

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let res = node_0
        .send_payment_keysend(&node_2, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    assert!(res.fee > 0);
    let nodes = res.router.nodes;
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
    let nodes = res.router.nodes;
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

        // sleep for a while
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
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
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
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
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
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
        // sleep for a while
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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
        res.router.nodes[1].channel_outpoint,
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
        false,
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
    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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
    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Failed, Some(5))
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

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    source_node.wait_until_failed(payment_hash).await;
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
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

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
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
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
    assert_eq!(result.retried_times, 1);

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
    eprintln!("result: {:?}", result);
    assert_eq!(result.retried_times, 1);
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
    eprintln!("payment_session: {:?}", payment_session);
    let used_channels: Vec<Hash256> = payment_session
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();
    eprintln!("used_channels: {:?}", used_channels);
    assert_eq!(used_channels.len(), 4);
    assert_eq!(used_channels[1], channel_3_funding_tx);

    tokio::time::sleep(tokio::time::Duration::from_millis(2500)).await;

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
    let payment_session = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");
    eprintln!("payment_session: {:?}", payment_session);
    let used_channels: Vec<Hash256> = payment_session
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
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();

    assert_eq!(used_channels.len(), 3);
    assert_eq!(used_channels[0], channel_1_funding_tx);
    assert_eq!(used_channels[1], channel_3_funding_tx);

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

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
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let node_a_pubkey = node_a.pubkey;
    let node_b_pubkey = node_b.pubkey;

    let mut node_a_sent = 0;
    let mut node_b_sent = 0;
    let mut all_sent = vec![];
    for _i in 1..8 {
        let rand_wait_time = rand::random::<u64>() % 1000;
        tokio::time::sleep(tokio::time::Duration::from_millis(rand_wait_time)).await;

        let rand_num = rand::random::<u64>() % 2;
        let amount = rand::random::<u128>() % 10000 + 1;
        eprintln!("generated amount: {}", amount);
        let (source, target) = if rand_num == 0 {
            (&node_a.network_actor, node_b_pubkey)
        } else {
            (&node_b.network_actor, node_a_pubkey)
        };
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    target_pubkey: Some(target),
                    amount: Some(amount),
                    keysend: Some(true),
                    ..Default::default()
                },
                rpc_reply,
            ))
        };

        let res = call!(source, message).expect("node_a alive").unwrap();

        if rand_num == 0 {
            all_sent.push((true, amount, res.payment_hash, res.status));
        } else {
            all_sent.push((false, amount, res.payment_hash, res.status));
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(4000)).await;
    for (a_sent, amount, payment_hash, create_status) in all_sent {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
        };
        let network = if a_sent {
            &node_a.network_actor
        } else {
            &node_b.network_actor
        };
        let res = call!(network, message).expect("node_a alive").unwrap();
        if res.status == PaymentSessionStatus::Success {
            assert!(matches!(
                create_status,
                PaymentSessionStatus::Created | PaymentSessionStatus::Inflight
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

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
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

    let node_a_pubkey = node_a.pubkey;
    let node_c_pubkey = node_c.pubkey;

    let amount_a_to_c = 60000;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(amount_a_to_c),
                keysend: Some(true),
                ..Default::default()
            },
            rpc_reply,
        ))
    };

    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    let payment_hash1 = res.payment_hash;
    let fee1 = res.fee;
    eprintln!("payment_hash1: {:?}", payment_hash1);

    let amount_c_to_a = 60000;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey),
                amount: Some(amount_c_to_a),
                keysend: Some(true),
                ..Default::default()
            },
            rpc_reply,
        ))
    };

    let res = call!(node_c.network_actor, message)
        .expect("node_a alive")
        .unwrap();

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
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let mut all_sent = HashSet::new();

    for i in 1..=10 {
        let payment = node_0
            .send_payment_keysend(&node_2, 1000, false)
            .await
            .unwrap();
        eprintln!("payment: {:?}", payment);
        all_sent.insert(payment.payment_hash);
        eprintln!("send: {} payment_hash: {:?} sent", i, payment.payment_hash);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    loop {
        for payment_hash in all_sent.clone().iter() {
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("got payment: {:?} status: {:?}", payment_hash, status);
            if status == PaymentSessionStatus::Success {
                eprintln!("payment_hash: {:?} success", payment_hash);
                all_sent.remove(payment_hash);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        let res = node_0.node_info().await;
        eprintln!("node0 node_info: {:?}", res);
        let res = node_1.node_info().await;
        eprintln!("node1 node_info: {:?}", res);
        let res = node_2.node_info().await;
        eprintln!("node2 node_info: {:?}", res);
        if all_sent.is_empty() {
            break;
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

        node_0.wait_until_success(payment.payment_hash).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

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
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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

    let nodes = NetworkNode::new_interconnected_nodes(2, false).await;
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
    eprintln!("debug result: {:?}", result);
    assert!(result
        .failed_error
        .expect("got error")
        .contains("Failed to build route"));
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
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    loop {
        for i in 0..4 {
            assert!(nodes[i].get_triggered_unexpected_events().await.is_empty());
        }

        for payment_hash in all_sent.clone().iter() {
            let status = nodes[0].get_payment_status(*payment_hash).await;
            //eprintln!("got payment: {:?} status: {:?}", payment_hash, status);
            if status == PaymentSessionStatus::Failed || status == PaymentSessionStatus::Success {
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                all_sent.remove(payment_hash);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
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
            let status = nodes[0].get_payment_status(*payment_hash).await;
            if status == PaymentSessionStatus::Success || status == PaymentSessionStatus::Failed {
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                all_sent.remove(payment_hash);
                if status == PaymentSessionStatus::Success {
                    succ_count += 1;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
) -> Vec<(Hash256, PaymentSessionStatus)> {
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
            let status = nodes[i].get_payment_status(payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if matches!(
                status,
                PaymentSessionStatus::Success | PaymentSessionStatus::Failed
            ) {
                result.push((payment_hash, status));
                all_sent.remove(&(i, payment_hash));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
        false,
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
        false,
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
        false,
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
        false,
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
            tlc_expiry_limit: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // Ok now
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
                    b_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a large value
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
        false,
    )
    .await;

    // no tlc_expiry_limit will be OK
    let _res = nodes[0]
        .assert_send_payment_success(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            tlc_expiry_limit: Some(DEFAULT_TLC_EXPIRY_DELTA),
            ..Default::default()
        })
        .await;

    assert!(res
        .unwrap_err()
        .to_string()
        .contains("no direct channel found for source node"));
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
        false,
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
        false,
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

    assert!(res
        .unwrap_err()
        .to_string()
        .contains("no direct channel found for source node"));

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
async fn test_send_payment_complex_network_payself_all_succeed() {
    // from issue 475
    // channel amount is enough, so all payments should success
    let res = run_complex_network_with_params(MIN_RESERVED_CKB + 100000000, || 1000).await;
    let failed_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentSessionStatus::Failed)
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
        .filter(|(_, status)| *status == PaymentSessionStatus::Failed)
        .count();
    assert!(failed_count > 0);
    let succ_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentSessionStatus::Success)
        .count();
    assert!(succ_count > 0);
}

#[tokio::test]
async fn test_send_payment_with_one_node_stop() {
    // make sure part of the payments will fail, since the node is stopped
    // TLC forwarding will fail and proper error will be returned
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
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    let mut failed_count = 0;
    let mut check_count = 0;
    while check_count < 100 {
        for payment_hash in all_sent.clone().iter() {
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!("payment_hash: {:?} status: {:?}", payment_hash, res.status);
            if res.status == PaymentSessionStatus::Failed {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        check_count += 1;
        if all_sent.is_empty() {
            break;
        }
    }
    assert_eq!(failed_count, 4);
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

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = nodes[0].send_payment_keysend(&nodes[3], 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = nodes[3].send_shutdown(channels[2], true).await;

            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            nodes[3]
                .send_channel_shutdown_tx_confirmed_event(
                    nodes[2].peer_id.clone(),
                    channels[2],
                    true,
                )
                .await;
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }

    // make sure the later payments will fail
    // because network actor will find out the inactive channels and disconnect peers
    // which send shutdown force message
    let mut failed_count = 0;
    let expect_failed_count = all_sent.len();
    let started = SystemTime::now();

    while !all_sent.is_empty() {
        for payment_hash in all_sent.clone().iter() {
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!(
                "payment_hash: {:?} status: {:?} failed_count: {:?}",
                payment_hash, res.status, failed_count
            );
            if res.status == PaymentSessionStatus::Failed {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        let elapsed = SystemTime::now()
            .duration_since(started)
            .expect("time passed")
            .as_secs();
        if elapsed > 60 {
            error!("timeout, failed_count: {:?}", failed_count);
            break;
        }
    }
    assert!(failed_count >= expect_failed_count);

    let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
    eprintln!(
        "node_3_channel_actor_state: {:?}",
        node_3_channel_actor_state.state
    );
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::UNCOOPERATIVE)
    );

    // because node2 didn't receive the shutdown message,
    // so it will still think the channel is ready
    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
    eprintln!(
        "node_2_channel_actor_state: {:?}",
        node_2_channel_actor_state.state
    );
    assert_eq!(node_2_channel_actor_state.state, ChannelState::ChannelReady);
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
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!(
                "payment_hash: {:?} status: {:?} failed_count: {:?}",
                payment_hash, res.status, failed_count
            );
            if res.status == PaymentSessionStatus::Failed
                || res.status == PaymentSessionStatus::Success
            {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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

    for i in 0..10 {
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
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        if !node_1_channel_actor_state.any_tlc_pending()
            && !node_2_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
    error!("node_1 state: {:?}", node_1_channel_actor_state.state);
    assert_eq!(
        node_1_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_0_channel_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    assert_eq!(
        node_0_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
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
        assert_eq!(status, PaymentSessionStatus::Success);

        nodes[restart_node_index].restart().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();
        let payment_hash = res.payment_hash;
        eprintln!("res: {:?}", payment_hash);

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentSessionStatus::Success);
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
        assert_eq!(status, PaymentSessionStatus::Success);

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
        assert_eq!(status, PaymentSessionStatus::Failed);

        tokio::time::sleep(tokio::time::Duration::from_millis(4 * 1000)).await;

        // now we start nodes[2], expect the payment will success
        nodes[restart_node_index].start().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        // because the probability of the path is not 100% after the node is restarted
        // send normal payment amount will fail at the beginning
        let normal_payment_amount = 100_000_000;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], normal_payment_amount, true)
            .await;
        assert!(res.is_err());

        // we can start send payment with small amount
        let payment_amount = 50000000;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();
        let payment_hash = res.payment_hash;
        eprintln!("res: {:?}", payment_hash);

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentSessionStatus::Success);

        // with time passed, we can send payment with larger amount
        let mut count = 0;
        loop {
            let res = nodes[0]
                .send_payment_keysend(&nodes[3], normal_payment_amount, true)
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
    for node in [&mut node_0, &mut node_1, &mut node_2, &mut node_3].into_iter() {
        let res = node.submit_tx(funding_tx.clone()).await;
        assert!(matches!(res, TxStatus::Committed(..)));
        node.add_channel_tx(channel_id, funding_tx_hash);
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let res = node_0
        .send_payment_keysend(&node_3, payment_amount, false)
        .await;

    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
// This test implies a bug when reconnecting a peer under the condition of multiple TLC operation
// skip temporarily until the bug is fixed
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
    let [mut node_0, mut node_1, node_2] = nodes.try_into().expect("4 nodes");

    let mut payments = HashSet::new();

    for _i in 0..5 {
        let amount = rand::random::<u128>() % 1000 + 1;
        let res = node_0
            .send_payment_keysend(&node_2, amount, false)
            .await
            .unwrap();
        payments.insert(res.payment_hash);
        node_0.wait_until_created(res.payment_hash).await;
    }

    let node1_id = node_1.peer_id.clone();
    let node0_id = node_0.peer_id.clone();
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(node1_id.clone()),
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

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

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
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentSessionStatus::Success {
                payments.remove(payment_hash);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
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
    let [mut node_0, _node_1, _node_2] = nodes.try_into().expect("4 nodes");

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

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    loop {
        for payment_hash in payments.clone().iter() {
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentSessionStatus::Failed {
                payments.remove(payment_hash);
            }
            assert_ne!(status, PaymentSessionStatus::Success);
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

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
    assert_eq!(payment_session.retried_times, 1);
}
