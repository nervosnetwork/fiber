use crate::{
    fiber::{
        graph::PaymentSessionState,
        network::SendPaymentCommand,
        tests::test_utils::{create_n_nodes_network, init_tracing, MIN_RESERVED_CKB},
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
    source_node.wait_until_success(payment_hash).await;

    let pss = PaymentSessionState::from_db(&source_node.store, payment_hash)
        .unwrap()
        .unwrap();
    dbg!(&pss.status, &pss.attempts.len());

    // We are using the second (newer) channel, so the first channel's balances are unchanged.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);

    // We are using the second (newer) channel, so the second channel's balances are changed.
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

    let pss = PaymentSessionState::from_db(&source_node.store, payment_hash)
        .unwrap()
        .unwrap();
    dbg!(&pss.status, &pss.attempts.len());

    // We are using the second (newer) channel, so the first channel's balances are unchanged.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 2500000000);
    assert_eq!(node_1_balance, 7500000000);

    // We are using the second (newer) channel, so the second channel's balances are changed.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 2500000000);
    assert_eq!(node_1_balance, 7500000000);
}
