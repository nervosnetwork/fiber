#![allow(clippy::needless_range_loop)]
use crate::fiber::channel::CloseFlags;
use crate::gen_rand_sha256_hash;
use crate::invoice::CkbInvoice;
use crate::rpc::channel::{ChannelState, ShutdownChannelParams};
use crate::rpc::config::RpcConfig;
use crate::rpc::info::NodeInfoResult;
use crate::tests::*;
use crate::{
    fiber::types::Hash256,
    invoice::Currency,
    rpc::{
        channel::{ListChannelsParams, ListChannelsResult},
        graph::{GraphNodesParams, GraphNodesResult},
        invoice::{InvoiceParams, InvoiceResult, NewInvoiceParams},
        payment::{GetPaymentCommandParams, GetPaymentCommandResult},
        peer::ListPeersResult,
    },
};
use biscuit_auth::macros::biscuit;
use biscuit_auth::{KeyPair, PrivateKey};
use ckb_types::packed::Script;
use std::str::FromStr;

fn rpc_config_with_auth() -> (RpcConfig, KeyPair) {
    let root = KeyPair::new();
    let mut config = gen_rpc_config();
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
        Some(gen_rpc_config()),
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

    let new_invoice_params = NewInvoiceParams {
        amount: 1000,
        description: Some("test".to_string()),
        currency: Currency::Fibd,
        expiry: Some(322),
        fallback_address: None,
        final_expiry_delta: Some(900000 + 1234),
        udt_type_script: Some(Script::default().into()),
        payment_preimage: Hash256::default(),
        hash_algorithm: Some(crate::fiber::hash_algorithm::HashAlgorithm::CkbHash),
        allow_mpp: Some(true),
    };

    // node0 generate a invoice
    let invoice_res: InvoiceResult = node_0
        .send_rpc_request("new_invoice", new_invoice_params)
        .await
        .unwrap();

    let ckb_invoice = invoice_res.invoice.clone();
    let invoice_payment_hash = ckb_invoice.data.payment_hash;
    let internal_ckb_invoice: CkbInvoice = invoice_res.invoice_address.parse().unwrap();
    assert!(internal_ckb_invoice.payment_secret().is_some());

    let get_invoice_res: InvoiceResult = node_0
        .send_rpc_request(
            "get_invoice",
            InvoiceParams {
                payment_hash: invoice_payment_hash,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        get_invoice_res.invoice.data.payment_hash,
        invoice_payment_hash
    );

    let raw_response = node_0
        .send_rpc_request_raw(
            "get_invoice",
            InvoiceParams {
                payment_hash: invoice_payment_hash,
            },
        )
        .await
        .unwrap();
    eprintln!("Raw RPC response: {}", raw_response);
    assert!(raw_response.to_string().contains("BASIC_MPP_OPTIONAL"));

    let new_invoice_params = NewInvoiceParams {
        amount: 1000,
        description: Some("test".to_string()),
        currency: Currency::Fibd,
        expiry: Some(322),
        fallback_address: None,
        final_expiry_delta: Some(900000 + 1234),
        udt_type_script: Some(Script::default().into()),
        payment_preimage: gen_rand_sha256_hash(),
        hash_algorithm: Some(crate::fiber::hash_algorithm::HashAlgorithm::CkbHash),
        allow_mpp: Some(false),
    };

    // node0 generate a invoice
    let invoice_res: InvoiceResult = node_0
        .send_rpc_request("new_invoice", new_invoice_params)
        .await
        .unwrap();

    let internal_ckb_invoice: CkbInvoice = invoice_res.invoice_address.parse().unwrap();
    assert!(internal_ckb_invoice.payment_secret().is_none());
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
        Some(gen_rpc_config()),
    )
    .await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");

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

    let mut node_3 = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("node-{}", 3)))
            .base_dir_prefix(&format!("test-fnn-node-{}-", 3))
            .rpc_config(Some(gen_rpc_config()))
            .build(),
    )
    .await;

    let list_peers: ListPeersResult = node_3.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 0);

    node_0.connect_to(&mut node_3).await;
    let list_peers: ListPeersResult = node_3.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 1);
    assert_eq!(list_peers.peers[0].pubkey, node_0.pubkey);

    node_0.connect_to(&mut node_1).await;
    let list_peers: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 2);
    dbg!("list_peers: {:?}", &list_peers);
    assert!(list_peers.peers.iter().any(|p| p.pubkey == node_1.pubkey));
    assert!(list_peers.peers.iter().any(|p| p.pubkey == node_3.pubkey));
    assert!(list_peers.peers.iter().any(|p| p.peer_id == node_1.peer_id));
    assert!(list_peers.peers.iter().any(|p| p.peer_id == node_3.peer_id));
}

#[tokio::test]
async fn test_rpc_graph() {
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
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let graph_nodes: GraphNodesResult = node_0
        .send_rpc_request(
            "graph_nodes",
            GraphNodesParams {
                limit: None,
                after: None,
            },
        )
        .await
        .unwrap();

    eprintln!("Graph nodes: {:#?}", graph_nodes);

    assert!(!graph_nodes.nodes.is_empty());
    assert!(graph_nodes.nodes.iter().any(|n| n.node_id == node_1.pubkey));
    assert!(graph_nodes
        .nodes
        .iter()
        .all(|n| n.version == *env!("CARGO_PKG_VERSION")));
    assert!(!graph_nodes.nodes[0].features.is_empty());
}

#[tokio::test]
async fn test_rpc_shutdown_channels() {
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
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, _node_1] = nodes.try_into().expect("2 nodes");

    let list_channels: ListChannelsResult = node_0
        .send_rpc_request(
            "list_channels",
            ListChannelsParams {
                peer_id: None,
                include_closed: None,
            },
        )
        .await
        .unwrap();
    eprintln!("List channels: {:#?}", list_channels);
    assert_eq!(list_channels.channels.len(), 2);
    let channel_id = list_channels.channels[0].channel_id;

    let _res: () = node_0
        .send_rpc_request(
            "shutdown_channel",
            ShutdownChannelParams {
                channel_id,
                close_script: None,
                fee_rate: None,
                force: None,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let list_channels: ListChannelsResult = node_0
        .send_rpc_request(
            "list_channels",
            ListChannelsParams {
                peer_id: None,
                include_closed: Some(true),
            },
        )
        .await
        .unwrap();
    eprintln!("List channels: {:#?}", list_channels);
    assert_eq!(list_channels.channels.len(), 2);
    let status = list_channels
        .channels
        .iter()
        .find(|c| c.channel_id == channel_id)
        .expect("channel should exist")
        .state;
    eprintln!("Channel status: {:?}", status);
    assert!(matches!(
        status,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    ));

    // test force close

    let channel_id = list_channels.channels[1].channel_id;
    let _res: () = node_0
        .send_rpc_request(
            "shutdown_channel",
            ShutdownChannelParams {
                channel_id,
                close_script: None,
                fee_rate: None,
                force: Some(true),
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let list_channels: ListChannelsResult = node_0
        .send_rpc_request(
            "list_channels",
            ListChannelsParams {
                peer_id: None,
                include_closed: Some(true),
            },
        )
        .await
        .unwrap();
    eprintln!("List channels: {:#?}", list_channels);
    assert_eq!(list_channels.channels.len(), 2);
    let status = list_channels
        .channels
        .iter()
        .find(|c| c.channel_id == channel_id)
        .expect("channel should exist")
        .state;
    eprintln!("Channel status: {:?}", status);
    assert!(matches!(
        status,
        ChannelState::Closed(CloseFlags::UNCOOPERATIVE)
    ));
}

#[tokio::test]
async fn test_rpc_node_info() {
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
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, _node_1] = nodes.try_into().expect("2 nodes");

    let node_info: NodeInfoResult = node_0.send_rpc_request("node_info", ()).await.unwrap();
    eprintln!("Node info: {:#?}", node_info);
    let version = env!("CARGO_PKG_VERSION").to_string();
    assert_eq!(node_info.version, version);
    assert_eq!(node_info.default_funding_lock_script, Default::default());
}

#[tokio::test]
async fn test_rpc_basic_with_auth() {
    init_tracing();
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

        biscuit.to_base64().unwrap()
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
                allow_mpp: None,
            },
        )
        .await
        .unwrap();

    let invoice_payment_hash = invoice_res.invoice.data.payment_hash;
    let get_invoice_res: InvoiceResult = node_0
        .send_rpc_request(
            "get_invoice",
            InvoiceParams {
                payment_hash: invoice_payment_hash,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        get_invoice_res.invoice.data.payment_hash,
        invoice_payment_hash
    );
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

        biscuit.to_base64().unwrap()
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

        biscuit.to_base64().unwrap()
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

        biscuit.to_base64().unwrap()
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

#[tokio::test]
async fn test_rpc_auth_with_fixed_token() {
    const PUBLIC_KEY: &str =
        "ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79";
    const PRIVATE_KEY: &str =
        "ed25519-private/89d6c88919e5ca326fbb8d1cbef406df08c0620575376651d53008762dc81f45";

    let mut rpc_config = gen_rpc_config();
    rpc_config.biscuit_public_key = Some(PUBLIC_KEY.to_string());

    let pk = PrivateKey::from_str(PRIVATE_KEY).unwrap();
    let auth_root = KeyPair::from(&pk);

    // sign a token with read node permission
    let token = {
        let biscuit = biscuit!(
            r#"
                read("peers");
    "#
        )
        .build(&auth_root)
        .unwrap();

        biscuit.to_base64().unwrap()
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

    let rpc_res: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(rpc_res.peers.len(), 0);
}
