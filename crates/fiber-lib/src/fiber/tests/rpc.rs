#![allow(clippy::needless_range_loop)]
use crate::rpc::config::RpcConfig;
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
use biscuit_auth::macros::biscuit;
use biscuit_auth::KeyPair;
use ckb_types::packed::Script;

fn rpc_config() -> RpcConfig {
    RpcConfig {
        listening_addr: None,
        biscuit_public_key: None,
        enabled_modules: vec![
            "channel".to_string(),
            "graph".to_string(),
            "payment".to_string(),
            "invoice".to_string(),
            "peer".to_string(),
            "watchtower".to_string(),
        ],
    }
}

fn rpc_config_with_auth() -> (RpcConfig, KeyPair) {
    let root = KeyPair::new();
    let mut config = rpc_config();
    config.biscuit_public_key = Some(root.public().to_string());
    (config, root)
}

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
        Some(rpc_config()),
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
        Some(rpc_config()),
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
            .enable_rpc_server()
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
}

#[tokio::test]
async fn test_rpc_basic_with_auth() {
    let (rpc_config, auth_root) = rpc_config_with_auth();
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
        Some(rpc_config),
    )
    .await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    // sign a token with read node permission
    let token = {
        let biscuit = biscuit!(
            r#"
                read("channels");
                read("payments");
                write("payments");
                read("invoices");
                write("invoices");
    "#
        )
        .build(&auth_root)
        .unwrap();

        biscuit.to_vec().unwrap()
    };

    node_0.set_auth_token(token.clone());
    node_1.set_auth_token(token);

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
async fn test_rpc_auth_without_token() {
    let (rpc_config, _auth_root) = rpc_config_with_auth();

    let node_0 = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("node-0".to_string()))
            .base_dir_prefix("test-fnn-node-0-")
            .rpc_config(Some(rpc_config))
            .build(),
    )
    .await;

    let rpc_res: Result<ListPeersResult, String> = node_0.send_rpc_request("list_peers", ()).await;
    assert!(rpc_res.is_err());
    assert!(rpc_res.unwrap_err().to_string().contains("Unauthorized"));
}

#[tokio::test]
async fn test_rpc_auth_with_token() {
    let (rpc_config, auth_root) = rpc_config_with_auth();

    // sign a token with read node permission
    let token = {
        let biscuit = biscuit!(
            r#"
                read("peers");
    "#
        )
        .build(&auth_root)
        .unwrap();

        biscuit.to_vec().unwrap()
    };

    let mut node_0 = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("node-0".to_string()))
            .base_dir_prefix("test-fnn-node-0-")
            .rpc_config(Some(rpc_config))
            .build(),
    )
    .await;

    dbg!(hex::encode(&token));
    node_0.set_auth_token(token);

    let rpc_res: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(rpc_res.peers.len(), 0);
}

#[tokio::test]
async fn test_rpc_auth_with_invalid_token() {
    let (rpc_config, _auth_root) = rpc_config_with_auth();

    // sign a token with read node permission
    let token = {
        let invalid_root = KeyPair::new();
        let biscuit = biscuit!(
            r#"
                read("peers");
    "#
        )
        .build(&invalid_root)
        .unwrap();

        biscuit.to_vec().unwrap()
    };

    let mut node_0 = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("node-0".to_string()))
            .base_dir_prefix("test-fnn-node-0-")
            .rpc_config(Some(rpc_config))
            .build(),
    )
    .await;

    dbg!(hex::encode(&token));
    node_0.set_auth_token(token);

    let rpc_res: Result<ListPeersResult, String> = node_0.send_rpc_request("list_peers", ()).await;
    assert!(rpc_res.is_err());
    assert!(rpc_res.unwrap_err().to_string().contains("Unauthorized"));
}

#[tokio::test]
async fn test_rpc_auth_with_wrong_permission() {
    let (rpc_config, auth_root) = rpc_config_with_auth();

    // sign a token with read node permission
    let token = {
        let biscuit = biscuit!(
            r#"
                read("channels");
    "#
        )
        .build(&auth_root)
        .unwrap();

        biscuit.to_vec().unwrap()
    };

    let mut node_0 = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("node-0".to_string()))
            .base_dir_prefix("test-fnn-node-0-")
            .rpc_config(Some(rpc_config))
            .build(),
    )
    .await;

    node_0.set_auth_token(token);

    let rpc_res: Result<ListPeersResult, String> = node_0.send_rpc_request("list_peers", ()).await;
    assert!(rpc_res.is_err());
    assert!(rpc_res.unwrap_err().to_string().contains("Unauthorized"));
}
