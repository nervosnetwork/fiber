use crate::{
    fiber::{network::SendPaymentCommand, payment::MppMode},
    gen_rand_sha256_hash,
    invoice::{Currency, InvoiceBuilder},
    test_utils::{create_n_nodes_network, init_tracing, MIN_RESERVED_CKB},
    HUGE_CKB_AMOUNT,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_basic_amp() {
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
    let res = node_0
        .send_atomic_mpp_payment(&node_1, 20000000000, Some(2))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
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
async fn test_send_single_amp_path() {
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
    let res = node_0
        .send_atomic_mpp_payment(&node_1, 10000000000, Some(2))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let attempts = node_0
        .get_payment_session(payment_hash)
        .unwrap()
        .attempts_count();
    assert_eq!(attempts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_amp_without_invoice() {
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

    let target_pubkey = node_1.get_public_key();
    let amount = 20000000000;
    let builder = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payee_pub_key(target_pubkey.into())
        .allow_atomic_mpp(true)
        .payment_hash(gen_rand_sha256_hash());

    let ckb_invoice = builder.build().expect("build invoice success");
    // we don't insert the invoice into the store
    //node_1.insert_invoice(ckb_invoice.clone(), None);
    let command = SendPaymentCommand {
        max_parts: Some(2),
        invoice: Some(ckb_invoice.to_string()),
        ..Default::default()
    };
    let res = node_0.send_payment(command).await;
    let payment_hash = res.unwrap().payment_hash;

    node_0.wait_until_failed(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_3_nodes_in_middle() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((2, 3), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let res = node_0
        .send_atomic_mpp_payment(&node_3, 30000000000, Some(3))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_3_nodes_in_last_hop() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((2, 3), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
            ((2, 3), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let res = node_0
        .send_atomic_mpp_payment(&node_3, 30000000000, Some(3))
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_3_nodes_pay_self() {
    init_tracing();

    async fn test_pay_self(mpp_mode: MppMode) {
        let (nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
                ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
                ((2, 3), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
                ((2, 3), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
                ((2, 3), (MIN_RESERVED_CKB + 10100000000, MIN_RESERVED_CKB)),
                ((3, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ],
            4,
        )
        .await;
        let [node_0, _node_1, _node_2, _node_3] = nodes.try_into().expect("4 nodes");
        let res = node_0
            .send_mpp_payment_with_command(
                &node_0,
                30000000000,
                SendPaymentCommand {
                    max_parts: Some(3),
                    allow_self_payment: true,
                    ..Default::default()
                },
                mpp_mode,
            )
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        node_0.wait_until_success(payment_hash).await;
    }

    test_pay_self(MppMode::BasicMpp).await;
    test_pay_self(MppMode::AtomicMpp).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_amp_can_not_retry() {
    init_tracing();

    // we have 4 channels in the middle, but we disable a channel quite,
    // AMP can not handle retry router currently
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
    let res = node_0
        .send_atomic_mpp_payment(&node_3, 30000000000, Some(3))
        .await;
    node_1.disable_channel_stealthy(channels[3]).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_failed(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    let mut attempts = payment_session.attempts();
    assert!(attempts.any(|x| x.last_error.clone().unwrap().contains("ChannelDisabled")));
}
