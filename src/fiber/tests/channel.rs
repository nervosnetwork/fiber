use super::test_utils::{init_tracing, NetworkNode};
use crate::fiber::channel::UpdateCommand;
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::graph::{ChannelInfo, PaymentSessionStatus};
use crate::fiber::network::{DebugEvent, SendPaymentCommand};
use crate::fiber::tests::test_utils::*;
use crate::fiber::types::{
    Hash256, PaymentHopData, PeeledOnionPacket, Pubkey, TlcErrorCode, NO_SHARED_SECRET,
};
use crate::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder};
use crate::{
    ckb::contracts::{get_cell_deps, Contract},
    fiber::{
        channel::{
            derive_private_key, derive_tlc_pubkey, AddTlcCommand, ChannelActorStateStore,
            ChannelCommand, ChannelCommandWithId, InMemorySigner, RemoveTlcCommand,
            ShutdownCommand, DEFAULT_COMMITMENT_FEE_RATE,
        },
        config::DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT,
        hash_algorithm::HashAlgorithm,
        network::{AcceptChannelCommand, OpenChannelCommand},
        tests::test_utils::establish_channel_between_nodes,
        types::{Privkey, RemoveTlcFulfill, RemoveTlcReason},
        NetworkActorCommand, NetworkActorMessage,
    },
    gen_rand_fiber_private_key, gen_rand_fiber_public_key, gen_rand_sha256_hash,
    now_timestamp_as_millis_u64, NetworkServiceEvent,
};
use ckb_jsonrpc_types::Status;
use ckb_types::{
    core::FeeRate,
    packed::{CellInput, Script, Transaction},
    prelude::{AsTransactionBuilder, Builder, Entity, IntoTransactionView, Pack, Unpack},
};
use ractor::call;
use secp256k1::Secp256k1;
use std::collections::HashSet;
use std::time::Duration;

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
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
                min_tlc_value: None,
                tlc_fee_proportional_millionths: None,
                tlc_expiry_delta: None,
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

    let (_node_a, _node_b, _new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_create_channel_with_remote_tlc_info() {
    async fn test(public: bool) {
        let node_a_funding_amount = 100000000000;
        let node_b_funding_amount = 6200000000;

        let (node_a, node_b, channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            public,
        )
        .await;

        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        let node_a_channel_state = node_a.store.get_channel_actor_state(&channel_id).unwrap();
        let node_b_channel_state = node_b.store.get_channel_actor_state(&channel_id).unwrap();

        assert_eq!(
            Some(node_a_channel_state.local_tlc_info),
            node_b_channel_state.remote_tlc_info
        );
        assert_eq!(
            Some(node_b_channel_state.local_tlc_info),
            node_a_channel_state.remote_tlc_info
        );
    }

    test(true).await;
    test(false).await;
}

#[tokio::test]
async fn test_create_public_channel() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (_node_a, _node_b, _new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        true,
    )
    .await;
}

async fn do_test_owned_channel_saved_to_the_owner_graph(public: bool) {
    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 6200000000;

    let (mut node1, mut node2, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node1_funding_amount,
            node2_funding_amount,
            public,
        )
        .await;

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node1_id = node1.peer_id.clone();
    node1.stop().await;
    let node2_id = node2.peer_id.clone();
    node2.stop().await;

    let node1_channels = node1.get_network_graph_channels().await;
    assert_eq!(node1_channels.len(), 1);
    let node1_channel = &node1_channels[0];
    assert_eq!(
        HashSet::from([node1_channel.node1_peerid(), node1_channel.node2_peerid()]),
        HashSet::from([node1_id.clone(), node2_id.clone()])
    );
    assert_ne!(node1_channel.update_of_node1, None);
    assert_ne!(node1_channel.update_of_node2, None);
    let node1_nodes = node1.get_network_graph_nodes().await;
    assert_eq!(node1_nodes.len(), 2);
    for node in node1_nodes {
        assert!(node.node_id == node1_channel.node1() || node.node_id == node1_channel.node2());
    }

    let node2_channels = node2.get_network_graph_channels().await;
    assert_eq!(node2_channels.len(), 1);
    let node2_channel = &node2_channels[0];
    assert_ne!(node2_channel.update_of_node1, None);
    assert_ne!(node2_channel.update_of_node2, None);
    assert_eq!(
        HashSet::from([node2_channel.node1_peerid(), node2_channel.node2_peerid()]),
        HashSet::from([node1_id, node2_id])
    );
    let node2_nodes = node2.get_network_graph_nodes().await;
    assert_eq!(node2_nodes.len(), 2);
    for node in node2_nodes {
        assert!(node.node_id == node2_channel.node1() || node.node_id == node2_channel.node2());
    }
}

#[tokio::test]
async fn test_owned_public_channel_saved_to_the_owner_graph() {
    do_test_owned_channel_saved_to_the_owner_graph(true).await;
}

#[tokio::test]
async fn test_owned_private_channel_saved_to_the_owner_graph() {
    do_test_owned_channel_saved_to_the_owner_graph(false).await;
}

async fn do_test_owned_channel_removed_from_graph_on_disconnected(public: bool) {
    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 6200000000;

    let (mut node1, mut node2, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node1_funding_amount,
            node2_funding_amount,
            public,
        )
        .await;

    let node1_id = node1.peer_id.clone();
    let node2_id = node2.peer_id.clone();

    let node1_channels = node1.get_network_graph_channels().await;
    assert_ne!(node1_channels, vec![]);
    let node2_channels = node2.get_network_graph_channels().await;
    assert_ne!(node2_channels, vec![]);

    node1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(node2_id.clone()),
        ))
        .expect("node_a alive");

    node1
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node2_id);
                true
            }
            _ => false,
        })
        .await;

    node2
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node1_id);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let node1_channels = node1.get_network_graph_channels().await;
    assert_eq!(node1_channels, vec![]);
    let node2_channels = node2.get_network_graph_channels().await;
    assert_eq!(node2_channels, vec![]);
}

#[tokio::test]
async fn test_owned_channel_removed_from_graph_on_disconnected_public_channel() {
    do_test_owned_channel_removed_from_graph_on_disconnected(true).await;
}

#[tokio::test]
async fn test_owned_channel_removed_from_graph_on_disconnected_private_channel() {
    do_test_owned_channel_removed_from_graph_on_disconnected(false).await;
}

async fn do_test_owned_channel_saved_to_graph_on_reconnected(public: bool) {
    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 6200000000;

    let (mut node1, mut node2, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node1_funding_amount,
            node2_funding_amount,
            public,
        )
        .await;

    let node1_id = node1.peer_id.clone();
    let node2_id = node2.peer_id.clone();

    let node1_channels = node1.get_network_graph_channels().await;
    assert_ne!(node1_channels, vec![]);
    let node2_channels = node2.get_network_graph_channels().await;
    assert_ne!(node2_channels, vec![]);

    node1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(node2_id.clone()),
        ))
        .expect("node_a alive");

    node1
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node2_id);
                true
            }
            _ => false,
        })
        .await;

    node2
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _) => {
                assert_eq!(peer_id, &node1_id);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let node1_channels = node1.get_network_graph_channels().await;
    assert_eq!(node1_channels, vec![]);
    let node2_channels = node2.get_network_graph_channels().await;
    assert_eq!(node2_channels, vec![]);

    // Don't use `connect_to` here as that may consume the `ChannelCreated` event.
    // This is due to tentacle connection is async. We may actually send
    // the `ChannelCreated` event before the `PeerConnected` event.
    node1.connect_to_nonblocking(&node2).await;

    node1
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node2_id);
                true
            }
            _ => false,
        })
        .await;

    node2
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", channel_id, peer_id);
                assert_eq!(peer_id, &node1_id);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let node1_channels = node1.get_network_graph_channels().await;
    assert_ne!(node1_channels, vec![]);
    let node2_channels = node2.get_network_graph_channels().await;
    assert_ne!(node2_channels, vec![]);
}

#[tokio::test]
async fn test_owned_channel_saved_to_graph_on_reconnected_public_channel() {
    do_test_owned_channel_saved_to_graph_on_reconnected(true).await;
}

#[tokio::test]
async fn test_owned_channel_saved_to_graph_on_reconnected_private_channel() {
    do_test_owned_channel_saved_to_graph_on_reconnected(false).await;
}

async fn do_test_update_graph_balance_after_payment(public: bool) {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, public)
            .await;
    let node_a_pubkey = node_a.pubkey;
    let node_b_pubkey = node_b.pubkey;

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let test_channel_info = |channels: Vec<ChannelInfo>,
                             node_a_pubkey: Pubkey,
                             node_b_pubkey: Pubkey,
                             node_a_balance: u128,
                             node_b_balance: u128| {
        assert_eq!(channels.len(), 1);
        let channel = &channels[0];
        assert_ne!(channel.update_of_node1, None);
        assert_ne!(channel.update_of_node2, None);
        assert_ne!(channel.get_channel_update_of(node_a_pubkey), None);
        assert_ne!(channel.get_channel_update_of(node_b_pubkey), None);
        assert_eq!(
            channel
                .get_channel_update_of(node_a_pubkey)
                .unwrap()
                .outbound_liquidity,
            Some(node_a_balance)
        );
        assert_eq!(
            channel
                .get_channel_update_of(node_b_pubkey)
                .unwrap()
                .outbound_liquidity,
            Some(node_b_balance)
        );
    };

    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);
    let node_a_old_channels = node_a.get_network_graph_channels().await;
    let node_b_old_channels = node_b.get_network_graph_channels().await;
    test_channel_info(
        node_a_old_channels,
        node_a_pubkey,
        node_b_pubkey,
        node_a_old_balance,
        node_b_old_balance,
    );
    test_channel_info(
        node_b_old_channels,
        node_a_pubkey,
        node_b_pubkey,
        node_a_old_balance,
        node_b_old_balance,
    );

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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res1 = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentSessionStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // the second payment is send from node_b to node_a
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey),
                amount: Some(9999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res2 = call!(node_b.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res2.status, PaymentSessionStatus::Created);
    let payment_hash2 = res2.payment_hash;

    // sleep for 2 seconds to make sure the payment is processed
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash1, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash2, rpc_reply))
    };
    let res = call!(node_b.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    let node_a_new_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_new_balance = node_b.get_local_balance_from_channel(new_channel_id);

    // assert the balance is right,
    // node_a send 10000 to node_b, and node_b send 9999 to node_a
    // so the balance should be node_a_old_balance - 1, node_b_old_balance + 1
    assert_eq!(node_a_new_balance, node_a_old_balance - 1);
    assert_eq!(node_b_new_balance, node_b_old_balance + 1);

    let node_a_new_channels = node_a.get_network_graph_channels().await;
    let node_b_new_channels = node_b.get_network_graph_channels().await;
    test_channel_info(
        node_a_new_channels,
        node_a_pubkey,
        node_b_pubkey,
        node_a_new_balance,
        node_b_new_balance,
    );
    test_channel_info(
        node_b_new_channels,
        node_a_pubkey,
        node_b_pubkey,
        node_a_new_balance,
        node_b_new_balance,
    );
}

#[tokio::test]
async fn test_update_graph_balance_after_payment_public_channel() {
    do_test_update_graph_balance_after_payment(true).await;
}

#[tokio::test]
async fn test_update_graph_balance_after_payment_private_channel() {
    do_test_update_graph_balance_after_payment(false).await;
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
        true,
        node1_funding_amount,
        node2_funding_amount,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    let status = node3.submit_tx(funding_tx).await;
    assert_eq!(status, Status::Committed);

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    node3.stop().await;
    let channels = node3.get_network_graph_channels().await;
    assert_eq!(channels.len(), 1);
    let channel = &channels[0];
    assert_eq!(
        HashSet::from([channel.node1_peerid(), channel.node2_peerid()]),
        HashSet::from([node1.peer_id.clone(), node2.peer_id.clone()])
    );

    let nodes = node3.get_network_graph_nodes().await;
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
        true,
        node1_funding_amount,
        node2_funding_amount,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    // We should submit the transaction to node 3's chain actor here.
    // If we don't do that node 3 will deem the funding transaction unconfirmed,
    // thus refusing to save the channel to the graph.

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    node3.stop().await;
    let channels = node3.get_network_graph_channels().await;
    // No channels here as node 3 didn't think the funding transaction is confirmed.
    assert_eq!(channels.len(), 0);
}

#[tokio::test]
async fn test_network_send_payment_normal_keysend_workflow() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_a_local_balance = node_a.get_local_balance_from_channel(channel_id);
    let node_b_local_balance = node_b.get_local_balance_from_channel(channel_id);

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentSessionStatus::Created);
    let payment_hash = res.payment_hash;

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    let new_balance_node_a = node_a.get_local_balance_from_channel(channel_id);
    let new_balance_node_b = node_b.get_local_balance_from_channel(channel_id);

    assert_eq!(node_a_local_balance - new_balance_node_a, 10000);
    assert_eq!(new_balance_node_b - node_b_local_balance, 10000);

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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentSessionStatus::Created);
    let payment_hash = res.payment_hash;

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);
}

#[tokio::test]
async fn test_network_send_payment_send_each_other() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let node_a_pubkey = node_a.pubkey.clone();
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res1 = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentSessionStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // the second payment is send from node_b to node_a
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey),
                amount: Some(9999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res2 = call!(node_b.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res2.status, PaymentSessionStatus::Created);
    let payment_hash2 = res2.payment_hash;

    // sleep for 2 seconds to make sure the payment is processed
    tokio::time::sleep(tokio::time::Duration::from_millis(4000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash1, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash2, rpc_reply))
    };
    let res = call!(node_b.network_actor, message)
        .expect("node_a alive")
        .unwrap();

    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    let node_a_new_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_new_balance = node_b.get_local_balance_from_channel(new_channel_id);

    // assert the balance is right,
    // node_a send 10000 to node_b, and node_b send 9999 to node_a
    // so the balance should be node_a_old_balance - 1, node_b_old_balance + 1
    assert_eq!(node_a_new_balance, node_a_old_balance - 1);
    assert_eq!(node_b_new_balance, node_b_old_balance + 1);
}

#[tokio::test]
async fn test_network_send_payment_more_send_each_other() {
    init_tracing();

    // node_a -> node_b  add_tlc 10000
    // node_b -> node_a  add_tlc 9999
    // node_a -> node_b  add_tlc 9999
    // node_b -> node_a  add_tlc 10000
    //
    // all the add_tlc are added at the same time
    // and the final balance should be same as the initial balance

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let node_a_pubkey = node_a.pubkey.clone();
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res1 = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentSessionStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // the second payment is send from node_b to node_a
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey),
                amount: Some(9999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res2 = call!(node_b.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res2.status, PaymentSessionStatus::Created);
    let payment_hash2 = res2.payment_hash;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(9999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res3 = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res3.status, PaymentSessionStatus::Created);
    let payment_hash3 = res3.payment_hash;

    // the second payment is send from node_b to node_a
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res4 = call!(node_b.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res4.status, PaymentSessionStatus::Created);
    let payment_hash4 = res4.payment_hash;

    // sleep for 3 seconds to make sure the payment is processed
    node_a.wait_until_success(payment_hash1).await;
    node_b.wait_until_success(payment_hash2).await;
    node_a.wait_until_success(payment_hash3).await;
    node_b.wait_until_success(payment_hash4).await;

    let node_a_new_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_new_balance = node_b.get_local_balance_from_channel(new_channel_id);

    // assert the balance is right, the balance should be same as the initial balance
    assert_eq!(node_a_new_balance, node_a_old_balance);
    assert_eq!(node_b_new_balance, node_b_old_balance);
}

#[tokio::test]
async fn test_network_send_payment_send_with_ack() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res1 = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentSessionStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // DON'T WAIT FOR A MOMENT, so the second payment will meet WaitingTlcAck first
    // but payment session will handle this case

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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res2 = call!(node_a.network_actor, message).expect("node_a alive");
    // the second send_payment will be blocked by WaitingTlcAck, since we didn't wait for a moment
    assert!(res2.is_ok());
    let payment_hash2 = res2.unwrap().payment_hash;

    node_a.wait_until_success(payment_hash1).await;
    node_a.wait_until_success(payment_hash2).await;
}

#[tokio::test]
async fn test_network_send_previous_tlc_error() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, mut node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let secp = Secp256k1::new();
    let keys: Vec<Privkey> = std::iter::repeat_with(|| gen_rand_fiber_private_key().into())
        .take(1)
        .collect();
    let hops_infos = vec![
        PaymentHopData {
            amount: 2,
            expiry: 3,
            next_hop: Some(keys[0].pubkey().into()),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
        PaymentHopData {
            amount: 8,
            expiry: 9,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
    ];
    let generated_payment_hash = gen_rand_sha256_hash();

    let packet = PeeledOnionPacket::create(
        gen_rand_fiber_private_key(),
        hops_infos.clone(),
        Some(generated_payment_hash.as_ref().to_vec()),
        &secp,
    )
    .expect("create peeled packet");

    // step1: try to send a invalid onion_packet with add_tlc
    // ==================================================================================
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000,
                        payment_hash: generated_payment_hash,
                        expiry: DEFAULT_EXPIRY_DELTA + now_timestamp_as_millis_u64(),
                        hash_algorithm: HashAlgorithm::Sha256,
                        // invalid onion packet
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret.clone(),
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    };

    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let node_b_peer_id = node_b.peer_id.clone();
    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::DebugEvent(DebugEvent::AddTlcFailed(
                peer_id,
                payment_hash,
                err,
            )) => {
                assert_eq!(peer_id, &node_b_peer_id);
                assert_eq!(payment_hash, &generated_payment_hash);
                assert_eq!(err.error_code, TlcErrorCode::InvalidOnionPayload);
                true
            }
            _ => false,
        })
        .await;
    // sleep 2 seconds to make sure node_b processed handle_add_tlc_peer_message
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX

    // step2: try to send the second valid payment, expect it to success
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b.pubkey),
                amount: Some(10000),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
                tlc_expiry_limit: None,
            },
            rpc_reply,
        ))
    };

    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_a.wait_until_success(payment_hash).await;
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
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let payment_hash = gen_rand_sha256_hash();

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
                hop_hints: None,
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

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_a_local_balance = node_a.get_local_balance_from_channel(channel_id);
    let node_b_local_balance = node_b.get_local_balance_from_channel(channel_id);

    let node_b_pubkey = node_b.pubkey.clone();
    let payment_hash = gen_rand_sha256_hash();

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
                hop_hints: None,
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

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

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

    let new_balance_node_a = node_a.get_local_balance_from_channel(channel_id);
    let new_balance_node_b = node_b.get_local_balance_from_channel(channel_id);

    assert_eq!(node_a_local_balance - new_balance_node_a, 0);
    assert_eq!(new_balance_node_b - node_b_local_balance, 0);
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

    let node_b_pubkey = gen_rand_fiber_public_key().into();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(10000),
                payment_hash: Some(gen_rand_sha256_hash()),
                final_tlc_expiry_delta: None,
                tlc_expiry_limit: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
    assert!(res.err().unwrap().contains("no path found"));
}

#[tokio::test]
async fn test_network_send_payment_amount_is_too_large() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000 + MIN_RESERVED_CKB;
    let node_b_funding_amount = MIN_RESERVED_CKB + 2;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(100000000000 + 5),
                payment_hash: Some(gen_rand_sha256_hash()),
                final_tlc_expiry_delta: None,
                tlc_expiry_limit: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");

    assert!(res.is_err());
    // because the amount is too large, we will consider balance for direct channel
    // so fail to build a path
    assert!(res.err().unwrap().contains("no path found"));
}

// FIXME: this is the case send_payment with direct channels, we should handle this case
#[tokio::test]
async fn test_network_send_payment_with_dry_run() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 62000000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(100000),
                payment_hash: Some(gen_rand_sha256_hash()),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
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
                target_pubkey: Some(gen_rand_fiber_public_key()),
                amount: Some(1000 + 5),
                payment_hash: Some(gen_rand_sha256_hash()),
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
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
async fn test_send_payment_with_3_nodes() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (node_a, node_b, node_c, channel_1, channel_2) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
        true,
    )
    .await;
    let node_a_local = node_a.get_local_balance_from_channel(channel_1);
    let node_b_local_left = node_b.get_local_balance_from_channel(channel_1);
    let node_b_local_right = node_b.get_local_balance_from_channel(channel_2);
    let node_c_local = node_c.get_local_balance_from_channel(channel_2);

    // sleep for 2 seconds to make sure the channel is established
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let sent_amount = 1000000 + 5;
    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(sent_amount),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Created);
    assert!(res.fee > 0);
    // sleep for 2 seconds to make sure the payment is sent
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(res.payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    let new_node_a_local = node_a.get_local_balance_from_channel(channel_1);
    let new_node_b_left = node_b.get_local_balance_from_channel(channel_1);
    let new_node_b_right = node_b.get_local_balance_from_channel(channel_2);
    let new_node_c_local = node_c.get_local_balance_from_channel(channel_2);

    let node_a_sent = node_a_local - new_node_a_local;
    assert_eq!(node_a_sent, sent_amount + res.fee);
    let node_b_sent = node_b_local_right - new_node_b_right;
    let node_b_received = new_node_b_left - node_b_local_left;
    let node_b_got = node_b_received - node_b_sent;
    assert_eq!(node_b_got, res.fee);
    let node_c_got = new_node_c_local - node_c_local;
    assert_eq!(node_c_got, sent_amount);
}

#[tokio::test]
async fn test_send_payment_with_rev_3_nodes() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        vec![
            ((2, 1), (100000000000, 100000000000)),
            ((1, 0), (100000000000, 100000000000)),
        ]
        .as_slice(),
        3,
        true,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let [channel_1, channel_2] = channels.try_into().expect("2 channels");

    let node_c_local = node_c.get_local_balance_from_channel(channel_1);
    let node_b_local_right = node_b.get_local_balance_from_channel(channel_1);
    let node_b_local_left = node_b.get_local_balance_from_channel(channel_2);
    let node_a_local = node_a.get_local_balance_from_channel(channel_2);

    let sent_amount = 1000000 + 5;
    let node_a_pubkey = node_a.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_a_pubkey),
                amount: Some(sent_amount),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_c.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Created);
    assert!(res.fee > 0);
    // sleep for 2 seconds to make sure the payment is sent
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(res.payment_hash, rpc_reply))
    };
    let res = call!(node_c.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Success);
    assert_eq!(res.failed_error, None);

    let new_node_c_local = node_c.get_local_balance_from_channel(channel_1);
    let new_node_b_right = node_b.get_local_balance_from_channel(channel_1);
    let new_node_b_left = node_b.get_local_balance_from_channel(channel_2);
    let new_node_a_local = node_a.get_local_balance_from_channel(channel_2);

    let node_c_sent = node_c_local - new_node_c_local;
    assert_eq!(node_c_sent, sent_amount + res.fee);
    let node_b_sent = node_b_local_left - new_node_b_left;
    let node_b_received = new_node_b_right - node_b_local_right;
    let node_b_got = node_b_received - node_b_sent;
    assert_eq!(node_b_got, res.fee);
    let node_a_got = new_node_a_local - node_a_local;
    assert_eq!(node_a_got, sent_amount);
}

#[tokio::test]
async fn test_send_payment_with_max_nodes() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let nodes_num = 15;
    let last = nodes_num - 1;
    let amounts = vec![(100000000000, 100000000000); nodes_num - 1];
    let (nodes, channels) =
        create_n_nodes_with_established_channel(&amounts, nodes_num, true).await;
    let source_node = &nodes[0];
    let target_pubkey = nodes[last].pubkey.clone();

    let sender_local = nodes[0].get_local_balance_from_channel(channels[0]);
    let receiver_local = nodes[last].get_local_balance_from_channel(channels[last - 1]);

    // sleep for seconds to make sure the channel is established
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let sent_amount = 1000000 + 5;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey),
                amount: Some(sent_amount),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(source_node.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey),
                amount: Some(sent_amount),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let source_node = &nodes[0];
    let res = call!(source_node.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Created);
    assert!(res.fee > 0);

    nodes[0].wait_until_success(res.payment_hash).await;

    let sender_local_new = nodes[0].get_local_balance_from_channel(channels[0]);
    let receiver_local_new = nodes[last].get_local_balance_from_channel(channels[last - 1]);

    let sender_sent = sender_local - sender_local_new;
    let receiver_received = receiver_local_new - receiver_local;

    assert_eq!(sender_sent, sent_amount + res.fee);
    assert_eq!(receiver_received, sent_amount);
}

#[tokio::test]
async fn test_send_payment_with_3_nodes_overflow() {
    // Fix issue #361

    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (node_a, _node_b, node_c, ..) = create_3_nodes_with_established_channel(
        (1000000000 * 100000000, 1000000000 * 100000000),
        (1000000000 * 100000000, 1000000000 * 100000000),
        true,
    )
    .await;

    // sleep for 2 seconds to make sure the channel is established
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let sent_amount = 0xfffffffffffffffffffffffffffffff;
    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(sent_amount),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
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
        .contains("The payment amount (21267647932558653966460912964485513215) should be less than 1844674407370955161"));
}

#[tokio::test]
async fn test_send_payment_fail_with_3_nodes_invalid_hash() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (node_a, node_b, node_c, channel_1, channel_2) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
        true,
    )
    .await;

    let node_a_local = node_a.get_local_balance_from_channel(channel_1);
    let node_b_local_left = node_b.get_local_balance_from_channel(channel_1);
    let node_b_local_right = node_b.get_local_balance_from_channel(channel_2);
    let node_c_local = node_c.get_local_balance_from_channel(channel_2);

    // sleep for 2 seconds to make sure the channel is established
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000 + 5),
                payment_hash: Some(gen_rand_sha256_hash()), // this payment hash is not from node_c
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: None,
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Created);
    assert!(res.fee > 0);
    // sleep for 2 seconds to make sure the payment is sent
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::GetPayment(res.payment_hash, rpc_reply))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Failed);
    assert_eq!(
        res.failed_error,
        Some("IncorrectOrUnknownPaymentDetails".to_string())
    );

    let new_node_a_local = node_a.get_local_balance_from_channel(channel_1);
    let new_node_b_left = node_b.get_local_balance_from_channel(channel_1);
    let new_node_b_right = node_b.get_local_balance_from_channel(channel_2);
    let new_node_c_local = node_c.get_local_balance_from_channel(channel_2);

    let node_a_sent = node_a_local - new_node_a_local;
    assert_eq!(node_a_sent, 0);
    let node_b_sent = node_b_local_right - new_node_b_right;
    let node_b_received = new_node_b_left - node_b_local_left;
    let node_b_got = node_b_received - node_b_sent;
    assert_eq!(node_b_got, 0);
    let node_c_got = new_node_c_local - node_c_local;
    assert_eq!(node_c_got, 0);
}

#[tokio::test]
async fn test_send_payment_fail_with_3_nodes_final_tlc_expiry_delta() {
    // Fix issue #367, we should check the final_tlc_expiry_delta

    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (node_a, _node_b, node_c, ..) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
        true,
    )
    .await;

    // sleep for 2 seconds to make sure the channel is established
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000000),
                payment_hash: None,
                final_tlc_expiry_delta: Some(86400000 + 100), // should be in normal range
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentSessionStatus::Created);

    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000000),
                payment_hash: None,
                final_tlc_expiry_delta: Some(14 * 24 * 60 * 60 * 1000 + 1), // 14 days + 1 ms
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("invalid final_tlc_expiry_delta"));

    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000000),
                payment_hash: None,
                final_tlc_expiry_delta: Some(14 * 24 * 60 * 60 * 1000 - 100), // 14 days - 100, will not find a path
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("no path found"));
}

#[tokio::test]
async fn test_send_payment_fail_with_3_nodes_dry_run_fee() {
    // Fix issue #360, dryrun option should get correct fee

    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (node_a, _node_b, node_c, ..) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
        true,
    )
    .await;

    // sleep for 2 seconds to make sure the channel is established
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(2000000000),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.fee, 2000000);

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000000),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: None,
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    // expect smaller fee since amount is smaller
    assert_eq!(res.fee, 1000000);

    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000000),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: Some(res.fee), // exact the same fee limit
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.fee, 1000000);

    let node_c_pubkey = node_c.pubkey.clone();
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(1000000000),
                payment_hash: None,
                final_tlc_expiry_delta: None,
                invoice: None,
                timeout: None,
                max_fee_amount: Some(res.fee - 1), // set a smaller fee limit, path find will fail
                tlc_expiry_limit: None,
                max_parts: None,
                keysend: Some(true),
                udt_type_script: None,
                allow_self_payment: false,
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
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
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let payment_hash = gen_rand_sha256_hash();
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
                hop_hints: None,
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
                hop_hints: None,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
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
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let payment_hash = gen_rand_sha256_hash();
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
                hop_hints: None,
                dry_run: true,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    dbg!(&res);
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_stash_broadcast_messages() {
    init_tracing();

    let _span = tracing::info_span!("node", node = "test").entered();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (_node_a, _node_b, _new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        true,
    )
    .await;

    // Wait for the channel announcement to be broadcasted
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
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
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
                min_tlc_value: None,
                tlc_fee_proportional_millionths: None,
                tlc_expiry_delta: None,
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
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
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
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, tx, _) => {
                println!(
                    "Commitment tx {:?} from {:?} for channel {:?} received",
                    &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(tx.clone())
            }
            _ => None,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

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
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, tx, _) => {
                println!(
                    "Commitment tx {:?} from {:?} for channel {:?} received",
                    &tx, peer_id, channel_id
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

async fn do_test_remove_tlc_with_wrong_hash_algorithm(
    correct_algorithm: HashAlgorithm,
    wrong_algorithm: HashAlgorithm,
) {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        false,
    )
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
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
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

    let preimage = [2; 32];
    // create a new payment hash
    let digest = correct_algorithm.hash(&preimage);
    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: wrong_algorithm,
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
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
async fn do_test_channel_remote_commitment_error() {
    // https://github.com/nervosnetwork/fiber/issues/447
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let tlc_number_in_flight_limit = 5;
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (new_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        false,
        node_a_funding_amount,
        node_b_funding_amount,
        Some(tlc_number_in_flight_limit as u64),
        None,
        None,
        None,
        None,
        Some(tlc_number_in_flight_limit as u64),
        None,
        None,
        None,
        None,
    )
    .await;

    let mut all_sent = vec![];
    let mut batch_remove_count = 0;
    while batch_remove_count <= 3 {
        let preimage: [u8; 32] = gen_rand_sha256_hash().as_ref().try_into().unwrap();

        // create a new payment hash
        let hash_algorithm = HashAlgorithm::Sha256;
        let digest = hash_algorithm.hash(&preimage);
        if let Ok(add_tlc_result) = call!(node_a.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: 1000,
                            hash_algorithm,
                            payment_hash: digest.into(),
                            expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                            onion_packet: None,
                            shared_secret: NO_SHARED_SECRET.clone(),
                            previous_tlc: None,
                        },
                        rpc_reply,
                    ),
                },
            ))
        })
        .expect("node_b alive")
        {
            dbg!(&add_tlc_result);
            all_sent.push((preimage, add_tlc_result.tlc_id));
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        if all_sent.len() >= tlc_number_in_flight_limit {
            while all_sent.len() > tlc_number_in_flight_limit - 2 {
                if let Some((preimage, tlc_id)) = all_sent.iter().next().cloned() {
                    let remove_tlc_result = call!(node_b.network_actor, |rpc_reply| {
                        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                            ChannelCommandWithId {
                                channel_id: new_channel_id,
                                command: ChannelCommand::RemoveTlc(
                                    RemoveTlcCommand {
                                        id: tlc_id,
                                        reason: RemoveTlcReason::RemoveTlcFulfill(
                                            RemoveTlcFulfill {
                                                payment_preimage: Hash256::from(preimage),
                                            },
                                        ),
                                    },
                                    rpc_reply,
                                ),
                            },
                        ))
                    })
                    .expect("node_b alive");
                    dbg!(&remove_tlc_result);
                    if remove_tlc_result.is_ok()
                        || remove_tlc_result
                            .unwrap_err()
                            .to_string()
                            .contains("Trying to remove non-existing tlc")
                    {
                        all_sent.remove(0);
                    }
                }
            }
            batch_remove_count += 1;
        }
    }
}

#[tokio::test]
async fn test_network_add_two_tlcs_remove_one() {
    let _span = tracing::info_span!("node", node = "test").entered();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted

    let old_a_balance = node_a.get_local_balance_from_channel(channel_id);
    let old_b_balance = node_b.get_local_balance_from_channel(channel_id);

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let preimage_a = [1; 32];
    let algorithm = HashAlgorithm::Sha256;
    let digest = algorithm.hash(&preimage_a);

    let add_tlc_result_a = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 1000,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_a alive")
    .expect("successfully added tlc");
    eprintln!("add_tlc_result: {:?}", add_tlc_result_a);

    // if we don't wait for a while, the next add_tlc will fail with temporary failure
    let preimage_b = [2; 32];
    let algorithm = HashAlgorithm::Sha256;
    let digest = algorithm.hash(&preimage_b);
    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 2000,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive");
    assert!(add_tlc_result.is_err());

    // now wait for a while, then add a tlc again, it will success
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let add_tlc_result_b = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 2000,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive");
    assert!(add_tlc_result_b.is_ok());

    eprintln!("add_tlc_result: {:?}", add_tlc_result_b);

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    // remove tlc from node_b
    let res = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result_a.tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage_a.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully removed tlc");
    eprintln!("remove tlc result: {:?}", res);
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let new_a_balance = node_a.get_local_balance_from_channel(channel_id);
    let new_b_balance = node_b.get_local_balance_from_channel(channel_id);
    eprintln!(
        "old_a_balance: {}, new_a_balance: {}, old_b_balance: {}, new_b_balance: {}",
        old_a_balance, new_a_balance, old_b_balance, new_b_balance
    );
    assert_eq!(new_a_balance, old_a_balance - 1000);
    assert_eq!(new_b_balance, old_b_balance + 1000);

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    // remove the later tlc from node_b
    let res = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result_b.unwrap().tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage_b.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive")
    .expect("successfully removed tlc");
    eprintln!("remove tlc result: {:?}", res);
    tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;

    let new_a_balance = node_a.get_local_balance_from_channel(channel_id);
    let new_b_balance = node_b.get_local_balance_from_channel(channel_id);
    eprintln!(
        "old_a_balance: {}, new_a_balance: {}, old_b_balance: {}, new_b_balance: {}",
        old_a_balance, new_a_balance, old_b_balance, new_b_balance
    );
    assert_eq!(new_a_balance, old_a_balance - 3000);
    assert_eq!(new_b_balance, old_b_balance + 3000);
}

#[tokio::test]
async fn test_remove_tlc_with_expiry_error() {
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
        payment_hash: digest.into(),
        expiry: now_timestamp_as_millis_u64() + 10,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET.clone(),
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
        payment_hash: digest.into(),
        expiry: now_timestamp_as_millis_u64() + MAX_PAYMENT_TLC_EXPIRY_LIMIT + 20 * 1000,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET.clone(),
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
async fn do_test_add_tlc_duplicated() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, _node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = HashAlgorithm::CkbHash.hash(&preimage);
    let tlc_amount = 1000000000;

    for i in 1..=2 {
        std::thread::sleep(std::time::Duration::from_millis(400));
        // add tlc command with expiry soon
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: digest.into(),
            expiry: now_timestamp_as_millis_u64() + 10,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
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
        if i == 1 {
            assert!(add_tlc_result.is_ok());
        }
        if i == 2 {
            assert!(add_tlc_result.is_err());
        }
    }
}

#[tokio::test]
async fn do_test_add_tlc_waiting_ack() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let tlc_amount = 1000000000;

    for i in 1..=2 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
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
        if i == 2 {
            // we are sending AddTlc constantly, so we should get a WaitingTlcAck
            assert!(add_tlc_result.is_err());
            let code = add_tlc_result.unwrap_err();
            assert_eq!(code.error_code, TlcErrorCode::TemporaryChannelFailure);
        } else {
            assert!(add_tlc_result.is_ok());
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    // send from b to a
    for i in 1..=2 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            previous_tlc: None,
            shared_secret: NO_SHARED_SECRET.clone(),
        };
        let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
                },
            ))
        })
        .expect("node_b alive");
        if i == 2 {
            // we are sending AddTlc constantly, so we should get a WaitingTlcAck
            assert!(add_tlc_result.is_err());
            let code = add_tlc_result.unwrap_err();
            assert_eq!(code.error_code, TlcErrorCode::TemporaryChannelFailure);
        } else {
            eprintln!("add_tlc_result: {:?}", add_tlc_result);
            assert!(add_tlc_result.is_ok());
        }
    }
}

#[tokio::test]
async fn do_test_add_tlc_with_number_limit() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let node_a_max_tlc_number = 2;
    let (new_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        true,
        node_a_funding_amount,
        node_b_funding_amount,
        Some(node_a_max_tlc_number),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let tlc_amount = 1000000000;

    // A -> B will have tlc number limit 2
    for i in 1..=node_a_max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
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
        .expect("source node alive");
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        if i == node_a_max_tlc_number + 1 {
            assert!(add_tlc_result.is_err());
            let code = add_tlc_result.unwrap_err();
            assert_eq!(code.error_code, TlcErrorCode::TemporaryChannelFailure);
        } else {
            dbg!(&add_tlc_result);
            assert!(add_tlc_result.is_ok());
        }
    }

    // B -> A can still add tlc
    for _ in 1..=node_a_max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
            previous_tlc: None,
        };
        let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
                },
            ))
        })
        .expect("source node alive");
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        dbg!(&add_tlc_result);
        assert!(add_tlc_result.is_ok());
    }
}

#[tokio::test]
async fn do_test_add_tlc_number_limit_reverse() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let node_b_max_tlc_number = 2;
    let (new_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        true,
        node_a_funding_amount,
        node_b_funding_amount,
        None,
        None,
        None,
        None,
        None,
        Some(node_b_max_tlc_number),
        None,
        None,
        None,
        None,
    )
    .await;

    let tlc_amount = 1000000000;
    // B -> A will have tlc number limit 2
    for i in 1..=node_b_max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
            previous_tlc: None,
        };
        let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
                },
            ))
        })
        .expect("source node alive");
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        if i == node_b_max_tlc_number + 1 {
            assert!(add_tlc_result.is_err());
            let code = add_tlc_result.unwrap_err();
            assert_eq!(code.error_code, TlcErrorCode::TemporaryChannelFailure);
        } else {
            dbg!(&add_tlc_result);
            assert!(add_tlc_result.is_ok());
        }
    }

    // A -> B can still add tlc
    for _ in 1..=node_b_max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
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
        .expect("source node alive");
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        dbg!(&add_tlc_result);
        assert!(add_tlc_result.is_ok());
    }
}

#[tokio::test]
async fn do_test_add_tlc_value_limit() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let max_tlc_number = 3;
    let (new_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        true,
        node_a_funding_amount,
        node_b_funding_amount,
        None,
        Some(3000000000),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let tlc_amount = 1000000000;

    // A -> B have tlc value limit 3_000_000_000
    for i in 1..=max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
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
        // sleep for a while to make sure the AddTlc processed by both party
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        if i == max_tlc_number + 1 {
            assert!(add_tlc_result.is_err());
            let code = add_tlc_result.unwrap_err();

            assert_eq!(code.error_code, TlcErrorCode::TemporaryChannelFailure);
        } else {
            assert!(add_tlc_result.is_ok());
        }
    }

    // B -> A can still add tlc
    for _ in 1..=max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash().into(),
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET.clone(),
            previous_tlc: None,
        };
        let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
                },
            ))
        })
        .expect("node_b alive");
        // sleep for a while to make sure the AddTlc processed by both party
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(add_tlc_result.is_ok());
    }
}

#[tokio::test]
async fn do_test_add_tlc_min_tlc_value_limit() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (new_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        true,
        node_a_funding_amount,
        node_b_funding_amount,
        None,
        None,
        None,
        Some(100),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // A -> B will be no limit
    let tlc_amount = 200;
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: gen_rand_sha256_hash().into(),
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET.clone(),
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
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    assert!(add_tlc_result.is_ok());

    // B -> A can still able to send amount less than 100
    // since it's not under the tlc relay context
    let tlc_amount = 99;
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: gen_rand_sha256_hash().into(),
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET.clone(),
    };
    let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
            },
        ))
    })
    .expect("node_b alive");
    assert!(add_tlc_result.is_ok());
    // sleep for a while to make sure the AddTlc processed by both party
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // B -> A can send at least 100
    let tlc_amount = 100;
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: gen_rand_sha256_hash().into(),
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET.clone(),
    };
    let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(add_tlc_command, rpc_reply),
            },
        ))
    })
    .expect("node_b alive");
    eprintln!("add_local_tlc_result: {:?}", add_tlc_result);
    assert!(add_tlc_result.is_ok());
}

#[tokio::test]
async fn test_channel_update_tlc_expiry() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (new_channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        true,
        node_a_funding_amount,
        node_b_funding_amount,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    // update channel with new tlc_expiry_delta which is too small
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: Some(1000),
                        tlc_minimum_value: None,
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .unwrap();
    assert!(update_result.is_err());
    assert!(update_result
        .unwrap_err()
        .to_string()
        .contains("TLC expiry delta is too small"));

    // update channel with new tlc_expiry_delta which is ok
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: Some(900000),
                        tlc_minimum_value: None,
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .unwrap();
    assert!(update_result.is_ok());
}

#[tokio::test]
async fn test_forward_payment_channel_disabled() {
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
        true,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let [_channel_a_b, channel_b_c] = channels.try_into().expect("2 channels");

    let node_c_pubkey = node_c.pubkey.clone();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let tlc_amount = 100;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentSessionStatus::Created);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // update channel to disable it from node_b
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_b_c,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(false),
                        tlc_expiry_delta: None,
                        tlc_minimum_value: None,
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .unwrap();
    assert!(update_result.is_ok());
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
}

#[tokio::test]
async fn test_forward_payment_tlc_minimum_value() {
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
        true,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let [channel_a_b, channel_b_c] = channels.try_into().expect("2 channels");

    let node_c_pubkey = node_c.pubkey.clone();
    let node_b_pubkey = node_b.pubkey.clone();
    let tlc_amount = 99;

    // update B's ChannelUpdate in channel_b_c with tlc_minimum_value set to our tlc_amount
    // this is used to override the default tlc_minimum_value value.
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_b_c,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: None,
                        tlc_minimum_value: Some(tlc_amount),
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .unwrap();
    assert!(update_result.is_ok());
    // sleep for a while to make sure the Update processed by both party
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // A -> C now will be with no limit
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .unwrap();
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentSessionStatus::Created);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // update B's ChannelUpdate in channel_b_c with new tlc_minimum_value
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_b_c,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: None,
                        tlc_minimum_value: Some(100),
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .unwrap();
    assert!(update_result.is_ok());
    // sleep for a while to make sure the Update processed by both party
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // AddTlc from A to B is OK because we didn't update the channel
    let tlc_amount = 99;
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: gen_rand_sha256_hash().into(),
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET.clone(),
    };
    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_a_b,
                command: ChannelCommand::AddTlc(add_tlc_command.clone(), rpc_reply),
            },
        ))
    })
    .expect("node_b alive");
    assert!(add_tlc_result.is_ok());
    // sleep for a while to make sure the AddTlc processed by both party
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // AddTlc from B to C is not OK because the forwarding value is too small
    let add_tlc_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_b_c,
                command: ChannelCommand::AddTlc(add_tlc_command.clone(), rpc_reply),
            },
        ))
    })
    .expect("node_b alive");
    assert!(add_tlc_result.is_err());
    // sleep for a while to make sure the AddTlc processed by both party
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // sending payment from A to B is OK because this has nothing to do with the channel_a_b.
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_b alive");
    assert!(res.is_ok());
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // sending payment from B to C is not OK because the forwarding value is too small
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_b.network_actor, message).expect("node_b alive");
    assert!(res.is_err());
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // sending payment from A to C should fail because the forwarding value is too small
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Failed to build route, PathFind error: no path found"));
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
}

#[tokio::test]
async fn test_send_payment_with_outdated_fee_rate() {
    init_tracing();
    let (nodes, _) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
        true,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    let node_b_pubkey = node_b.pubkey.clone();
    let node_c_pubkey = node_c.pubkey.clone();
    let hash_set: HashSet<_> = [node_b_pubkey, node_c_pubkey].into_iter().collect();

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    node_a
        .with_network_graph_mut(|graph| {
            for channel in graph.channels.values_mut() {
                tracing::debug!("channel: {:?}", channel);
                if hash_set.contains(&channel.node1()) && hash_set.contains(&channel.node2()) {
                    let channel_update = if channel.node1() == node_b_pubkey {
                        channel.update_of_node1.as_mut().unwrap()
                    } else {
                        channel.update_of_node2.as_mut().unwrap()
                    };
                    tracing::debug!("channel_update: {:?}", channel_update);
                    channel_update.fee_rate = 0;
                }
            }
        })
        .await;

    // sending payment from A to C should fail because the forwarding value is too small
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_c_pubkey),
                amount: Some(10000000000),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(
        res.is_ok(),
        "Send payment should be ok because we can find path: {:?}",
        res
    );
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    // The payment should fail because our fee rate is too low.
    node_a.wait_until_failed(payment_hash).await;
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

    let (mut node_a, mut node_b, new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            false,
        )
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
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
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
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
                min_tlc_value: None,
                tlc_fee_proportional_millionths: None,
                tlc_expiry_delta: None,
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
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, tx, _) => {
                println!(
                    "Commitment tx {:?} from {:?} for channel {:?} received",
                    &tx, peer_id, channel_id
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

    let revocation_data = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RevokeAndAckReceived(
                peer_id,
                channel_id,
                revocation_data,
                _settlement_data,
            ) => {
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                assert_eq!(revocation_data.commitment_number, 0u64);
                Some(revocation_data.clone())
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
        .output(revocation_data.output)
        .output_data(revocation_data.output_data)
        .build();

    let empty_witness_args = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    let witness = [
        empty_witness_args.to_vec(),
        vec![0xFF],
        revocation_data.commitment_number.to_be_bytes().to_vec(),
        revocation_data.x_only_aggregated_pubkey.to_vec(),
        revocation_data.aggregated_signature.serialize().to_vec(),
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
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
                min_tlc_value: None,
                tlc_fee_proportional_millionths: None,
                tlc_expiry_delta: None,
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
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, tx, _) => {
                println!(
                    "Commitment tx {:?} from {:?} for channel {:?} received",
                    &tx, peer_id, channel_id
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
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, tx, _) => {
                println!(
                    "Commitment tx {:?} from {:?} for channel {:?} received",
                    &tx, peer_id, channel_id
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
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
                min_tlc_value: None,
                tlc_fee_proportional_millionths: None,
                tlc_expiry_delta: None,
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
    let (mut node_a, mut node_b, channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(16200000000, 6200000000, true).await;

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
    let (node_a, _node_b, channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(amount_a, amount_b, true).await;

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
async fn test_connect_to_peers_with_mutual_channel_on_restart_1() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    // sleep for a while to make sure this test works both for release mode
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    node_a.restart().await;

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    ).await;
    node_a
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::DebugEvent(DebugEvent::Common(
                    info
                )) if "Reestablished channel in ChannelReady" == info)
        })
        .await;
    node_b
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::DebugEvent(DebugEvent::Common(
                    info
                )) if "Reestablished channel in ChannelReady" == info)
        })
        .await;
}

#[tokio::test]
async fn test_connect_to_peers_with_mutual_channel_on_restart_2() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    // sleep for a while to make sure this test works both for release mode
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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
async fn test_send_payment_with_node_restart_then_resend_add_tlc() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let node_b_pubkey = node_b.pubkey.clone();
    let tlc_amount = 99;
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(node_b_pubkey),
                amount: Some(tlc_amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(node_a.network_actor, message).expect("node_a alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_b.stop().await;

    let payment_status = node_a.get_payment_status(payment_hash).await;
    assert_eq!(payment_status, PaymentSessionStatus::Inflight);

    node_b.start().await;

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    )
    .await;

    node_a
        .expect_event(|event| {
            matches!(
            event,
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(
                info
            )) if "resend add tlc" == info)
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let payment_status = node_a.get_payment_status(payment_hash).await;
    assert_eq!(payment_status, PaymentSessionStatus::Success);
}

#[tokio::test]
async fn test_node_reestablish_resend_remove_tlc() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (mut node_a, mut node_b, new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let node_a_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let preimage = [2; 32];
    // create a new payment hash
    let payment_hash = HashAlgorithm::CkbHash.hash(&preimage);

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 1000,
                        hash_algorithm: HashAlgorithm::CkbHash,
                        payment_hash: payment_hash.clone().into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET.clone(),
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

    node_a.stop().await;

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
    assert!(remove_tlc_result.is_ok());

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // assert balance does not changed since remove tlc is not processed by node_a
    let new_node_a_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let new_node_b_balance = node_b.get_local_balance_from_channel(new_channel_id);
    assert_eq!(node_a_balance, new_node_a_balance);
    assert_eq!(node_b_balance, new_node_b_balance);

    node_a.start().await;
    node_b
        .expect_event(|event| {
            matches!(
        event,
        NetworkServiceEvent::DebugEvent(DebugEvent::Common(
            info
        )) if "resend remove tlc" == info)
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // assert balance changed since remove tlc is processed by node_a after node_b resending remove tlc
    let new_node_a_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let new_node_b_balance = node_b.get_local_balance_from_channel(new_channel_id);
    assert_eq!(node_a_balance - 1000, new_node_a_balance);
    assert_eq!(node_b_balance + 1000, new_node_b_balance);
    eprintln!(
        "node_a_balance: {}, new_node_a_balance: {}",
        node_a_balance, new_node_a_balance
    );
    eprintln!(
        "node_b_balance: {}, new_node_b_balance: {}",
        node_b_balance, new_node_b_balance
    );
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
            .base_dir_prefix(&format!("test-fnn-node-{}-", i))
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

    // create a private channel for testing shutdown,
    // https://github.com/nervosnetwork/fiber/issues/431
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

    node_a
        .expect_event(|event| matches!(event, NetworkServiceEvent::DebugEvent(DebugEvent::Common(message)) if message == "ChannelClosed"))
        .await;

    node_b
        .expect_event(|event| matches!(event, NetworkServiceEvent::DebugEvent(DebugEvent::Common(message)) if message == "ChannelClosed"))
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

#[tokio::test]
async fn test_shutdown_channel_network_graph_will_not_sync_private_channel() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, _channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    // sleep for 1 second
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let network_nodes = node_a.get_network_nodes().await;
    assert_eq!(network_nodes.len(), 2);

    let network_nodes = node_b.get_network_nodes().await;
    assert_eq!(network_nodes.len(), 2);

    let network_channels = node_a.get_network_channels().await;
    assert_eq!(network_channels.len(), 0);

    let network_channels = node_b.get_network_channels().await;
    assert_eq!(network_channels.len(), 0);
}

#[tokio::test]
async fn test_shutdown_channel_network_graph_with_sync_up() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 6200000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    // sleep for 1 second
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let network_nodes = node_a.get_network_nodes().await;
    assert_eq!(network_nodes.len(), 2);

    let network_nodes = node_b.get_network_nodes().await;
    assert_eq!(network_nodes.len(), 2);

    let network_channels = node_a.get_network_channels().await;
    assert_eq!(network_channels.len(), 1);

    let network_channels = node_b.get_network_channels().await;
    assert_eq!(network_channels.len(), 1);

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channel_id,
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

    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let network_nodes = node_a.get_network_nodes().await;
    assert_eq!(network_nodes.len(), 2);

    let network_nodes = node_b.get_network_nodes().await;
    assert_eq!(network_nodes.len(), 2);

    let network_channels = node_a.get_network_channels().await;
    assert_eq!(network_channels.len(), 0);

    let network_channels = node_b.get_network_channels().await;
    assert_eq!(network_channels.len(), 0);
}

#[tokio::test]
async fn test_send_payment_with_channel_balance_error() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let nodes_num = 4;
    let amounts = vec![(100000000000, 100000000000); nodes_num - 1];
    let (nodes, channels) =
        create_n_nodes_with_established_channel(&amounts, nodes_num, true).await;
    let [node_0, _node_1, mut node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(3000),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    // sleep for a while
    source_node.wait_until_success(payment_hash).await;

    node_2.update_channel_local_balance(channels[2], 100).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(3000),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment failed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    let res = source_node.get_payment_result(payment_hash).await;

    assert_eq!(res.status, PaymentSessionStatus::Failed);
    assert!(res.failed_error.unwrap().contains("Failed to build route"));

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retried_times, 2);
}

#[tokio::test]
async fn test_send_payment_with_disable_channel() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let nodes_num = 4;
    let amounts = vec![(100000000000, 100000000000); nodes_num - 1];
    let (nodes, channels) =
        create_n_nodes_with_established_channel(&amounts, nodes_num, true).await;
    let [node_0, _node_1, mut node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // begin to set channel disable, but do not notify the network
    node_2.disable_channel_stealthy(channels[1]).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(3000),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment failed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retried_times, 2);
}

#[tokio::test]
async fn test_send_payment_with_multiple_edges_in_middle_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_1 and node_2, they are all with the same meta information except the later one has more capacity
    // path finding will try the channel with larger capacity first, so we assert the payment retry times is 1
    // the send payment should be succeed
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (MIN_RESERVED_CKB + 900, 5200000000)),
            ((1, 2), (MIN_RESERVED_CKB + 1000, 5200000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_success(payment_hash).await;
    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retried_times, 1);
}

#[tokio::test]
async fn test_send_payment_with_all_failed_middle_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_1 and node_2
    // they liquid capacity is enough for send payment, but actual balance are both not enough
    // path finding will all try them but all failed, so we assert the payment retry times is 3
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (MIN_RESERVED_CKB + 900, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (MIN_RESERVED_CKB + 910, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to failed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_failed(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    assert!(node_0.get_triggered_unexpected_events().await.is_empty());
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retried_times, 3);
}

#[tokio::test]
async fn test_send_payment_with_multiple_edges_can_succeed_in_retry() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_1 and node_2, they are all with the same meta information except the later one has more capacity
    // but even channel_2's capacity is larger, the to_local_amount is not enough for the payment
    // path finding will retry the first channel and the send payment should be succeed
    // the payment retry times should be 2
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (MIN_RESERVED_CKB + 1000, 5200000000)),
            ((1, 2), (MIN_RESERVED_CKB + 900, 6200000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retried_times, 2);
}

#[tokio::test]
async fn test_send_payment_with_final_hop_multiple_edges_in_middle_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_2 and node_3, they are all with the same meta information except the later one has more capacity
    // path finding will try the channel with larger capacity first, so we assert the payment retry times is 1
    // the send payment should be succeed
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 900, 5200000000)),
            ((2, 3), (MIN_RESERVED_CKB + 1000, 5200000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retried_times, 1);
}

#[tokio::test]
async fn test_send_payment_with_final_all_failed_middle_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_2 and node_3
    // they liquid capacity is enough for send payment, but actual balance are both not enough
    // path finding will all try them but all failed, so we assert the payment retry times is 3
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 900, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 910, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Failed, Some(3))
        .await;
}

#[tokio::test]
async fn test_send_payment_with_final_multiple_edges_can_succeed_in_retry() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_2 and node_3, they are all with the same meta information except the later one has more capacity
    // but even channel_2's capacity is larger, the to_local_amount is not enough for the payment
    // path finding will retry the first channel and the send payment should be succeed
    // the payment retry times should be 2
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 1000, 5200000000)),
            ((2, 3), (MIN_RESERVED_CKB + 900, 6200000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(2))
        .await;
}

#[tokio::test]
async fn test_send_payment_with_first_hop_failed_with_fee() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            // even 1000 > 999, but it's not enough for fee, and this is the direct channel
            // so we can check the actual balance of channel
            // the payment will fail
            ((0, 1), (MIN_RESERVED_CKB + 1000, 5200000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to fail
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Failed to build route"));
}

#[tokio::test]
async fn test_send_payment_succeed_with_multiple_edges_in_first_hop() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_0 and node_1, they are all with the same meta information except the later one has more capacity
    // path finding will try the channel with larger capacity first, so we assert the payment retry times is 1
    // the send payment should be succeed
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 900, 5200000000)),
            ((0, 1), (MIN_RESERVED_CKB + 1001, 5200000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_with_first_hop_all_failed() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_0 and node_1
    // they liquid capacity is enough for send payment, but actual balance are both not enough
    // path finding will fail in the first time of send payment
    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 900, MIN_RESERVED_CKB + 1000)),
            ((0, 1), (MIN_RESERVED_CKB + 910, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;
    let target_pubkey = node_3.pubkey.clone();

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to faile
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Failed to build route"));
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_direct_channel_info_first_hop() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_0 and node_1
    // the path finding will first try the channel with larger capacity,
    // but we manually set the to_local_amount to smaller value for testing
    // path finding will get the direct channel info with actual balance of channel,
    // so it will try the channel with smaller capacity and the payment will succeed
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((0, 1), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();

    // manually update the channel's to_local_amount
    source_node
        .update_channel_local_balance(channels[0], 100)
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(999),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message).expect("source_node alive");
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_retry_in_middle_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_2 and node_3
    // the path finding will first try the channel with larger capacity,
    // but we manually set the to_local_amount to smaller value for testing
    // path finding will get a temporary failure in the first try and retry the second channel
    // so it will try the channel with smaller capacity and the payment will succeed
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, mut node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let node_0_amount = source_node.get_local_balance_from_channel(channels[0]);
    let target_pubkey = node_3.pubkey.clone();

    // manually update the channel's to_local_amount
    node_2.update_channel_local_balance(channels[2], 100).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let amount = 999;
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
            SendPaymentCommand {
                target_pubkey: Some(target_pubkey.clone()),
                amount: Some(amount),
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
                hop_hints: None,
                dry_run: false,
            },
            rpc_reply,
        ))
    };

    // expect send payment to succeed
    let res = call!(source_node.network_actor, message)
        .expect("source_node alive")
        .unwrap();

    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;

    let fee = res.fee;
    eprintln!("fee: {:?}", fee);
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(2))
        .await;

    let new_node0_amount = source_node.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_amount - amount - fee, new_node0_amount);
}

#[tokio::test]
async fn test_send_payment_will_fail_with_last_hop_info_in_add_tlc_peer() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // we have two chaneels between node_2 and node_3
    // the path finding will first try the channel with larger capacity,
    // but we manually set the to_remote_amount for node_3 to a larger amount,
    // this will make node3 trigger error in add_tlc_peer and got an Musig2VerifyError(BadSignature)
    // the send_payment will failed with retry times of 1
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, mut node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();

    // manually update the channel's to_remote_amount
    node_3
        .update_channel_remote_balance(channels[2], 100000000)
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(999),
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
            hop_hints: None,
            dry_run: false,
        })
        .await;

    // expect send payment to failed
    assert!(res.is_ok());

    node_3
        .expect_event(|event| match event {
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(error)) => {
                assert!(error.contains("Musig2VerifyError(BadSignature)"));
                true
            }
            _ => false,
        })
        .await;

    let payment_hash = res.unwrap().payment_hash;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Inflight, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_will_fail_with_invoice_not_generated_by_target() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();

    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success")
        .to_string();

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(100),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(invoice.clone()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    // expect send payment to succeed
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Failed, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_valid_invoice() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, mut node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();
    let old_amount = node_3.get_local_balance_from_channel(channels[2]);

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage.clone())
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    node_3.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(100),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(ckb_invoice.to_string()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    // expect send payment to succeed
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    let new_amount = node_3.get_local_balance_from_channel(channels[2]);
    assert_eq!(new_amount, old_amount + 100);
    assert_eq!(
        node_3.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Paid)
    );
}

#[tokio::test]
async fn test_send_payment_will_fail_with_no_invoice_preimage() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, mut node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();
    let old_amount = node_3.get_local_balance_from_channel(channels[2]);

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage.clone())
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    // insert invoice without preimage
    node_3.insert_invoice(ckb_invoice.clone(), None);

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(100),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(ckb_invoice.to_string()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    // expect send payment to failed because we can not find preimage
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Failed, Some(1))
        .await;

    let new_amount = node_3.get_local_balance_from_channel(channels[2]);
    assert_eq!(new_amount, old_amount);

    // we should never update the invoice status if there is an error
    assert_eq!(
        node_3.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Open)
    );
}

#[tokio::test]
async fn test_send_payment_will_fail_with_cancelled_invoice() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, mut node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();
    let old_amount = node_3.get_local_balance_from_channel(channels[2]);

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage.clone())
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    node_3.insert_invoice(ckb_invoice.clone(), Some(preimage));
    node_3.cancel_invoice(ckb_invoice.payment_hash());
    // sleep for a while
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(100),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(ckb_invoice.to_string()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Failed, Some(1))
        .await;

    let new_amount = node_3.get_local_balance_from_channel(channels[2]);
    assert_eq!(new_amount, old_amount);
    assert_eq!(
        node_3.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Cancelled)
    );
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_large_tlc_expiry_limit() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    // from https://github.com/nervosnetwork/fiber/issues/367

    let (nodes, _channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
        true,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey.clone();

    let expected_minimal_tlc_expiry_limit = (24 * 60 * 60 * 1000) * 3;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(999),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: Some(expected_minimal_tlc_expiry_limit - 1),
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    assert!(res.unwrap_err().contains("Failed to build route"));

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey.clone()),
            amount: Some(999),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: Some(expected_minimal_tlc_expiry_limit),
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    // expect send payment to succeed
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    source_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;
}
