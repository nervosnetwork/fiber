use super::test_utils::init_tracing;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::network::HopHint;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::tests::test_utils::*;
use crate::fiber::types::Hash256;
use crate::fiber::NetworkActorCommand;
use crate::fiber::NetworkActorMessage;
use ractor::call;

#[tokio::test]
async fn test_send_payment_for_direct_channel_and_dry_run() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // from https://github.com/nervosnetwork/fiber/issues/359

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB, MIN_RESERVED_CKB + 10000000000)),
        ],
        2,
        true,
    )
    .await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey.clone();

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(10000000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: true,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(10000000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
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

    let res = node_1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(source_node.pubkey.clone()),
            amount: Some(10000000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let payment_hash = res.unwrap().payment_hash;
    node_1
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);

    // A -> B: 10000000000 use the first channel
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);

    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);

    // B -> A: 10000000000 use the second channel
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_send_payment_for_pay_self() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // from https://github.com/nervosnetwork/fiber/issues/362

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((2, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
        true,
    )
    .await;
    let [mut node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // now node_0 -> node_2 will be ok only with node_1, so the fee is larger than 0
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: true,
        })
        .await;

    assert!(res.unwrap().fee > 0);

    // node_0 -> node_0 will be ok for dry_run if `allow_self_payment` is true
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
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
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: true,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert_eq!(res.unwrap().fee, 0);
}

#[tokio::test]
async fn test_send_payment_for_pay_self_with_two_nodes() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // from https://github.com/nervosnetwork/fiber/issues/355

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
        true,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);

    // node_0 -> node_0 will be ok for dry_run if `allow_self_payment` is true
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
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
    let _span = tracing::info_span!("node", node = "test").entered();
    // from https://github.com/nervosnetwork/fiber/issues/362

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
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
        true,
    )
    .await;
    let [mut node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // node_0 -> node_0 will be ok if `allow_self_payment` is true
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
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
async fn test_send_payment_with_route_to_self_with_hop_hints() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
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
        true,
    )
    .await;
    let [mut node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    let channel_0_funding_tx = node_0.get_channel_funding_tx(&channels[0]).unwrap();

    // node_0 -> node_0 will be ok if `allow_self_payment` is true
    // use hop hints to help find_path use node1 -> node0,
    // then the only valid route will be node0 -> node2 -> node1 -> node0
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            hop_hints: Some(vec![HopHint {
                pubkey: node_0.pubkey.clone(),
                channel_funding_tx: channel_0_funding_tx,
                inbound: true,
            }]),
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
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
async fn test_send_payment_with_route_to_self_with_outbound_hop_hints() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
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
        true,
    )
    .await;
    let [mut node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    let channel_0_funding_tx = node_0.get_channel_funding_tx(&channels[0]).unwrap();

    // node_0 -> node_0 will be ok if `allow_self_payment` is true
    // use hop hints to help find_path use node0 -> node1,
    // then the only valid route will be node0 -> node1 -> node2 -> node0
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            hop_hints: Some(vec![HopHint {
                pubkey: node_0.pubkey.clone(),
                channel_funding_tx: channel_0_funding_tx,
                inbound: false,
            }]),
            dry_run: false,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
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
    // node0 -> node1 -> node2 -> node0
    let node1_fee = (node_1_new_channel0_balance - node_1_channel0_balance)
        - (node_1_channel1_balance - node_1_new_channel1_balance);

    assert!(node1_fee > 0);

    let node2_fee = (node_2_new_channel1_balance - node_2_channel1_balance)
        - (node_2_channel2_balance - node_2_new_channel2_balance);

    assert_eq!(node1_fee + node2_fee, res.fee);
}

#[tokio::test]
async fn test_send_payment_select_channel_with_hop_hints() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            // there are 3 channels from node1 -> node2
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
        true,
    )
    .await;
    let [mut node_0, node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);
    eprintln!("node_3: {:?}", node_3.pubkey);

    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();
    eprintln!("channel_3_funding_tx: {:?}", channel_3_funding_tx);
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_3.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            // at node_1, we must use channel_3 to reach node_2
            hop_hints: Some(vec![HopHint {
                pubkey: node_2.pubkey.clone(),
                channel_funding_tx: channel_3_funding_tx,
                inbound: true,
            }]),
            dry_run: false,
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
    assert_eq!(used_channels[1], channel_3_funding_tx);

    tokio::time::sleep(tokio::time::Duration::from_millis(2500)).await;

    // try channel_2 with outbound hop hints
    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    eprintln!("channel_2_funding_tx: {:?}", channel_2_funding_tx);
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_3.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            // at node_1, we must use channel_2 to reach node_2
            hop_hints: Some(vec![HopHint {
                pubkey: node_1.pubkey.clone(),
                channel_funding_tx: channel_2_funding_tx,
                inbound: false,
            }]),
            dry_run: false,
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
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_3.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            // at node_1, we must use channel_3 to reach node_2
            hop_hints: Some(vec![HopHint {
                pubkey: node_2.pubkey.clone(),
                channel_funding_tx: wrong_channel_hash,
                inbound: true,
            }]),
            dry_run: false,
        })
        .await;
    eprintln!("res: {:?}", res);
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("PathFind error: no path found"));
}

#[tokio::test]
async fn test_send_payment_two_nodes_with_hop_hints_and_multiple_channels() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
        true,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[1]).unwrap();
    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();
    let old_balance = node_0.get_local_balance_from_channel(channels[1]);
    let old_node1_balance = node_1.get_local_balance_from_channel(channels[3]);
    eprintln!("channel_1_funding_tx: {:?}", channel_1_funding_tx);
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey.clone()),
            amount: Some(60000000),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            hop_hints: Some(vec![
                // node1 - channel_1 -> node2
                HopHint {
                    pubkey: node_0.pubkey.clone(),
                    channel_funding_tx: channel_1_funding_tx,
                    inbound: false,
                },
                // node2 - channel_3 -> node1
                HopHint {
                    pubkey: node_0.pubkey.clone(),
                    channel_funding_tx: channel_3_funding_tx,
                    inbound: true,
                },
            ]),
            dry_run: false,
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
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
async fn test_network_send_payment_randomly_send_each_other() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let node_a_pubkey = node_a.pubkey.clone();
    let node_b_pubkey = node_b.pubkey.clone();

    let mut node_a_sent = 0;
    let mut node_b_sent = 0;
    let mut all_sent = vec![];
    for _i in 1..8 {
        let rand_wait_time = rand::random::<u64>() % 1000;
        tokio::time::sleep(tokio::time::Duration::from_millis(rand_wait_time)).await;

        let rand_num = rand::random::<u64>() % 2;
        let amount = rand::random::<u128>() % 10000;
        eprintln!("generated ampunt: {}", amount);
        let (source, target) = if rand_num == 0 {
            (&node_a.network_actor, node_b_pubkey.clone())
        } else {
            (&node_b.network_actor, node_a_pubkey.clone())
        };
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    target_pubkey: Some(target),
                    amount: Some(amount),
                    payment_hash: None,
                    final_tlc_expiry_delta: None,
                    tlc_expiry_limit: None,
                    invoice: None,
                    timeout: None,
                    max_fee_amount: None,
                    max_parts: None,
                    keysend: Some(true),
                    udt_type_script: None,
                    allow_self_payment: false,
                    hop_hints: None,
                    dry_run: false,
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
