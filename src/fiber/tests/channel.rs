use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::tests::test_utils::{
    gen_rand_public_key, gen_sha256_hash, NetworkNodeConfigBuilder,
};
use crate::{
    ckb::contracts::{get_cell_deps, Contract},
    fiber::{
        channel::{
            derive_private_key, derive_tlc_pubkey, AddTlcCommand, ChannelActorStateStore,
            ChannelCommand, ChannelCommandWithId, InMemorySigner, RemoveTlcCommand,
            ShutdownCommand, DEFAULT_COMMITMENT_FEE_RATE,
        },
        config::DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT,
        graph::NetworkGraphStateStore,
        hash_algorithm::HashAlgorithm,
        network::{AcceptChannelCommand, OpenChannelCommand},
        types::{Hash256, Privkey, RemoveTlcFulfill, RemoveTlcReason},
        NetworkActorCommand, NetworkActorMessage,
    },
    now_timestamp_as_millis_u64, NetworkServiceEvent,
};
use ckb_jsonrpc_types::Status;
use ckb_types::{
    core::{FeeRate, TransactionView},
    packed::{CellInput, Script, Transaction},
    prelude::{AsTransactionBuilder, Builder, Entity, IntoTransactionView, Pack, Unpack},
};
use ractor::call;
use std::collections::HashSet;

use super::test_utils::{init_tracing, NetworkNode};

const DEFAULT_EXPIRY_DELTA: u64 = 24 * 60 * 60 * 1000; // 24 hours

#[test]
fn test_per_commitment_point_and_secret_consistency() {
    init_tracing();

    let signer = InMemorySigner::generate_from_seed(&[1; 32]);
    assert_eq!(
        signer.get_commitment_point(0),
        Privkey::from(&signer.get_commitment_secret(0)).pubkey()
    );
}

#[test]
fn test_derive_private_and_public_tlc_keys() {
    let privkey = Privkey::from(&[1; 32]);
    let per_commitment_point = Privkey::from(&[2; 32]).pubkey();
    let derived_privkey = derive_private_key(&privkey, &per_commitment_point);
    let derived_pubkey = derive_tlc_pubkey(&privkey.pubkey(), &per_commitment_point);
    assert_eq!(derived_privkey.pubkey(), derived_pubkey);
}

#[tokio::test]
async fn test_open_channel_to_peer() {
    let [node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let _open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
}

#[tokio::test]
async fn test_open_and_accept_channel() {
    let [node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT as u128,
                shutdown_script: None,
            },
            rpc_reply,
        ))
    };

    let _accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");
}

#[tokio::test]
async fn test_create_private_channel() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (_node_a, _node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;
}

#[tokio::test]
async fn test_create_public_channel() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (_node_a, _node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
}

#[tokio::test]
async fn test_public_channel_saved_to_the_owner_graph() {
    init_tracing();

    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 6200000000;

    let (mut node1, mut node2, _new_channel_id) =
        create_nodes_with_established_channel(node1_funding_amount, node2_funding_amount, true)
            .await;

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node1_store = node1.store.clone();
    let node1_id = node1.peer_id.clone();
    node1.stop().await;
    let node2_store = node2.store.clone();
    let node2_id = node2.peer_id.clone();
    node2.stop().await;

    let node1_channels = node1_store.get_channels(None);
    assert_eq!(node1_channels.len(), 1);
    let node1_channel = &node1_channels[0];
    assert_eq!(
        HashSet::from([node1_channel.node1_peerid(), node1_channel.node2_peerid()]),
        HashSet::from([node1_id.clone(), node2_id.clone()])
    );
    let node1_nodes = node1_store.get_nodes(None);
    assert_eq!(node1_nodes.len(), 2);
    for node in node1_nodes {
        assert!(node.node_id == node1_channel.node1() || node.node_id == node1_channel.node2());
    }

    let node2_channels = node2_store.get_channels(None);
    assert_eq!(node2_channels.len(), 1);
    let node2_channel = &node2_channels[0];
    assert_eq!(
        HashSet::from([node2_channel.node1_peerid(), node2_channel.node2_peerid()]),
        HashSet::from([node1_id, node2_id])
    );
    let node2_nodes = node2_store.get_nodes(None);
    assert_eq!(node2_nodes.len(), 2);
    for node in node2_nodes {
        assert!(node.node_id == node2_channel.node1() || node.node_id == node2_channel.node2());
    }
}

#[tokio::test]
async fn test_public_channel_saved_to_the_other_nodes_graph() {
    init_tracing();

    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 6200000000;

    let [mut node1, mut node2, mut node3] = NetworkNode::new_n_interconnected_nodes().await;
    let (_channel_id, funding_tx) = establish_channel_between_nodes(
        &mut node1,
        &mut node2,
        node1_funding_amount,
        node2_funding_amount,
        true,
    )
    .await;
    let status = node3.submit_tx(funding_tx).await;
    assert_eq!(status, Status::Committed);

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node3_store = node3.store.clone();
    node3.stop().await;
    let channels = node3_store.get_channels(None);
    assert_eq!(channels.len(), 1);
    let channel = &channels[0];
    assert_eq!(
        HashSet::from([channel.node1_peerid(), channel.node2_peerid()]),
        HashSet::from([node1.peer_id.clone(), node2.peer_id.clone()])
    );
    let nodes = node3_store.get_nodes(None);
    let node_pubkeys = nodes
        .iter()
        .map(|node| node.node_id)
        .collect::<HashSet<_>>();
    assert!(node_pubkeys.contains(&channel.node1()));
    assert!(node_pubkeys.contains(&channel.node2()));
}

#[tokio::test]
async fn test_public_channel_with_unconfirmed_funding_tx() {
    init_tracing();

    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 6200000000;

    let [mut node1, mut node2, mut node3] = NetworkNode::new_n_interconnected_nodes().await;
    let (_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node1,
        &mut node2,
        node1_funding_amount,
        node2_funding_amount,
        true,
    )
    .await;

    // We should submit the transaction to node 3's chain actor here.
    // If we don't do that node 3 will deem the funding transaction unconfirmed,
    // thus refusing to save the channel to the graph.

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node3_store = node3.store.clone();
    node3.stop().await;
    let channels = node3_store.get_channels(None);
    // No channels here as node 3 didn't think the funding transaction is confirmed.
    assert_eq!(channels.len(), 0);
}

#[tokio::test]
async fn test_network_send_payment_normal_keysend_workflow() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
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
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentSessionStatus::Inflight);
    let payment_hash = res.payment_hash;

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    // we can make the same payment again, since payment_hash will be generated randomly
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                tlc_expiry_limit: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentSessionStatus::Inflight);
}

#[tokio::test]
async fn test_network_send_payment_keysend_with_payment_hash() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let payment_hash = gen_sha256_hash();

    // This payment request is without an invoice, the receiver will return an error `IncorrectOrUnknownPaymentDetails`
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(payment_hash),
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
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res
        .err()
        .unwrap()
        .contains("keysend payment should not have payment_hash"));
}

#[tokio::test]
async fn test_network_send_payment_final_incorrect_hash() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let payment_hash = gen_sha256_hash();

    // This payment request is without an invoice, the receiver will return an error `IncorrectOrUnknownPaymentDetails`
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(payment_hash),
                final_tlc_expiry_delta: None,
                tlc_expiry_limit: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    assert_eq!(res.unwrap().status, PaymentSessionStatus::Inflight);

    tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Failed);
    assert_eq!(
        res.failed_error,
        Some("IncorrectOrUnknownPaymentDetails".to_string())
    );
}

#[tokio::test]
async fn test_network_send_payment_target_not_found() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, _node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_b_pubkey = gen_rand_public_key().into();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(gen_sha256_hash()),
                final_tlc_expiry_delta: None,
                tlc_expiry_limit: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
    assert!(res.err().unwrap().contains("target node not found"));
}

#[tokio::test]
async fn test_network_send_payment_amount_is_too_large() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(100000000000 + 5),
                payment_hash: Some(gen_sha256_hash()),
                final_tlc_expiry_delta: None,
                tlc_expiry_limit: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
    assert!(res
        .err()
        .unwrap()
        .contains("IncorrectOrUnknownPaymentDetails"));
}

// FIXME: this is the case send_payment with direct channels, we should handle this case
#[tokio::test]
async fn test_network_send_payment_with_dry_run() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(100000000000 + 5),
                payment_hash: Some(gen_sha256_hash()),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Created);
    // since there are only sender and receiver in the router, fee will be 0
    assert_eq!(res.fee, 0);

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(gen_rand_public_key()),
                amount: Some(1000 + 5),
                payment_hash: Some(gen_sha256_hash()),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    // since the target is not valid, the payment check will fail
    assert!(res.is_err());
}

#[tokio::test]
async fn test_network_send_payment_dry_run_can_still_query() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let payment_hash = gen_sha256_hash();
    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(payment_hash),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());

    // sleep for a while to make sure the payment session is created
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(payment_hash),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    eprintln!("{:?}", res);
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_network_send_payment_dry_run_will_not_create_payment_session() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

    let payment_hash = gen_sha256_hash();
    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(payment_hash),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());

    // make sure we can send the same payment after dry run query
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(payment_hash),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    eprintln!("{:?}", res);
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_stash_broadcast_messages() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, _node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    // Mark sync done for node_a after 1 second
    node_a
        .network_actor
        .send_after(ractor::concurrency::Duration::from_secs(1), || {
            NetworkActorMessage::new_command(NetworkActorCommand::MarkSyncingDone)
        });

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;
}

async fn do_test_channel_commitment_tx_after_add_tlc(algorithm: HashAlgorithm) {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let node_a_funding_amount = 100000000000;
    let node_b_funidng_amount = 6200000000;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: node_a_funding_amount,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: node_b_funidng_amount,
                shutdown_script: None,
            },
            rpc_reply,
        ))
    };
    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");
    let new_channel_id = accept_channel_result.new_channel_id;

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    let preimage = [1; 32];
    let digest = algorithm.hash(&preimage);
    let tlc_amount = 1000000000;

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: algorithm,
                        payment_hash: Some(digest.into()),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        preimage: None,
                        onion_packet: vec![],
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully added tlc");

    dbg!(&add_tlc_result);

    // Since we currently automatically send a `CommitmentSigned` message
    // after sending a `AddTlc` message, we can expect the `RemoteCommitmentSigned`
    // to be received by node b.
    let node_b_commitment_tx = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                println!(
                    "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                    num, &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx.clone())
            }
            _ => None,
        })
        .await;

    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result.tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully removed tlc");

    // Since we currently automatically send a `CommitmentSigned` message
    // after sending a `RemoveTlc` message, we can expect the `RemoteCommitmentSigned`
    // to be received by node a.
    let node_a_commitment_tx = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                println!(
                    "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                    num, &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx.clone())
            }
            _ => None,
        })
        .await;

    assert_eq!(
        node_a.submit_tx(node_a_commitment_tx.clone()).await,
        Status::Committed
    );

    assert_eq!(
        node_b.submit_tx(node_b_commitment_tx.clone()).await,
        Status::Committed
    );
}

#[tokio::test]
async fn test_channel_commitment_tx_after_add_tlc_ckbhash() {
    do_test_channel_commitment_tx_after_add_tlc(HashAlgorithm::CkbHash).await
}

#[tokio::test]
async fn test_channel_commitment_tx_after_add_tlc_sha256() {
    do_test_channel_commitment_tx_after_add_tlc(HashAlgorithm::Sha256).await
}

async fn establish_channel_between_nodes(
    node_a: &mut NetworkNode,
    node_b: &mut NetworkNode,
    node_a_funding_amount: u128,
    node_b_funding_amount: u128,
    public: bool,
) -> (Hash256, TransactionView) {
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public,
                shutdown_script: None,
                funding_amount: node_a_funding_amount,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: node_b_funding_amount,
                shutdown_script: None,
            },
            rpc_reply,
        ))
    };
    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");
    let new_channel_id = accept_channel_result.new_channel_id;

    let funding_tx_outpoint = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, funding_tx_outpoint) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(funding_tx_outpoint.clone())
            }
            _ => None,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    let funding_tx = node_a
        .get_tx_from_hash(funding_tx_outpoint.tx_hash())
        .await
        .expect("tx found");
    (new_channel_id, funding_tx)
}

async fn create_nodes_with_established_channel(
    node_a_funding_amount: u128,
    node_b_funding_amount: u128,
    public: bool,
) -> (NetworkNode, NetworkNode, Hash256) {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        node_a_funding_amount,
        node_b_funding_amount,
        public,
    )
    .await;

    (node_a, node_b, channel_id)
}

async fn do_test_remove_tlc_with_wrong_hash_algorithm(
    correct_algorithm: HashAlgorithm,
    wrong_algorithm: HashAlgorithm,
) {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = correct_algorithm.hash(&preimage);
    let tlc_amount = 1000000000;

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: correct_algorithm,
                        payment_hash: Some(digest.into()),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        preimage: None,
                        onion_packet: vec![],
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully added tlc");

    dbg!(&add_tlc_result);

    dbg!("Sleeping for some time to wait for the AddTlc processed by both party");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result.tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully removed tlc");

    dbg!("Sleeping for some time to wait for the RemoveTlc processed by both party");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: wrong_algorithm,
                        payment_hash: Some(digest.into()),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        preimage: None,
                        onion_packet: vec![],
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully added tlc");

    dbg!(&add_tlc_result);

    dbg!("Sleeping for some time to wait for the AddTlc processed by both party");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let remove_tlc_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result.tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive");

    dbg!(&remove_tlc_result);
    assert!(remove_tlc_result.is_err());
}

#[tokio::test]
async fn do_test_remove_tlc_with_expiry_error() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, _node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = HashAlgorithm::CkbHash.hash(&preimage);
    let tlc_amount = 1000000000;

    // add tlc command with expiry soon
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: Some(digest.into()),
        expiry: now_timestamp_as_millis_u64() + 10,
        preimage: None,
        onion_packet: vec![],
        previous_tlc: None,
    };

    std::thread::sleep(std::time::Duration::from_millis(400));
    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
            },
        ))
    })
    .expect("node_b alive");
    assert!(add_tlc_result.is_err());

    // add tlc command with expiry in the future too long
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: Some(digest.into()),
        expiry: now_timestamp_as_millis_u64() + MAX_PAYMENT_TLC_EXPIRY_LIMIT + 10,
        preimage: None,
        onion_packet: vec![],
        previous_tlc: None,
    };

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
            },
        ))
    })
    .expect("node_b alive");

    assert!(add_tlc_result.is_err());
}

#[tokio::test]
async fn test_remove_tlc_with_wrong_hash_algorithm() {
    let supported_algorithms = HashAlgorithm::supported_algorithms();
    for algorithm1 in &supported_algorithms {
        for algorithm2 in &supported_algorithms {
            if algorithm2 == algorithm1 {
                continue;
            }
            do_test_remove_tlc_with_wrong_hash_algorithm(*algorithm1, *algorithm2).await;
        }
    }
}

async fn do_test_channel_with_simple_update_operation(algorithm: HashAlgorithm) {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = algorithm.hash(&preimage);
    let tlc_amount = 1000000000;

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: algorithm,
                        payment_hash: Some(digest.into()),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        preimage: None,
                        onion_packet: vec![],
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully added tlc");

    dbg!(&add_tlc_result);

    dbg!("Sleeping for some time to wait for the AddTlc processed by both party");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result.tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully removed tlc");

    let fee_rate = FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE);
    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Script::default().as_builder().build(),
                        fee_rate,
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully shutdown channel");

    let node_a_shutdown_tx_hash = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelClosed(peer_id, channel_id, tx_hash) => {
                println!(
                    "Shutdown tx ({:?}) from {:?} for channel {:?} received",
                    &tx_hash, &peer_id, channel_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx_hash.clone())
            }
            _ => None,
        })
        .await;

    dbg!(&node_a_shutdown_tx_hash);

    let node_b_shutdown_tx_hash = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelClosed(peer_id, channel_id, tx_hash) => {
                println!(
                    "Shutdown tx ({:?}) from {:?} for channel {:?} received",
                    &tx_hash, &peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx_hash.clone())
            }
            _ => None,
        })
        .await;

    dbg!(&node_b_shutdown_tx_hash);

    assert_eq!(node_a_shutdown_tx_hash, node_b_shutdown_tx_hash);

    assert_eq!(
        node_a.trace_tx_hash(node_a_shutdown_tx_hash.clone()).await,
        Status::Committed
    );
    assert_eq!(
        node_b.trace_tx_hash(node_b_shutdown_tx_hash.clone()).await,
        Status::Committed
    );

    // TODO: maybe also check shutdown tx outputs and output balances here.
}

#[tokio::test]
async fn test_open_channel_with_invalid_ckb_amount_range() {
    init_tracing();

    let [node_a, node_b] = NetworkNode::new_n_interconnected_nodes().await;
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: true,
                shutdown_script: None,
                funding_amount: 0xfffffffffffffffffffffffffffffff,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(open_channel_result
        .err()
        .unwrap()
        .contains("The funding amount (21267647932558653966460912964485513215) should be less than 18446744073709551615"));
}

#[tokio::test]
async fn test_revoke_old_commitment_transaction() {
    init_tracing();

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: 6200000000,
                shutdown_script: None,
            },
            rpc_reply,
        ))
    };
    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");
    let new_channel_id = accept_channel_result.new_channel_id;

    let commitment_tx = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                println!(
                    "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                    num, &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx.clone())
            }
            _ => None,
        })
        .await;

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    node_a
        .network_actor
        .send_message(NetworkActorMessage::Command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::CommitmentSigned(),
            }),
        ))
        .expect("node_a alive");

    let (x_only_aggregated_pubkey, signature, output, output_data) = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RevokeAndAckReceived(
                peer_id,
                channel_id,
                commitment_number,
                x_only_aggregated_pubkey,
                signature,
                output,
                output_data,
            ) => {
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                assert_eq!(*commitment_number, 1u64);
                Some((
                    x_only_aggregated_pubkey.clone(),
                    signature.clone(),
                    output.clone(),
                    output_data.clone(),
                ))
            }
            _ => None,
        })
        .await;

    assert_eq!(
        node_a.submit_tx(commitment_tx.clone()).await,
        Status::Committed
    );

    println!("commitment_tx: {:?}", commitment_tx);

    let tx = Transaction::default()
        .as_advanced_builder()
        .cell_deps(get_cell_deps(vec![Contract::CommitmentLock], &None))
        .input(
            CellInput::new_builder()
                .previous_output(commitment_tx.output_pts().get(0).unwrap().clone())
                .build(),
        )
        .output(output)
        .output_data(output_data)
        .build();

    let empty_witness_args = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    let witness = [
        empty_witness_args.to_vec(),
        vec![0xFF],
        1u64.to_be_bytes().to_vec(),
        x_only_aggregated_pubkey.to_vec(),
        signature.serialize().to_vec(),
    ]
    .concat();

    let revocation_tx = tx.as_advanced_builder().witness(witness.pack()).build();

    assert_eq!(
        node_a.submit_tx(revocation_tx.clone()).await,
        Status::Committed
    );
}

#[tokio::test]
async fn test_channel_with_simple_update_operation() {
    for algorithm in HashAlgorithm::supported_algorithms() {
        do_test_channel_with_simple_update_operation(algorithm).await
    }
}

#[tokio::test]
async fn test_create_channel() {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: 6200000000,
                shutdown_script: None,
            },
            rpc_reply,
        ))
    };
    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");
    let new_channel_id = accept_channel_result.new_channel_id;

    let node_a_commitment_tx = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                println!(
                    "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                    num, &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx.clone())
            }
            _ => None,
        })
        .await;

    let node_b_commitment_tx = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, num, tx) => {
                println!(
                    "Commitment tx (#{}) {:?} from {:?} for channel {:?} received",
                    num, &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx.clone())
            }
            _ => None,
        })
        .await;

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    // We can submit the commitment txs to the chain now.
    assert_eq!(
        node_a.submit_tx(node_a_commitment_tx.clone()).await,
        Status::Committed
    );
    assert_eq!(
        node_b.submit_tx(node_b_commitment_tx.clone()).await,
        Status::Committed
    );
}

#[tokio::test]
async fn test_reestablish_channel() {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: 6200000000,
                shutdown_script: None,
            },
            rpc_reply,
        ))
    };
    let _accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node_b.peer_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;

    node_a
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(node_b.peer_id.clone()),
        ))
        .expect("node_a alive");

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node_b.peer_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;

    // Don't use `connect_to` here as that may consume the `ChannelCreated` event.
    // This is due to tentacle connection is async. We may actually send
    // the `ChannelCreated` event before the `PeerConnected` event.
    node_a.connect_to_nonblocking(&node_b).await;

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node_b.peer_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
}

#[tokio::test]
async fn test_force_close_channel_when_remote_is_offline() {
    let (mut node_a, mut node_b, channel_id) =
        create_nodes_with_established_channel(16200000000, 6200000000, true).await;

    node_b.stop().await;
    node_a
        .expect_event(|event| matches!(event, NetworkServiceEvent::PeerDisConnected(_, _)))
        .await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Script::default(),
                        fee_rate: FeeRate::from_u64(1000),
                        force: true,
                    },
                    rpc_reply,
                ),
            },
        ))
    };

    call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("successfully shutdown channel");
}

#[tokio::test]
async fn test_commitment_tx_capacity() {
    let (amount_a, amount_b) = (16200000000, 6200000000);
    let (node_a, _node_b, channel_id) =
        create_nodes_with_established_channel(amount_a, amount_b, true).await;

    let state = node_a.store.get_channel_actor_state(&channel_id).unwrap();
    let commitment_tx = state.latest_commitment_transaction.unwrap().into_view();
    let output_capacity: u64 = commitment_tx.output(0).unwrap().capacity().unpack();

    // default fee rate is 1000 shannons per kb, and there is a gap of 20 bytes between the mock commitment tx and the real one
    // ref to fn commitment_tx_size
    assert_eq!(
        amount_a + amount_b - (commitment_tx.data().serialized_size_in_block() + 20) as u128,
        output_capacity as u128
    );
}

#[tokio::test]
async fn test_connect_to_peers_with_mutual_channel_on_restart() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    node_a.restart().await;

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    )
    .await;
}

#[tokio::test]
async fn test_connect_to_peers_with_mutual_channel_on_restart_version_2() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    node_a.stop().await;

    node_b.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerDisConnected(id, _addr) if id == &node_a.peer_id),
    )
    .await;

    node_a.start().await;

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    )
    .await;
}

#[tokio::test]
async fn test_open_channel_with_large_size_shutdown_script_should_fail() {
    let [node_a, node_b] = NetworkNode::new_n_interconnected_nodes().await;

    // test open channel with large size shutdown script
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: Some(Script::new_builder().args(vec![0u8; 40].pack()).build()),
                funding_amount: (81 + 1) * 100000000 - 1,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };

    let open_channel_result = call!(node_a.network_actor, message).expect("node_a alive");

    assert!(open_channel_result
        .err()
        .unwrap()
        .contains("The funding amount (8199999999) should be greater than or equal to 8200000000"));
}

#[tokio::test]
#[should_panic(expected = "Waiting for event timeout")]
async fn test_accept_channel_with_large_size_shutdown_script_should_fail() {
    let mut nodes = NetworkNode::new_n_interconnected_nodes_with_config(2, |i| {
        NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("node-{}", i)))
            .base_dir_prefix(&format!("fnn-test-node-{}-", i))
            .fiber_config_updater(|config| {
                // enable auto accept channel with default value
                config.auto_accept_channel_ckb_funding_amount = Some(6200000000);
                config.open_channel_auto_accept_min_ckb_funding_amount = Some(16200000000);
            })
            .build()
    })
    .await;

    let mut node_a = nodes.pop().unwrap();
    let mut node_b = nodes.pop().unwrap();

    // test auto accept channel with large size shutdown script
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: Some(Script::new_builder().args(vec![0u8; 40].pack()).build()),
                funding_amount: (81 + 1 + 90) * 100000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_max_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };

    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(channel_id, &open_channel_result.channel_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;

    // should fail
    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                println!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                true
            }
            _ => false,
        })
        .await;
}

#[tokio::test]
async fn test_shutdown_channel_with_large_size_shutdown_script_should_fail() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (_node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Script::new_builder().args(vec![0u8; 21].pack()).build(),
                        fee_rate: FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE),
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    };

    let shutdown_channel_result = call!(node_b.network_actor, message).expect("node_b alive");
    assert!(shutdown_channel_result
        .err()
        .unwrap()
        .contains("Local balance is not enough to pay the fee"));
}

#[tokio::test]
async fn test_shutdown_channel_with_different_size_shutdown_script() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Script::new_builder().args(vec![0u8; 19].pack()).build(),
                        fee_rate: FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE),
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    };

    call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("successfully shutdown channel");

    let node_a_shutdown_tx_hash = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelClosed(peer_id, channel_id, tx_hash) => {
                println!(
                    "Shutdown tx ({:?}) from {:?} for channel {:?} received",
                    &tx_hash, &peer_id, channel_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx_hash.clone())
            }
            _ => None,
        })
        .await;

    let node_b_shutdown_tx_hash = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelClosed(peer_id, channel_id, tx_hash) => {
                println!(
                    "Shutdown tx ({:?}) from {:?} for channel {:?} received",
                    &tx_hash, &peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx_hash.clone())
            }
            _ => None,
        })
        .await;

    assert_eq!(node_a_shutdown_tx_hash, node_b_shutdown_tx_hash);

    assert_eq!(
        node_a.trace_tx_hash(node_a_shutdown_tx_hash.clone()).await,
        Status::Committed
    );
    assert_eq!(
        node_b.trace_tx_hash(node_b_shutdown_tx_hash.clone()).await,
        Status::Committed
    );
}
