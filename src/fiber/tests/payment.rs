use super::test_utils::init_tracing;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::tests::test_utils::*;

#[tokio::test]
async fn test_send_payment_for_direct_channel_and_dry_run() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // from https://github.com/nervosnetwork/fiber/issues/359

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (4200000000 + 10000000000, 4200000000)),
            ((0, 1), (4200000000, 4200000000 + 10000000000)),
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
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let payment_hash = res.unwrap().payment_hash;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

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
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
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
