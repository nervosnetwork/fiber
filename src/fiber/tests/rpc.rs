#![allow(clippy::needless_range_loop)]

use crate::{fiber::tests::test_utils::*, rpc::channel::ListChannelsParams};

#[tokio::test]
async fn test_rpc_basic() {
    let (nodes, _channels) = create_n_nodes_network_with_rpc_option(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
        true,
    )
    .await;
    let [node_0, _node_1] = nodes.try_into().expect("2 nodes");

    let res = node_0
        .send_rpc_request(
            "list_channels",
            ListChannelsParams {
                peer_id: None,
                include_closed: None,
            },
        )
        .await
        .unwrap();
    eprintln!("get_info: {:?}", res);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
}
