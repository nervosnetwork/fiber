use crate::test_utils::{create_n_nodes_network, init_tracing, MIN_RESERVED_CKB};

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
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let res = node_0
        .send_atomic_mpp_payment(&mut node_1, 20000000000, Some(2))
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
