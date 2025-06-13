use crate::cch::tests::lnd_test_utils::LndNode;

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_one_node() {
    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    println!("node_info: {:?}", lnd.get_info().await);
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_two_nodes_with_the_same_bitcoind() {
    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    println!("lnd 1 node_info: {:?}", lnd.get_info().await);

    let mut lnd2 = lnd.new_lnd_with_the_same_bitcoind(Default::default()).await;
    // The second node should be able to run independently of the first node.
    drop(lnd);

    println!("lnd 2 node_info: {:?}", lnd2.get_info().await);
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_two_nodes_with_established_channel() {
    let (_lnd1, _lnd2, channel) = LndNode::new_two_nodes_with_established_channel(
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .await;
    println!("channel: {:?}", channel);
}
