#![allow(clippy::needless_range_loop)]
use crate::tests::*;
use crate::{
    fiber::types::Hash256,
    invoice::Currency,
    rpc::{
        channel::{ListChannelsParams, ListChannelsResult},
        invoice::{InvoiceParams, InvoiceResult, NewInvoiceParams},
        payment::{GetPaymentCommandParams, GetPaymentCommandResult},
        peer::ListPeersResult,
    },
};
use ckb_types::packed::Script;

#[tokio::test]
async fn test_rpc_basic() {
    let (nodes, _channels) = create_n_nodes_network_with_params(
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
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
        ],
        2,
        true,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let res: ListChannelsResult = node_0
        .send_rpc_request(
            "list_channels",
            ListChannelsParams {
                peer_id: None,
                include_closed: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.channels.len(), 2);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let payment = node_0
        .send_payment_keysend(&node_1, 1000, false)
        .await
        .unwrap();

    let payment_hash = payment.payment_hash;
    node_0.wait_until_success(payment_hash).await;

    let payment: GetPaymentCommandResult = node_0
        .send_rpc_request("get_payment", GetPaymentCommandParams { payment_hash })
        .await
        .unwrap();
    assert_eq!(payment.payment_hash, payment_hash);

    // node0 generate a invoice
    let invoice_res: InvoiceResult = node_0
        .send_rpc_request(
            "new_invoice",
            NewInvoiceParams {
                amount: 1000,
                description: Some("test".to_string()),
                currency: Currency::Fibd,
                expiry: Some(322),
                fallback_address: None,
                final_expiry_delta: Some(900000 + 1234),
                udt_type_script: Some(Script::default().into()),
                payment_preimage: Hash256::default(),
                hash_algorithm: Some(crate::fiber::hash_algorithm::HashAlgorithm::CkbHash),
            },
        )
        .await
        .unwrap();

    let invoice_payment_hash = invoice_res.invoice.payment_hash();
    let get_invoice_res: InvoiceResult = node_0
        .send_rpc_request(
            "get_invoice",
            InvoiceParams {
                payment_hash: *invoice_payment_hash,
            },
        )
        .await
        .unwrap();

    assert_eq!(get_invoice_res.invoice.payment_hash(), invoice_payment_hash);
}

#[tokio::test]
async fn test_rpc_list_peers() {
    let (nodes, _channels) = create_n_nodes_network_with_params(
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
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
        ],
        2,
        true,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");

    let list_peers: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 1);
    assert_eq!(list_peers.peers[0].pubkey, node_1.pubkey);
    let node_1_peer_id = list_peers.peers[0].peer_id.clone();

    let _res: () = node_0
        .send_rpc_request(
            "disconnect_peer",
            crate::rpc::peer::DisconnectPeerParams {
                peer_id: node_1_peer_id,
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let list_peers: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 0);

    let node_3 = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("node-{}", 3)))
            .base_dir_prefix(&format!("test-fnn-node-{}-", 3))
            .enable_rpc_server(true)
            .build(),
    )
    .await;

    let list_peers: ListPeersResult = node_3.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 0);

    node_0.connect_to(&node_3).await;
    let list_peers: ListPeersResult = node_3.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 1);
    assert_eq!(list_peers.peers[0].pubkey, node_0.pubkey);

    node_0.connect_to(&node_1).await;
    let list_peers: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 2);
    dbg!("list_peers: {:?}", &list_peers);
    assert!(list_peers.peers.iter().any(|p| p.pubkey == node_1.pubkey));
    assert!(list_peers.peers.iter().any(|p| p.pubkey == node_3.pubkey));
    assert!(list_peers.peers.iter().any(|p| p.peer_id == node_1.peer_id));
    assert!(list_peers.peers.iter().any(|p| p.peer_id == node_3.peer_id));
}
