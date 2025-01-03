use super::test_utils::init_tracing;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::tests::test_utils::*;
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

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

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

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

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
            dry_run: true,
        })
        .await;

    eprintln!("res: {:?}", res);
    assert_eq!(res.unwrap().fee, 0);
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

#[tokio::test]
async fn test_network_three_nodes_two_channels_send_each_other() {
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
        ],
        3,
        true,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_b_old_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_old_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);

    let node_a_pubkey = node_a.pubkey.clone();
    let node_c_pubkey = node_c.pubkey.clone();

    let amount_a_to_c = 60000;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey.clone()),
                amount: Some(amount_a_to_c),
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
                dry_run: false,
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

    let amount_c_to_a = 50000;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey.clone()),
                amount: Some(amount_c_to_a),
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
                dry_run: false,
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

    tokio::time::sleep(tokio::time::Duration::from_millis(12000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash1, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash2, rpc_reply))
    };
    let res = call!(node_c.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);

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
                (2, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 0),
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

    let node_a_pubkey = node_a.pubkey.clone();
    let node_c_pubkey = node_c.pubkey.clone();

    let amount_a_to_c = 60000;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey.clone()),
                amount: Some(amount_a_to_c),
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
                dry_run: false,
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
                target_pubkey: Some(node_a_pubkey.clone()),
                amount: Some(amount_c_to_a),
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
                dry_run: false,
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

    tokio::time::sleep(tokio::time::Duration::from_millis(7000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash1, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash2, rpc_reply))
    };
    let res = call!(node_c.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);

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
