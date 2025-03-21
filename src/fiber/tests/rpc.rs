#![allow(clippy::needless_range_loop)]

use ckb_types::packed::Script;

use crate::{
    fiber::{tests::test_utils::*, types::Hash256},
    invoice::Currency,
    rpc::{
        channel::{ListChannelsParams, ListChannelsResult},
        invoice::{InvoiceParams, InvoiceResult, NewInvoiceParams},
        payment::{GetPaymentCommandParams, GetPaymentCommandResult},
        peer::ListPeersResult,
    },
};

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
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let list_peers: ListPeersResult = node_0.send_rpc_request("list_peers", ()).await.unwrap();
    assert_eq!(list_peers.peers.len(), 1);
    assert_eq!(list_peers.peers[0].pubkey, node_1.pubkey);

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
