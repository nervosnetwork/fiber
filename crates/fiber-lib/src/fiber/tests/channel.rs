use crate::ckb::tests::test_utils::complete_commitment_tx;
use crate::fiber::channel::{
    AddTlcResponse, ChannelState, CloseFlags, NegotiatingFundingFlags, OutboundTlcStatus, TLCId,
    TlcStatus, UpdateCommand, MAX_COMMITMENT_DELAY_EPOCHS, MIN_COMMITMENT_DELAY_EPOCHS,
    XUDT_COMPATIBLE_WITNESS,
};
use crate::fiber::config::{
    DEFAULT_COMMITMENT_DELAY_EPOCHS, DEFAULT_FINAL_TLC_EXPIRY_DELTA, DEFAULT_TLC_EXPIRY_DELTA,
    MAX_PAYMENT_TLC_EXPIRY_LIMIT, MILLI_SECONDS_PER_EPOCH, MIN_TLC_EXPIRY_DELTA,
};
use crate::fiber::features::FeatureVector;
use crate::fiber::graph::ChannelInfo;
use crate::fiber::network::{DebugEvent, FiberMessageWithPeerId, PeerDisconnectReason};
use crate::fiber::payment::{PaymentStatus, SendPaymentCommand};
use crate::fiber::types::{
    AddTlc, FiberMessage, Hash256, Init, PaymentHopData, PeeledPaymentOnionPacket, Pubkey, TlcErr,
    TlcErrorCode, NO_SHARED_SECRET,
};
use crate::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder};
use crate::test_utils::{init_tracing, NetworkNode};
use crate::tests::test_utils::*;
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
        types::{Privkey, RemoveTlcFulfill, RemoveTlcReason},
        NetworkActorCommand, NetworkActorMessage,
    },
    gen_rand_fiber_private_key, gen_rand_fiber_public_key, gen_rand_sha256_hash,
    now_timestamp_as_millis_u64, NetworkServiceEvent,
};
use ckb_types::core::EpochNumberWithFraction;
use ckb_types::{
    core::{tx_pool::TxStatus, FeeRate},
    packed::{CellInput, Script, Transaction},
    prelude::{AsTransactionBuilder, Builder, Entity, Pack, Unpack},
};
use musig2::secp::Point;
use musig2::KeyAggContext;
use ractor::call;
use secp256k1::Secp256k1;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{debug, error};

#[tokio::test]
// Not supported on wasm: require filesystem access
async fn test_connect_to_other_node() {
    let mut node_a = NetworkNode::new().await;
    let mut node_b = NetworkNode::new().await;
    node_a.connect_to(&mut node_b).await;
}

#[tokio::test]
async fn test_restart_network_node() {
    let mut node = NetworkNode::new().await;
    node.restart().await;
    node.expect_debug_event("network actor started").await;
}

#[test]
fn test_per_commitment_point_and_secret_consistency() {
    init_tracing();

    let signer = InMemorySigner::generate_from_seed(&[1; 32]);
    assert_eq!(
        signer.get_commitment_point(0),
        Privkey::from(&signer.get_commitment_secret(0)).pubkey()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
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
    let node_b_funding_amount = 11800000000;

    let (_node_a, _node_b, _new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_send_init_msg_with_different_chain_hash() {
    init_tracing();

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let dummy_err_chain_hash = Hash256::from([1; 32]);
    node_a.send_init_peer_message(
        node_b.peer_id.clone(),
        Init {
            features: FeatureVector::default(),
            chain_hash: dummy_err_chain_hash,
        },
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _reason) => {
                assert_eq!(peer_id, &node_b.peer_id);
                true
            }
            _ => false,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(peer_id, _reason) => {
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;
}

#[tokio::test]
async fn test_create_channel_with_remote_tlc_info() {
    async fn test(public: bool) {
        let node_a_funding_amount = 100000000000;
        let node_b_funding_amount = 11800000000;

        let (node_a, node_b, channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            public,
        )
        .await;

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
    let node_b_funding_amount = 11800000000;

    let (_node_a, _node_b, _new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        true,
    )
    .await;
}

async fn do_test_owned_channel_saved_to_the_owner_graph(public: bool) {
    let node1_funding_amount = 100000000000;
    let node2_funding_amount = 11800000000;

    let (mut node1, mut node2, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node1_funding_amount,
            node2_funding_amount,
            public,
        )
        .await;

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
async fn test_create_channel_with_too_large_amounts() {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let params = ChannelParameters {
        node_a_funding_amount: u64::MAX as u128 - 1,
        node_b_funding_amount: MIN_RESERVED_CKB,
        ..Default::default()
    };
    let res = create_channel_with_nodes(&mut node_a, &mut node_b, params).await;
    assert!(res.is_err(), "Create channel failed: {:?}", res);
    assert!(res.unwrap_err().to_string().contains(
        "The total funding amount (18446744063809551614) should be less than 18446744053909551615"
    ));

    let params = ChannelParameters {
        node_a_funding_amount: MIN_RESERVED_CKB,
        node_b_funding_amount: u64::MAX as u128 - 1,
        ..Default::default()
    };
    let res = create_channel_with_nodes(&mut node_a, &mut node_b, params).await;
    assert!(res.is_err(), "Create channel failed: {:?}", res);
    assert!(res.unwrap_err().to_string().contains(
        "The total funding amount (18446744063809551614) should be less than 18446744053909551615"
    ));

    let params = ChannelParameters {
        node_a_funding_amount: u128::MAX - 100,
        node_b_funding_amount: 101,
        funding_udt_type_script: Some(Script::default()),
        ..Default::default()
    };
    let res = create_channel_with_nodes(&mut node_a, &mut node_b, params).await;
    assert!(res.is_err(), "Create channel failed: {:?}", res);
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("The total UDT funding amount should be less"));
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
    let node2_funding_amount = 11800000000;

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
            NetworkActorCommand::DisconnectPeer(node2_id.clone(), PeerDisconnectReason::Requested),
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
    let node2_funding_amount = 11800000000;

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
            NetworkActorCommand::DisconnectPeer(node2_id.clone(), PeerDisconnectReason::Requested),
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

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, public)
            .await;
    let node_a_pubkey = node_a.pubkey;
    let node_b_pubkey = node_b.pubkey;

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

    let res1 = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // the second payment is send from node_b to node_a
    let res2 = node_b
        .send_payment_keysend(&node_a, 9999, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res2.status, PaymentStatus::Created);
    let payment_hash2 = res2.payment_hash;

    // make sure the payment is processed
    node_a.wait_until_success(payment_hash1).await;
    node_b.wait_until_success(payment_hash2).await;

    assert_eq!(
        node_a.get_payment_status(payment_hash1).await,
        PaymentStatus::Success
    );
    assert_eq!(
        node_b.get_payment_status(payment_hash2).await,
        PaymentStatus::Success
    );

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

    assert!(node_a.get_triggered_unexpected_events().await.is_empty());
    assert!(node_b.get_triggered_unexpected_events().await.is_empty());
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
    let node2_funding_amount = 11800000000;

    let [mut node1, mut node2, mut node3] = NetworkNode::new_n_interconnected_nodes().await;
    let (_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node1,
        &mut node2,
        ChannelParameters {
            public: true,
            node_a_funding_amount: node1_funding_amount,
            node_b_funding_amount: node2_funding_amount,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node1
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");
    assert!(matches!(
        node3.submit_tx(funding_tx).await,
        TxStatus::Committed(..)
    ));

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
    let node2_funding_amount = 11800000000;

    let [mut node1, mut node2, mut node3] = NetworkNode::new_n_interconnected_nodes().await;
    let (_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node1,
        &mut node2,
        ChannelParameters::new(node1_funding_amount, node2_funding_amount),
    )
    .await;

    // We should submit the transaction to node 3's chain actor here.
    // If we don't do that node 3 will deem the funding transaction unconfirmed,
    // thus refusing to save the channel to the graph.

    node3.stop().await;
    let channels = node3.get_network_graph_channels().await;
    // No channels here as node 3 didn't think the funding transaction is confirmed.
    assert_eq!(channels.len(), 0);
}

#[tokio::test]
async fn test_network_send_payment_normal_keysend_workflow() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_a_local_balance = node_a.get_local_balance_from_channel(channel_id);
    let node_b_local_balance = node_b.get_local_balance_from_channel(channel_id);

    let res = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentStatus::Created);
    let payment_hash = res.payment_hash;

    node_a.wait_until_success(payment_hash).await;

    let new_balance_node_a = node_a.get_local_balance_from_channel(channel_id);
    let new_balance_node_b = node_b.get_local_balance_from_channel(channel_id);

    assert_eq!(node_a_local_balance - new_balance_node_a, 10000);
    assert_eq!(new_balance_node_b - node_b_local_balance, 10000);
    assert_eq!(
        node_a.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );

    // we can make the same payment again, since payment_hash will be generated randomly
    let res = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentStatus::Created);
    let payment_hash = res.payment_hash;

    assert_eq!(res.failed_error, None);
    node_a.wait_until_success(payment_hash).await;

    assert!(node_a.get_payment_preimage(&payment_hash).is_none());
}

#[tokio::test]
async fn test_network_send_payment_send_each_other() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let res1 = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // the second payment is send from node_b to node_a
    let res2 = node_b
        .send_payment_keysend(&node_a, 9999, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res2.status, PaymentStatus::Created);
    let payment_hash2 = res2.payment_hash;

    node_a.wait_until_success(payment_hash1).await;
    node_b.wait_until_success(payment_hash2).await;

    assert_eq!(
        node_a.get_payment_status(payment_hash1).await,
        PaymentStatus::Success
    );
    assert_eq!(
        node_b.get_payment_status(payment_hash2).await,
        PaymentStatus::Success
    );

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

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let res1 = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // the second payment is send from node_b to node_a
    let res2 = node_b
        .send_payment_keysend(&node_a, 9999, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res2.status, PaymentStatus::Created);
    let payment_hash2 = res2.payment_hash;

    let res3 = node_a
        .send_payment_keysend(&node_b, 9999, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res3.status, PaymentStatus::Created);
    let payment_hash3 = res3.payment_hash;

    // the second payment is send from node_b to node_a
    let res4 = node_b
        .send_payment_keysend(&node_a, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res4.status, PaymentStatus::Created);
    let payment_hash4 = res4.payment_hash;

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

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let res1 = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res1.status, PaymentStatus::Created);
    let payment_hash1 = res1.payment_hash;

    // DON'T WAIT FOR A MOMENT, so the second payment will meet WaitingTlcAck first
    // but payment session will handle this case

    // we can make the same payment again, since payment_hash will be generated randomly
    let res2 = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    // the second send_payment will be blocked by WaitingTlcAck, since we didn't wait for a moment
    let payment_hash2 = res2.payment_hash;

    node_a.wait_until_success(payment_hash1).await;
    node_a.wait_until_success(payment_hash2).await;
}

#[tokio::test]
async fn test_network_send_previous_tlc_error() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, mut node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let secp = Secp256k1::new();
    let keys: Vec<Privkey> = std::iter::repeat_with(gen_rand_fiber_private_key)
        .take(1)
        .collect();
    let hops_infos = vec![
        PaymentHopData {
            amount: 2,
            expiry: 3,
            next_hop: Some(keys[0].pubkey()),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 8,
            expiry: 9,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let generated_payment_hash = gen_rand_sha256_hash();

    let packet = PeeledPaymentOnionPacket::create(
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
                        attempt_id: None,
                        expiry: DEFAULT_TLC_EXPIRY_DELTA + now_timestamp_as_millis_u64(),
                        hash_algorithm: HashAlgorithm::Sha256,
                        // invalid onion packet
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
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
    let res = node_a
        .send_payment_keysend(&node_b, 10000, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;

    node_a.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_network_send_previous_tlc_error_with_limit_amount_error() {
    init_tracing();

    let node_a_funding_amount = MIN_RESERVED_CKB + 400000000;
    let node_b_funding_amount = MIN_RESERVED_CKB;

    let (node_a, mut node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let secp = Secp256k1::new();
    let keys: Vec<Privkey> = std::iter::repeat_with(gen_rand_fiber_private_key)
        .take(1)
        .collect();
    let hops_infos = vec![
        PaymentHopData {
            amount: 300000000,
            expiry: 3,
            next_hop: Some(keys[0].pubkey()),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 300300000,
            expiry: 9,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let generated_payment_hash = gen_rand_sha256_hash();

    let packet = PeeledPaymentOnionPacket::create(
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
                        amount: 300300000,
                        payment_hash: generated_payment_hash,
                        attempt_id: None,
                        expiry: DEFAULT_TLC_EXPIRY_DELTA + now_timestamp_as_millis_u64(),
                        hash_algorithm: HashAlgorithm::Sha256,
                        // invalid onion packet
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
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
    let res = node_a
        .send_payment_keysend(&node_b, 300000000, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;

    node_a.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_network_send_payment_keysend_with_payment_hash() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_b_pubkey = node_b.pubkey;
    let payment_hash = gen_rand_sha256_hash();

    // This payment request is without an invoice, the receiver will return an error `IncorrectOrUnknownPaymentDetails`
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    assert!(res
        .err()
        .unwrap()
        .contains("keysend payment should not have payment_hash"));
}

#[tokio::test]
async fn test_network_send_payment_final_incorrect_hash() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_a_local_balance = node_a.get_local_balance_from_channel(channel_id);
    let node_b_local_balance = node_b.get_local_balance_from_channel(channel_id);

    let node_b_pubkey = node_b.pubkey;
    let payment_hash = gen_rand_sha256_hash();

    // This payment request is without an invoice, the receiver will return an error `IncorrectOrUnknownPaymentDetails`
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    assert_eq!(
        node_a.get_payment_status(payment_hash).await,
        PaymentStatus::Inflight
    );

    node_a.wait_until_failed(payment_hash).await;

    let res = node_a.get_payment_result(payment_hash).await;
    assert_eq!(res.status, PaymentStatus::Failed);
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

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, _node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_b_pubkey = gen_rand_fiber_public_key();
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(gen_rand_sha256_hash()),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());
    assert!(res.err().unwrap().contains("no path found"));
}

#[tokio::test]
async fn test_network_send_payment_amount_is_too_large() {
    init_tracing();

    let node_a_funding_amount = 100000000000 + MIN_RESERVED_CKB;
    let node_b_funding_amount = MIN_RESERVED_CKB + 2;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_b_pubkey = node_b.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(100000000000 + 5),
            payment_hash: Some(gen_rand_sha256_hash()),
            ..Default::default()
        })
        .await;

    assert!(res.is_err());
    // because the amount is too large, we will consider balance for direct channel
    // so fail to build a path
    assert!(res.err().unwrap().contains("no path found"));
}

// FIXME: this is the case send_payment with direct channels, we should handle this case
#[tokio::test]
async fn test_network_send_payment_with_dry_run() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 118000000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let node_b_pubkey = node_b.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(100000),
            payment_hash: Some(gen_rand_sha256_hash()),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentStatus::Created);
    // since there are only sender and receiver in the router, fee will be 0
    assert_eq!(res.fee, 0);
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(gen_rand_fiber_public_key()),
            amount: Some(1000 + 5),
            payment_hash: Some(gen_rand_sha256_hash()),
            dry_run: true,
            ..Default::default()
        })
        .await;
    // since the target is not valid, the payment check will fail
    assert!(res.is_err());
}

#[tokio::test]
async fn test_send_payment_with_3_nodes_f() {
    init_tracing();

    let (node_a, mut node_b, node_c, channel_1, channel_2) =
        create_3_nodes_with_established_channel(
            (100000000000, 100000000000),
            (100000000000, 100000000000),
        )
        .await;
    let node_a_local = node_a.get_local_balance_from_channel(channel_1);
    let node_b_local_left = node_b.get_local_balance_from_channel(channel_1);
    let node_b_local_right = node_b.get_local_balance_from_channel(channel_2);
    let node_c_local = node_c.get_local_balance_from_channel(channel_2);

    let sent_amount = 1000000 + 5;
    let res = node_a
        .send_payment_keysend(&node_c, sent_amount, false)
        .await
        .expect("send ok");
    assert_eq!(res.status, PaymentStatus::Created);
    assert!(res.fee > 0);

    node_b
        .expect_debug_event(&format!(
            "store payment_preimage for: {:?}",
            res.payment_hash
        ))
        .await;
    assert!(node_b.get_payment_preimage(&res.payment_hash).is_some());

    node_a.wait_until_success(res.payment_hash).await;

    assert!(node_a.get_payment_preimage(&res.payment_hash).is_none());
    assert!(node_b.get_payment_preimage(&res.payment_hash).is_none());
    assert!(node_c.get_payment_preimage(&res.payment_hash).is_none());

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

    let (nodes, channels) = create_n_nodes_network(
        vec![
            ((2, 1), (100000000000, 100000000000)),
            ((1, 0), (100000000000, 100000000000)),
        ]
        .as_slice(),
        3,
    )
    .await;

    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let [channel_1, channel_2] = channels.try_into().expect("2 channels");

    let node_c_local = node_c.get_local_balance_from_channel(channel_1);
    let node_b_local_right = node_b.get_local_balance_from_channel(channel_1);
    let node_b_local_left = node_b.get_local_balance_from_channel(channel_2);
    let node_a_local = node_a.get_local_balance_from_channel(channel_2);

    let sent_amount = 1000000 + 5;
    let res = node_c
        .send_payment_keysend(&node_a, sent_amount, false)
        .await
        .expect("send ok");
    assert_eq!(res.status, PaymentStatus::Created);
    assert!(res.fee > 0);
    // make sure the payment is sent
    node_c.wait_until_success(res.payment_hash).await;
    assert_eq!(
        node_c.get_payment_status(res.payment_hash).await,
        PaymentStatus::Success
    );

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

    let nodes_num = 15;
    let last = nodes_num - 1;
    let amounts = vec![(100000000000, 100000000000); nodes_num - 1];
    let (nodes, channels) = create_n_nodes_with_established_channel(&amounts, nodes_num).await;

    let sender_local = nodes[0].get_local_balance_from_channel(channels[0]);
    let receiver_local = nodes[last].get_local_balance_from_channel(channels[last - 1]);

    let sent_amount = 1000000 + 5;

    let source_node = &nodes[0];
    let res = source_node
        .send_payment_keysend(&nodes[last], sent_amount, false)
        .await
        .expect("send ok");
    assert_eq!(res.status, PaymentStatus::Created);
    assert!(res.fee > 0);

    // make sure the payment is sent
    nodes[0].wait_until_success(res.payment_hash).await;

    assert_eq!(
        nodes[0].get_payment_status(res.payment_hash).await,
        PaymentStatus::Success
    );
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

    let (node_a, _node_b, node_c, ..) = create_3_nodes_with_established_channel(
        (1000000000 * 100000000, 1000000000 * 100000000),
        (1000000000 * 100000000, 1000000000 * 100000000),
    )
    .await;

    let sent_amount = 0xfffffffffffffffffffffffffffffff;
    let res = node_a
        .send_payment_keysend(&node_c, sent_amount, false)
        .await;
    assert!(res.is_err());
    assert!(res
        .err()
        .unwrap()
        .contains("The payment amount (21267647932558653966460912964485513215) should be less than 1844674407370955161"));
}

#[tokio::test]
async fn test_send_payment_fail_with_3_nodes_invalid_hash() {
    init_tracing();

    let (node_a, node_b, node_c, channel_1, channel_2) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
    )
    .await;

    let node_a_local = node_a.get_local_balance_from_channel(channel_1);
    let node_b_local_left = node_b.get_local_balance_from_channel(channel_1);
    let node_b_local_right = node_b.get_local_balance_from_channel(channel_2);
    let node_c_local = node_c.get_local_balance_from_channel(channel_2);

    let node_c_pubkey = node_c.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c_pubkey),
            amount: Some(1000000 + 5),
            payment_hash: Some(gen_rand_sha256_hash()), // this payment hash is not from node_c
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentStatus::Created);
    assert!(res.fee > 0);
    // make sure the payment is sent
    node_a.wait_until_failed(res.payment_hash).await;

    let res = node_a.get_payment_result(res.payment_hash).await;
    assert_eq!(res.status, PaymentStatus::Failed);
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

    let (node_a, _node_b, node_c, ..) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
    )
    .await;

    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c.pubkey),
            amount: Some(1000000000),
            final_tlc_expiry_delta: Some(86400000 + 100), // should be in normal range
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.status, PaymentStatus::Created);

    let node_c_pubkey = node_c.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c_pubkey),
            amount: Some(1000000000),
            payment_hash: None,
            final_tlc_expiry_delta: Some(14 * 24 * 60 * 60 * 1000 + 1), // 14 days + 1 ms
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("invalid final_tlc_expiry_delta"));

    let node_c_pubkey = node_c.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c_pubkey),
            amount: Some(1000000000),
            payment_hash: None,
            final_tlc_expiry_delta: Some(14 * 24 * 60 * 60 * 1000 - 100), // 14 days - 100, will not find a path
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("no path found"));
}

#[tokio::test]
async fn test_send_payment_fail_with_3_nodes_dry_run_fee() {
    // Fix issue #360, dryrun option should get correct fee
    init_tracing();

    let (node_a, _node_b, node_c, ..) = create_3_nodes_with_established_channel(
        (100000000000, 100000000000),
        (100000000000, 100000000000),
    )
    .await;

    let res = node_a
        .send_payment_keysend(&node_c, 2000000000, true)
        .await
        .expect("send ok");
    assert_eq!(res.fee, 2000000);

    let res = node_a
        .send_payment_keysend(&node_c, 1000000000, true)
        .await
        .expect("send ok");
    // expect smaller fee since amount is smaller
    assert_eq!(res.fee, 1000000);

    let node_c_pubkey = node_c.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c_pubkey),
            amount: Some(1000000000),
            max_fee_amount: Some(res.fee), // exact the same fee limit
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    let res = res.unwrap();
    assert_eq!(res.fee, 1000000);

    let node_c_pubkey = node_c.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_c_pubkey),
            amount: Some(1000000000),
            max_fee_amount: Some(res.fee - 1), // set a smaller fee limit, path find will fail
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    assert!(res.is_err());
}

#[tokio::test]
async fn test_network_send_payment_dry_run_can_still_query() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let payment_hash = gen_rand_sha256_hash();
    let node_b_pubkey = node_b.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            dry_run: false,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    // sleep for a while to make sure the payment session is created
    node_a.wait_until_created(payment_hash).await;

    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Payment session already exists"));

    // now use a different payment hash
    let payment_hash = gen_rand_sha256_hash();
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_network_send_payment_dry_run_will_not_create_payment_session() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, _new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let payment_hash = gen_rand_sha256_hash();
    let node_b_pubkey = node_b.pubkey;
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    // make sure we can send the same payment after dry run query
    let res = node_a
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_b_pubkey),
            amount: Some(10000),
            payment_hash: Some(payment_hash),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_stash_broadcast_messages() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (_node_a, _node_b, _new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        true,
    )
    .await;
}

async fn do_test_channel_commitment_tx_after_add_tlc(algorithm: HashAlgorithm) {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let node_a_funding_amount = 100000000000;
    let node_b_funidng_amount = 11800000000;

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
    let digest = algorithm.hash(preimage);
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
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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
                Some(complete_commitment_tx(tx))
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
                Some(complete_commitment_tx(tx))
            }
            _ => None,
        })
        .await;
    assert!(matches!(
        node_a.submit_tx(node_a_commitment_tx.clone()).await,
        TxStatus::Committed(..)
    ));

    assert!(matches!(
        node_b.submit_tx(node_b_commitment_tx.clone()).await,
        TxStatus::Committed(..)
    ));
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
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, new_channel_id, _) = NetworkNode::new_2_nodes_with_established_channel(
        node_a_funding_amount,
        node_b_funding_amount,
        false,
    )
    .await;

    let preimage = [1; 32];
    let digest = correct_algorithm.hash(preimage);
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
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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
    let digest = correct_algorithm.hash(preimage);
    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: wrong_algorithm,
                        payment_hash: digest.into(),
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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
    init_tracing();

    // https://github.com/nervosnetwork/fiber/issues/447
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let tlc_number_in_flight_limit = 5;
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public: false,
            node_a_funding_amount,
            node_b_funding_amount,
            a_max_tlc_number_in_flight: Some(tlc_number_in_flight_limit as u64),
            b_max_tlc_number_in_flight: Some(tlc_number_in_flight_limit as u64),
            ..Default::default()
        },
    )
    .await;

    let mut all_sent = vec![];
    let mut batch_remove_count = 0;
    while batch_remove_count <= 3 {
        let preimage: [u8; 32] = gen_rand_sha256_hash().as_ref().try_into().unwrap();

        // create a new payment hash
        let hash_algorithm = HashAlgorithm::Sha256;
        let digest = hash_algorithm.hash(preimage);
        if let Ok(res) = call!(node_a.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: 1000,
                            hash_algorithm,
                            payment_hash: digest.into(),
                            attempt_id: None,
                            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                            onion_packet: None,
                            shared_secret: NO_SHARED_SECRET,
                            previous_tlc: None,
                        },
                        rpc_reply,
                    ),
                },
            ))
        })
        .expect("node_b alive")
        {
            all_sent.push((preimage, res.tlc_id));
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        if all_sent.len() >= tlc_number_in_flight_limit {
            while all_sent.len() > tlc_number_in_flight_limit - 2 {
                if let Some((preimage, tlc_id)) = all_sent.first().cloned() {
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
async fn do_test_channel_add_tlc_amount_invalid() {
    async fn run_add_tlc_amount(amount: u128, send_amount: u128) {
        let node_a_funding_amount = amount + MIN_RESERVED_CKB;
        let node_b_funding_amount = amount + MIN_RESERVED_CKB;

        let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;
        let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
            &mut node_a,
            &mut node_b,
            ChannelParameters {
                public: false,
                node_a_funding_amount,
                node_b_funding_amount,
                ..Default::default()
            },
        )
        .await;

        let preimage: [u8; 32] = gen_rand_sha256_hash().as_ref().try_into().unwrap();
        // create a new payment hash
        let hash_algorithm = HashAlgorithm::Sha256;
        let digest = hash_algorithm.hash(preimage);
        let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: new_channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            attempt_id: None,
                            amount: send_amount,
                            hash_algorithm,
                            payment_hash: digest.into(),
                            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                            onion_packet: None,
                            shared_secret: NO_SHARED_SECRET,
                            previous_tlc: None,
                        },
                        rpc_reply,
                    ),
                },
            ))
        })
        .expect("node_b alive");

        dbg!(&add_tlc_result);
        if send_amount > amount {
            assert!(add_tlc_result.is_err());
            assert_eq!(
                add_tlc_result.unwrap_err().to_string(),
                "TemporaryChannelFailure".to_string()
            );
        } else if send_amount == 0 {
            assert!(add_tlc_result.is_err());
            assert_eq!(
                add_tlc_result.unwrap_err().to_string(),
                "AmountBelowMinimum".to_string()
            );
        } else {
            assert!(add_tlc_result.is_ok());
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    run_add_tlc_amount(100, 0).await;
    run_add_tlc_amount(1000, 100).await;
    run_add_tlc_amount(1000, 1000).await;
    run_add_tlc_amount(1000, 1000 + 1).await;
}

#[tokio::test]
async fn test_network_add_tlc_amount_overflow_error() {
    init_tracing();

    let node_a_funding_amount = 1000 + MIN_RESERVED_CKB;
    let node_b_funding_amount = 1000 + MIN_RESERVED_CKB;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;
    let (channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public: false,
            node_a_funding_amount,
            node_b_funding_amount,
            ..Default::default()
        },
    )
    .await;

    async fn send_add_tlc(
        node: &NetworkNode,
        amount: u128,
        channel_id: Hash256,
    ) -> Result<AddTlcResponse, TlcErr> {
        let preimage: [u8; 32] = gen_rand_sha256_hash().as_ref().try_into().unwrap();
        // create a new payment hash
        let hash_algorithm = HashAlgorithm::Sha256;
        let digest = hash_algorithm.hash(preimage);
        call!(node.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount,
                            hash_algorithm,
                            payment_hash: digest.into(),
                            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                            onion_packet: None,
                            shared_secret: NO_SHARED_SECRET,
                            previous_tlc: None,
                            attempt_id: None,
                        },
                        rpc_reply,
                    ),
                },
            ))
        })
        .expect("node_b alive")
    }

    let res = send_add_tlc(&node_a, 10, channel_id).await;
    assert!(res.is_ok());
    let res = send_add_tlc(&node_a, u128::MAX, channel_id).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn test_network_add_two_tlcs_remove_one() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    // Wait for the channel announcement to be broadcasted

    let old_a_balance = node_a.get_local_balance_from_channel(channel_id);
    let old_b_balance = node_b.get_local_balance_from_channel(channel_id);

    let preimage_a = [1; 32];
    let algorithm = HashAlgorithm::Sha256;
    let digest = algorithm.hash(preimage_a);

    let add_tlc_result_a = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 1000,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_a alive")
    .expect("successfully added tlc");

    // if we don't wait for a while, the next add_tlc will fail with temporary failure
    let preimage_b = [2; 32];
    let algorithm = HashAlgorithm::Sha256;
    let digest = algorithm.hash(preimage_b);
    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 2000,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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
    loop {
        let node_a_state = node_a.get_channel_actor_state(channel_id);
        if !node_a_state.is_waiting_tlc_ack() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    let add_tlc_result_b = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 2000,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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

    loop {
        // remove tlc from node_b
        let res = call!(node_b.network_actor, |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
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
        .expect("node_b alive");

        let a_tlc_state = node_a.get_channel_actor_state(channel_id);
        let tlc_is_removed = a_tlc_state
            .tlc_state
            .get(&TLCId::Offered(add_tlc_result_a.tlc_id))
            .is_none();

        if res.is_ok() || tlc_is_removed {
            println!("remove tlc result: {:?}", res);
            break;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        let a_tlc_state = node_a.get_channel_actor_state(channel_id);
        if a_tlc_state
            .tlc_state
            .get(&TLCId::Offered(add_tlc_result_a.tlc_id))
            .is_none()
        {
            break;
        }
    }

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
    let tlc_id_b = add_tlc_result_b.unwrap().tlc_id;
    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: tlc_id_b,
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
    eprintln!("remove tlc result: {:?}", ());

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        let a_tlc_state = node_a.get_channel_actor_state(channel_id);
        if a_tlc_state
            .tlc_state
            .get(&TLCId::Offered(tlc_id_b))
            .is_none()
        {
            break;
        }
    }

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
    let node_b_funding_amount = 11800000000;

    let (node_a, _node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = HashAlgorithm::CkbHash.hash(preimage);
    let tlc_amount = 1000000000;

    // add tlc command with expiry soon
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: digest.into(),
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + 10,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
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

    // add tlc command with expiry soon
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: digest.into(),
        expiry: now_timestamp_as_millis_u64() + MIN_TLC_EXPIRY_DELTA - 1000,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
        previous_tlc: None,
        attempt_id: None,
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

    // add tlc command with expiry is OK
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: digest.into(),
        expiry: now_timestamp_as_millis_u64() + MIN_TLC_EXPIRY_DELTA + 200,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
        previous_tlc: None,
        attempt_id: None,
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
    let err = add_tlc_result.unwrap_err();
    assert_eq!(err.error_code, TlcErrorCode::ExpiryTooSoon,);

    // add tlc command with expiry in the future too long
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: digest.into(),
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + MAX_PAYMENT_TLC_EXPIRY_LIMIT + 20 * 1000,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
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
    let error_code = add_tlc_result.unwrap_err().error_code;
    assert_eq!(error_code, TlcErrorCode::ExpiryTooFar);
}

#[tokio::test]
async fn test_update_commitment_delay_epoch_will_trigger_signature_error() {
    init_tracing();

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT, true).await;

    let mut node_a_channel_state = node_a.get_channel_actor_state(new_channel_id);
    node_a_channel_state.commitment_delay_epoch = 0;
    node_a
        .update_channel_actor_state(node_a_channel_state, None)
        .await;

    let _res = node_a.send_payment_keysend(&node_b, 10000, false).await;

    let mut expect_error = false;
    for _ in 0..10 {
        let res = node_b.get_triggered_unexpected_events().await;
        error!("Unexpected event: {:?}", res);
        if res.iter().any(|x| x == "Musig2VerifyError") {
            expect_error = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }
    assert!(expect_error, "Expected Musig2VerifyError to be triggered");
}

#[tokio::test]
async fn test_remove_expired_tlc_in_background() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, _node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = HashAlgorithm::CkbHash.hash(preimage);
    let tlc_amount = 1000000000;

    // add tlc command with expiry soon
    let epoch_delay_milliseconds =
        (DEFAULT_COMMITMENT_DELAY_EPOCHS as f64 * MILLI_SECONDS_PER_EPOCH as f64 * 2.0 / 3.0)
            as u64;

    let a_valid_but_small_expiry = now_timestamp_as_millis_u64() + epoch_delay_milliseconds + 5000;
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: digest.into(),
        expiry: a_valid_but_small_expiry,
        onion_packet: None,
        shared_secret: NO_SHARED_SECRET,
        previous_tlc: None,
        attempt_id: None,
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
    assert!(add_tlc_result.is_ok());
    let tlc_id = add_tlc_result.unwrap().tlc_id;

    tokio::time::sleep(tokio::time::Duration::from_millis(
        epoch_delay_milliseconds + 6000,
    ))
    .await;

    // check if the expired tlc is removed
    let node_a_channel_state = node_a.get_channel_actor_state(new_channel_id);

    matches!(node_a_channel_state.state, ChannelState::ChannelReady);
    let tlc = node_a_channel_state
        .tlc_state
        .get(&TLCId::Offered(tlc_id))
        .unwrap();
    assert_eq!(
        tlc.status,
        TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed)
    );
}

#[tokio::test]
async fn do_test_add_tlc_duplicated() {
    init_tracing();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, _node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let preimage = [1; 32];
    let digest = HashAlgorithm::CkbHash.hash(preimage);
    let tlc_amount = 1000000000;

    for i in 1..=2 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: digest.into(),
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA + 1000,
            attempt_id: None,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
        debug!("add_tlc_result: {:?}", add_tlc_result);
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
    init_tracing();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11000000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let tlc_amount = 1000000000;

    for i in 1..=2 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // send from b to a
    for i in 1..=2 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            previous_tlc: None,
            shared_secret: NO_SHARED_SECRET,
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
    let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public: true,
            node_a_funding_amount,
            node_b_funding_amount,
            a_max_tlc_number_in_flight: Some(node_a_max_tlc_number),
            ..Default::default()
        },
    )
    .await;

    let tlc_amount = 1000000000;

    // A -> B will have tlc number limit 2
    for i in 1..=node_a_max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
    let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public: true,
            node_a_funding_amount,
            node_b_funding_amount,
            b_max_tlc_number_in_flight: Some(node_b_max_tlc_number),
            ..Default::default()
        },
    )
    .await;

    let tlc_amount = 1000000000;
    // B -> A will have tlc number limit 2
    for i in 1..=node_b_max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
    let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public: true,
            node_a_funding_amount,
            node_b_funding_amount,
            a_max_tlc_value_in_flight: Some(3000000000),
            ..Default::default()
        },
    )
    .await;

    let tlc_amount = 1000000000;

    // A -> B have tlc value limit 3_000_000_000
    for i in 1..=max_tlc_number + 1 {
        let add_tlc_command = AddTlcCommand {
            amount: tlc_amount,
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
            payment_hash: gen_rand_sha256_hash(),
            attempt_id: None,
            expiry: now_timestamp_as_millis_u64() + 100000000,
            onion_packet: None,
            shared_secret: NO_SHARED_SECRET,
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
    let node_b_funding_amount = 10000000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public: true,
            node_a_funding_amount,
            node_b_funding_amount,
            a_tlc_min_value: Some(100),
            ..Default::default()
        },
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // A -> B will be no limit
    let tlc_amount = 200;
    let add_tlc_command = AddTlcCommand {
        amount: tlc_amount,
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_hash: gen_rand_sha256_hash(),
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET,
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
        payment_hash: gen_rand_sha256_hash(),
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET,
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
        payment_hash: gen_rand_sha256_hash(),
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET,
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
    init_tracing();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters::new(node_a_funding_amount, node_b_funding_amount),
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

    // update channel with new tlc_expiry_delta which is too large
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: Some(MAX_PAYMENT_TLC_EXPIRY_LIMIT + 1),
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
        .contains("TLC expiry delta is too large"));

    let epoch_delay_milliseconds =
        (DEFAULT_COMMITMENT_DELAY_EPOCHS as f64 * MILLI_SECONDS_PER_EPOCH as f64 * 2.0 / 3.0)
            as u64;
    // update channel with new tlc_expiry_delta which is still too small
    // for less than 2/3 of the commitment delay
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: Some(epoch_delay_milliseconds - 10),
                        tlc_minimum_value: None,
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .unwrap();
    debug!("update_result: {:?}", update_result);
    assert!(update_result.is_err());

    let epoch_delay_milliseconds =
        (DEFAULT_COMMITMENT_DELAY_EPOCHS as f64 * MILLI_SECONDS_PER_EPOCH as f64 * 2.0 / 3.0)
            as u64;

    // update tlc_expiry_delta with 2/3 of the commitment delay
    // this should be successful
    let update_result = call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: Some(true),
                        tlc_expiry_delta: Some(epoch_delay_milliseconds),
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
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let [_channel_a_b, channel_b_c] = channels.try_into().expect("2 channels");

    let res = node_a
        .send_payment_keysend(&node_c, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_a.wait_until_success(payment_hash).await;

    let res = node_b
        .send_payment_keysend(&node_c, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_b.wait_until_success(payment_hash).await;

    let res = node_c
        .send_payment_keysend(&node_a, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_c.wait_until_success(payment_hash).await;

    let res = node_b
        .send_payment_keysend(&node_a, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_b.wait_until_success(payment_hash).await;

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

    let res = node_a
        .send_payment_keysend(&node_c, 10_000_000, false)
        .await;
    assert!(res.is_err(), "Send payment should fail: {:?}", res);

    let res = node_b
        .send_payment_keysend(&node_c, 10_000_000, false)
        .await;
    assert!(res.is_err(), "Send payment should fail: {:?}", res);

    let res = node_c
        .send_payment_keysend(&node_b, 10_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_c.wait_until_success(payment_hash).await;

    let res = node_c
        .send_payment_keysend(&node_a, 80_000_000, false)
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_c.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_forward_payment_tlc_minimum_value() {
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    let [channel_a_b, channel_b_c] = channels.try_into().expect("2 channels");

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
    let res = node_a
        .send_payment_keysend(&node_c, tlc_amount, false)
        .await
        .expect("send ok");
    // this is the payment_hash generated by keysend
    assert_eq!(res.status, PaymentStatus::Created);
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
        payment_hash: gen_rand_sha256_hash(),
        attempt_id: None,
        expiry: now_timestamp_as_millis_u64() + 100000000,
        onion_packet: None,
        previous_tlc: None,
        shared_secret: NO_SHARED_SECRET,
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

    // sending payment from A to B is OK because this has nothing to do with the channel_a_b.
    let _res = node_a
        .send_payment_keysend(&node_b, tlc_amount, false)
        .await
        .expect("send ok");
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // sending payment from B to C is not OK because the forwarding value is too small
    let res = node_b
        .send_payment_keysend(&node_c, tlc_amount, false)
        .await;
    assert!(res.is_err());

    // sending payment from A to C should fail because the forwarding value is too small
    let res = node_a
        .send_payment_keysend(&node_c, tlc_amount - 1, false)
        .await;
    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Failed to build route, PathFind error: no path found"));
}

#[tokio::test]
async fn test_send_payment_with_outdated_fee_rate() {
    init_tracing();
    let (nodes, _) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    let node_b_pubkey = node_b.pubkey;
    let node_c_pubkey = node_c.pubkey;
    let hash_set: HashSet<_> = [node_b_pubkey, node_c_pubkey].into_iter().collect();

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
    let res = node_a
        .send_payment_keysend(&node_c, 10000000000, false)
        .await
        .expect("send ok");
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
    let node_b_funding_amount = 11800000000;

    let (mut node_a, mut node_b, new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            false,
        )
        .await;

    let preimage = [1; 32];
    let digest = algorithm.hash(preimage);
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
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fee_rate = FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE);
    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: None,
                        fee_rate: Some(fee_rate),
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

    assert!(matches!(
        node_a
            .trace_tx(node_a_shutdown_tx_hash.clone().into())
            .await,
        TxStatus::Committed(..)
    ));
    assert!(matches!(
        node_b
            .trace_tx(node_b_shutdown_tx_hash.clone().into())
            .await,
        TxStatus::Committed(..)
    ));

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
                funding_amount: 11800000000,
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

    let x_only_aggregated_pubkey = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteTxComplete(
                _,
                _,
                _,
                _,
                _,
                local_funding_pubkey,
                remote_funding_pubkey,
                _,
            ) => {
                let key_agg_ctx =
                    KeyAggContext::new(vec![remote_funding_pubkey, local_funding_pubkey])
                        .expect("Valid pubkeys");
                Some(key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly())
            }
            _ => None,
        })
        .await;

    let commitment_tx = node_b
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::RemoteCommitmentSigned(peer_id, channel_id, tx, _) => {
                println!(
                    "Commitment tx {:?} from {:?} for channel {:?} received",
                    &tx, peer_id, channel_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(complete_commitment_tx(tx))
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
                assert_eq!(revocation_data.commitment_number, 1u64);
                Some(revocation_data.clone())
            }
            _ => None,
        })
        .await;

    assert!(matches!(
        node_a.submit_tx(commitment_tx.clone()).await,
        TxStatus::Committed(..)
    ));

    println!("commitment_tx: {:?}", commitment_tx);

    let tx = Transaction::default()
        .as_advanced_builder()
        .cell_deps(
            get_cell_deps(vec![Contract::CommitmentLock], &None)
                .await
                .expect("get cell deps"),
        )
        .input(
            CellInput::new_builder()
                .previous_output(commitment_tx.output_pts().first().unwrap().clone())
                .build(),
        )
        .output(revocation_data.output)
        .output_data(revocation_data.output_data)
        .build();

    let witness = [
        XUDT_COMPATIBLE_WITNESS.to_vec(),
        vec![0x00],
        revocation_data.commitment_number.to_be_bytes().to_vec(),
        x_only_aggregated_pubkey.to_vec(),
        revocation_data.aggregated_signature.serialize().to_vec(),
    ]
    .concat();

    let revocation_tx = tx.as_advanced_builder().witness(witness.pack()).build();

    assert!(matches!(
        node_a.submit_tx(revocation_tx.clone()).await,
        TxStatus::Committed(..)
    ));
}

#[tokio::test]
async fn test_channel_with_simple_update_operation() {
    init_tracing();

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
                funding_amount: 11800000000,
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
                Some(complete_commitment_tx(tx))
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
                Some(complete_commitment_tx(tx))
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
    assert!(matches!(
        node_a.submit_tx(node_a_commitment_tx.clone()).await,
        TxStatus::Committed(..)
    ));
    assert!(matches!(
        node_b.submit_tx(node_b_commitment_tx.clone()).await,
        TxStatus::Committed(..)
    ));
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
                funding_amount: 11800000000,
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
            NetworkActorCommand::DisconnectPeer(
                node_b.peer_id.clone(),
                PeerDisconnectReason::Requested,
            ),
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
        NetworkNode::new_2_nodes_with_established_channel(111800000000, 11800000000, true).await;

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
                        close_script: None,
                        fee_rate: Some(FeeRate::from_u64(1000)),
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
async fn test_normal_shutdown_with_remove_tlc() {
    init_tracing();

    let (node_a, node_b, channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT, true)
            .await;

    let preimage = [1; 32];
    let algorithm = HashAlgorithm::CkbHash;
    let digest = algorithm.hash(preimage);
    let tlc_amount = 1000000000;

    let node_a_state = node_a.get_channel_actor_state(channel_id);
    let node_b_state = node_b.get_channel_actor_state(channel_id);
    let old_node_a_balance = node_a_state.to_local_amount;
    let old_node_b_balance = node_b_state.to_local_amount;

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: tlc_amount,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
                        previous_tlc: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_a alive")
    .expect("successfully added tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // node_a send Shutdown
    let message = |rpc_reply| -> NetworkActorMessage {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: None,
                        fee_rate: Some(FeeRate::from_u64(1000)),
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    };
    let res = call!(node_a.network_actor, message);
    debug!("shutdown res: {:?}", res);
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // node_b send remove tlc
    call!(node_b.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
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

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_a_state = node_a.get_channel_actor_state(channel_id);
    let node_b_state = node_b.get_channel_actor_state(channel_id);

    assert_eq!(
        node_a_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
    assert_eq!(
        node_b_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
    let node_a_balance = node_a_state.to_local_amount;
    let node_b_balance = node_b_state.to_local_amount;
    assert_eq!(node_a_balance, old_node_a_balance - tlc_amount);
    assert_eq!(node_b_balance, old_node_b_balance + tlc_amount);
}

#[tokio::test]
async fn test_commitment_tx_capacity() {
    let (amount_a, amount_b) = (111800000000, 11800000000);
    let (node_a, _node_b, channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(amount_a, amount_b, true).await;

    let state = node_a.store.get_channel_actor_state(&channel_id).unwrap();
    let commitment_tx = state.get_latest_commitment_transaction().await.unwrap();
    let output_capacity: u64 = commitment_tx.output(0).unwrap().capacity().unpack();

    // default fee rate is 1000 shannons per kb
    assert_eq!(
        amount_a + amount_b - commitment_tx.data().serialized_size_in_block() as u128,
        output_capacity as u128
    );
}

#[tokio::test]
async fn test_connect_to_peers_with_mutual_channel_on_restart_1() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (mut node_a, mut node_b, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    node_a.restart().await;

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    ).await;

    node_a
        .expect_debug_event("Reestablished channel in ChannelReady")
        .await;
    node_b
        .expect_debug_event("Reestablished channel in ChannelReady")
        .await;
    assert!(node_a.get_triggered_unexpected_events().await.is_empty());
    assert!(node_b.get_triggered_unexpected_events().await.is_empty());
}

#[tokio::test]
async fn test_connect_to_peers_with_mutual_channel_on_restart_2() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (mut node_a, mut node_b, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;
    debug!("debug tentacle node_a and node_b connected");

    debug!("debug tentacle before stop node_a");
    node_a.stop().await;
    debug!("debug tentacle after stop node_a");

    node_b.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerDisConnected(id, _addr) if id == &node_a.peer_id),
    )
    .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    debug!("debug tentacle before restart node_a");
    node_a.start().await;
    debug!("debug tentacle after restart node_a");

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    )
    .await;
}

#[tokio::test]
async fn test_send_payment_with_node_restart_then_resend_add_tlc() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (mut node_a, mut node_b, _new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    let tlc_amount = 99;
    let res = node_a
        .send_payment_keysend(&node_b, tlc_amount, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;

    node_b.stop().await;

    let payment_status = node_a.get_payment_status(payment_hash).await;
    assert_eq!(payment_status, PaymentStatus::Inflight);

    node_b.start().await;

    node_a.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &node_b.peer_id),
    )
    .await;

    node_a.expect_debug_event("resend add tlc").await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    let payment_status = node_a.get_payment_status(payment_hash).await;
    assert_eq!(payment_status, PaymentStatus::Success);
    assert!(node_a.get_triggered_unexpected_events().await.is_empty());
    assert!(node_b.get_triggered_unexpected_events().await.is_empty());
}

#[tokio::test]
async fn test_node_reestablish_resend_remove_tlc() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (mut node_a, mut node_b, new_channel_id, _) =
        NetworkNode::new_2_nodes_with_established_channel(
            node_a_funding_amount,
            node_b_funding_amount,
            true,
        )
        .await;

    let node_a_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let preimage = [2; 32];
    // create a new payment hash
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage);

    let add_tlc_result = call!(node_a.network_actor, |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 1000,
                        hash_algorithm: HashAlgorithm::CkbHash,
                        payment_hash: payment_hash.into(),
                        attempt_id: None,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                        shared_secret: NO_SHARED_SECRET,
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
    node_b.expect_debug_event("resend remove tlc").await;
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
    assert!(node_a.get_triggered_unexpected_events().await.is_empty());
    assert!(node_b.get_triggered_unexpected_events().await.is_empty());
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
                shutdown_script: Some(Script::new_builder().args([0u8; 60].pack()).build()),
                funding_amount: (101 + 1) * 100000000 - 1,
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

    assert!(open_channel_result.err().unwrap().contains(
        "The funding amount (10199999999) should be greater than or equal to 10200000000"
    ));
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
                config.auto_accept_channel_ckb_funding_amount = Some(11800000000);
                config.open_channel_auto_accept_min_ckb_funding_amount = Some(111800000000);
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
                shutdown_script: Some(Script::new_builder().args([0u8; 40].pack()).build()),
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
    let node_b_funding_amount = 9900000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Some(Script::new_builder().args([0u8; 58].pack()).build()),
                        fee_rate: Some(FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE)),
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

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: new_channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Some(Script::new_builder().args([0u8; 21].pack()).build()),
                        fee_rate: Some(FeeRate::from_u64(u64::MAX)),
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    };

    let shutdown_channel_result = call!(node_a.network_actor, message).expect("node_b alive");
    assert!(shutdown_channel_result
        .err()
        .unwrap()
        .contains("Local balance is not enough to pay the fee"));
}

#[tokio::test]
async fn test_shutdown_channel_with_invalid_feerate_peer_message() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, mut node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

    let command = ShutdownCommand {
        close_script: Some(Script::new_builder().args([0u8; 21].pack()).build()),
        fee_rate: Some(FeeRate::from_u64(u64::MAX)),
        force: false,
    };

    node_a
        .handle_shutdown_command_without_check(new_channel_id, command)
        .await;

    node_b
        .expect_debug_event("InvalidParameter(\"Shutdown fee is invalid\")")
        .await;
    let state = node_b.get_channel_actor_state(new_channel_id);
    matches!(state.state, ChannelState::ChannelReady);
}

#[tokio::test]
async fn test_shutdown_channel_with_different_size_shutdown_script() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

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
                        close_script: None,
                        fee_rate: Some(FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE)),
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

    node_a.expect_debug_event("ChannelClosed").await;
    node_b.expect_debug_event("ChannelClosed").await;

    assert_eq!(node_a_shutdown_tx_hash, node_b_shutdown_tx_hash);

    assert!(matches!(
        node_a
            .trace_tx(node_a_shutdown_tx_hash.clone().into())
            .await,
        TxStatus::Committed(..)
    ));
    assert!(matches!(
        node_b
            .trace_tx(node_b_shutdown_tx_hash.clone().into())
            .await,
        TxStatus::Committed(..)
    ));
}

#[tokio::test]
async fn test_shutdown_channel_network_graph_will_not_sync_private_channel() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, _channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, false)
            .await;

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
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

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
                channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Some(Script::new_builder().args([0u8; 19].pack()).build()),
                        fee_rate: Some(FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE)),
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
    assert!(network_channels.is_empty());

    let network_channels = node_b.get_network_channels().await;
    assert!(network_channels.is_empty());
}

#[tokio::test]
async fn test_shutdown_channel_and_shutdown_transaction_hash() {
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let (node_a, node_b, channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Some(Script::new_builder().args([0u8; 19].pack()).build()),
                        fee_rate: Some(FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE)),
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

    let channel_state_a = node_a.get_channel_actor_state(channel_id);
    assert!(channel_state_a.shutdown_transaction_hash.is_some());

    let channel_state_b = node_b.get_channel_actor_state(channel_id);
    assert_eq!(
        channel_state_a.shutdown_transaction_hash,
        channel_state_b.shutdown_transaction_hash
    );
}

#[tokio::test]
async fn test_send_payment_with_channel_balance_error() {
    init_tracing();

    let nodes_num = 4;
    let amounts = vec![(100000000000, 100000000000); nodes_num - 1];
    let (nodes, channels) = create_n_nodes_with_established_channel(&amounts, nodes_num).await;
    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    let res = source_node
        .send_payment_keysend(&node_3, 3000, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;
    // sleep for a while
    source_node.wait_until_success(payment_hash).await;

    node_2.update_channel_local_balance(channels[2], 100).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // expect send payment failed
    let res = source_node
        .send_payment_keysend(&node_3, 3000, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    let res = source_node.get_payment_result(payment_hash).await;

    assert_eq!(res.status, PaymentStatus::Failed);
    assert!(res.failed_error.unwrap().contains("Failed to build route"));

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.attempts_count(), 1);
    assert_eq!(payment_session.retry_times(), 2);
}

#[tokio::test]
async fn test_send_payment_with_disable_channel() {
    init_tracing();

    let nodes_num = 4;
    let amounts = vec![(100000000000, 100000000000); nodes_num - 1];
    let (nodes, channels) = create_n_nodes_with_established_channel(&amounts, nodes_num).await;
    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    // begin to set channel disable, but do not notify the network
    node_2.disable_channel_stealthy(channels[1]).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // expect send payment failed from node_3 to node_0
    let res = node_3.send_payment_keysend(&node_0, 3000, false).await;
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_3.wait_until_failed(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session_state = node_3.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session_state.retry_times(), 2);

    // expect send payment successfully from node_0 to node_3
    let res = node_0.send_payment_keysend(&node_3, 3000, false).await;
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    node_0.wait_until_success(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retry_times(), 1);
}

#[tokio::test]
async fn test_send_payment_with_multiple_edges_in_middle_hops() {
    init_tracing();

    // we have two chaneels between node_1 and node_2, they are all with the same meta information except the later one has more capacity
    // path finding will try the channel with larger capacity first, so we assert the payment retry times is 1
    // the send payment should be succeed
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (MIN_RESERVED_CKB + 900, 10800000000)),
            ((1, 2), (MIN_RESERVED_CKB + 1000, 10800000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to succeed
    let res = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;

    source_node.wait_until_success(payment_hash).await;
    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retry_times(), 1);
    assert_eq!(payment_session.attempts_count(), 1);
}

#[tokio::test]
async fn test_send_payment_with_all_failed_middle_hops() {
    init_tracing();

    // we have two chaneels between node_1 and node_2
    // they liquid capacity is enough for send payment, but actual balance are both not enough
    // path finding will all try them but all failed, so we assert the payment retry times is 3
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (MIN_RESERVED_CKB + 900, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (MIN_RESERVED_CKB + 910, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to failed
    let res = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;
    source_node.wait_until_failed(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    assert!(node_0.get_triggered_unexpected_events().await.is_empty());
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.attempts_count(), 1);
    assert_eq!(payment_session.retry_times(), 3);
}

#[tokio::test]
async fn test_send_payment_with_multiple_edges_can_succeed_in_retry() {
    init_tracing();

    // we have two chaneels between node_1 and node_2, they are all with the same meta information except the later one has more capacity
    // but even channel_2's capacity is larger, the to_local_amount is not enough for the payment
    // path finding will retry the first channel and the send payment should be succeed
    // the payment retry times should be 2
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (MIN_RESERVED_CKB + 1000, 10800000000)),
            ((1, 2), (MIN_RESERVED_CKB + 900, 11800000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to succeed
    let res = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.attempts_count(), 1);
    assert_eq!(payment_session.retry_times(), 2);
}

#[tokio::test]
async fn test_send_payment_with_final_hop_multiple_edges_in_middle_hops() {
    init_tracing();

    // we have two chaneels between node_2 and node_3, they are all with the same meta information except the later one has more capacity
    // path finding will try the channel with larger capacity first, so we assert the payment retry times is 1
    // the send payment should be succeed
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 900, 10800000000)),
            ((2, 3), (MIN_RESERVED_CKB + 1000, 10800000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to succeed
    let res = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("send ok");
    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // because there is only one path for the payment, the payment will fail in the second try
    // this assertion make sure we didn't do meaningless retry
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.attempts_count(), 1);
    assert_eq!(payment_session.retry_times(), 1);
}

#[tokio::test]
async fn test_send_payment_with_final_all_failed_middle_hops() {
    init_tracing();

    // we have two chaneels between node_2 and node_3
    // they liquid capacity is enough for send payment, but actual balance are both not enough
    // path finding will all try them but all failed, so we assert the payment retry times is 3
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 900, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 910, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to succeed
    let payment_hash = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("payment successful")
        .payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Failed, Some(3))
        .await;
}

#[tokio::test]
async fn test_send_payment_with_final_multiple_edges_can_succeed_in_retry() {
    init_tracing();

    // we have two chaneels between node_2 and node_3, they are all with the same meta information except the later one has more capacity
    // but even channel_2's capacity is larger, the to_local_amount is not enough for the payment
    // path finding will retry the first channel and the send payment should be succeed
    // the payment retry times should be 2
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 1000, 10800000000)),
            ((2, 3), (MIN_RESERVED_CKB + 900, 11800000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to succeed
    let payment_hash = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("payment successful")
        .payment_hash;

    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(2))
        .await;
}

#[tokio::test]
async fn test_send_payment_with_first_hop_failed_with_fee() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            // even 1000 > 999, but it's not enough for fee, and this is the direct channel
            // so we can check the actual balance of channel
            // the payment will fail
            ((0, 1), (MIN_RESERVED_CKB + 1000, 10800000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    // expect send payment to fail
    let res = node_0.send_payment_keysend(&node_3, 999, false).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Failed to build route"));
}

#[tokio::test]
async fn test_send_payment_succeed_with_multiple_edges_in_first_hop() {
    init_tracing();

    // we have two chaneels between node_0 and node_1, they are all with the same meta information except the later one has more capacity
    // path finding will try the channel with larger capacity first, so we assert the payment retry times is 1
    // the send payment should be succeed
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 900, 10800000000)),
            ((0, 1), (MIN_RESERVED_CKB + 1001, 10800000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &node_0;

    // expect send payment to succeed
    let payment_hash = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("payment successful")
        .payment_hash;

    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_with_first_hop_all_failed() {
    init_tracing();

    // we have two chaneels between node_0 and node_1
    // they liquid capacity is enough for send payment, but actual balance are both not enough
    // path finding will fail in the first time of send payment
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 900, MIN_RESERVED_CKB + 1000)),
            ((0, 1), (MIN_RESERVED_CKB + 910, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");

    // expect send payment to failed
    let res = node_0.send_payment_keysend(&node_3, 999, false).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Failed to build route"));
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_direct_channel_info_first_hop() {
    init_tracing();

    // we have two chaneels between node_0 and node_1
    // the path finding will first try the channel with larger capacity,
    // but we manually set the to_local_amount to smaller value for testing
    // path finding will get the direct channel info with actual balance of channel,
    // so it will try the channel with smaller capacity and the payment will succeed
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((0, 1), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;

    // manually update the channel's to_local_amount
    source_node
        .update_channel_local_balance(channels[0], 100)
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // expect send payment to succeed
    let payment_hash = source_node
        .send_payment_keysend(&node_3, 999, false)
        .await
        .expect("payment successful")
        .payment_hash;

    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_retry_in_middle_hops() {
    init_tracing();

    // we have two chaneels between node_2 and node_3
    // the path finding will first try the channel with larger capacity,
    // but we manually set the to_local_amount to smaller value for testing
    // path finding will get a temporary failure in the first try and retry the second channel
    // so it will try the channel with smaller capacity and the payment will succeed
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let node_0_amount = source_node.get_local_balance_from_channel(channels[0]);

    // manually update the channel's to_local_amount
    node_2.update_channel_local_balance(channels[2], 100).await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let amount = 999;
    // expect send payment to succeed
    let res = source_node
        .send_payment_keysend(&node_3, amount, false)
        .await
        .expect("payment successful");

    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;

    let fee = res.fee;
    eprintln!("fee: {:?}", fee);
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(2))
        .await;

    let new_node0_amount = source_node.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_amount - amount - fee, new_node0_amount);
}

#[tokio::test]
async fn test_send_payment_will_fail_with_last_hop_info_in_add_tlc_peer() {
    init_tracing();

    // we have two chaneels between node_2 and node_3
    // the path finding will first try the channel with larger capacity,
    // but we manually set the to_remote_amount for node_3 to a larger amount,
    // this will make node3 trigger error in add_tlc_peer and got an Musig2VerifyError(BadSignature)
    // the send_payment will failed with retry times of 1
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, mut node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey;

    // manually update the channel's to_remote_amount
    node_3
        .update_channel_remote_balance(channels[2], 100000000)
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(999),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    // expect send payment to failed
    assert!(res.is_ok());

    node_3
        .expect_event(|event| match event {
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(error)) => {
                error.contains("Musig2VerifyError(BadSignature)")
            }
            _ => false,
        })
        .await;

    let payment_hash = res.unwrap().payment_hash;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Inflight, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_will_fail_with_invoice_not_generated_by_target() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey;

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
            target_pubkey: Some(target_pubkey),
            amount: Some(100),
            max_fee_rate: Some(1000),
            invoice: Some(invoice.clone()),
            ..Default::default()
        })
        .await;

    // expect send payment to succeed
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Failed, Some(1))
        .await;
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_valid_invoice() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey;
    let old_amount = node_3.get_local_balance_from_channel(channels[2]);

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    node_3.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(100),
            max_fee_rate: Some(1000),
            invoice: Some(ckb_invoice.to_string()),
            ..Default::default()
        })
        .await;

    // expect send payment to succeed
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let new_amount = node_3.get_local_balance_from_channel(channels[2]);
    assert_eq!(new_amount, old_amount + 100);
    assert_eq!(
        node_3.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Paid)
    );
    assert!(node_3
        .get_payment_preimage(ckb_invoice.payment_hash())
        .is_none());
}

#[tokio::test]
async fn test_send_payment_will_fail_with_no_invoice_preimage() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey;
    let old_amount = node_3.get_local_balance_from_channel(channels[2]);

    let preimage = gen_rand_sha256_hash();
    // with a short expiry time for test purpose
    let expiry_time_in_seconds = 5;
    let timer_started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("get time now")
        .as_secs();

    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .expiry_time(Duration::from_secs(expiry_time_in_seconds))
        .build()
        .expect("build invoice success");

    // insert invoice without preimage
    node_3.insert_invoice(ckb_invoice.clone(), None);

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(100),
            max_fee_rate: Some(1000),
            invoice: Some(ckb_invoice.to_string()),
            ..Default::default()
        })
        .await;

    // expect send payment to failed because we can not find preimage
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Failed, Some(1))
        .await;

    let time_elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("get time now")
        .as_secs()
        - timer_started;

    assert!(time_elapsed >= expiry_time_in_seconds);

    let new_amount = node_3.get_local_balance_from_channel(channels[2]);
    assert_eq!(new_amount, old_amount);

    // the invoice is updated to Received
    assert_eq!(
        node_3.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Received)
    );

    // send the same payment_hash again will also fail
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(100),
            invoice: Some(ckb_invoice.to_string()),
            ..Default::default()
        })
        .await;

    let error = res.unwrap_err();
    assert!(error.contains("invoice is expired"));
}

#[tokio::test]
async fn test_send_payment_will_fail_with_cancelled_invoice() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (100000000000, 100000000000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1005, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey;
    let old_amount = node_3.get_local_balance_from_channel(channels[2]);

    let preimage = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(100))
        .payment_preimage(preimage)
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
            target_pubkey: Some(target_pubkey),
            amount: Some(100),
            max_fee_rate: Some(1000),
            invoice: Some(ckb_invoice.to_string()),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_failed(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Failed, Some(1))
        .await;

    let new_amount = node_3.get_local_balance_from_channel(channels[2]);
    assert_eq!(new_amount, old_amount);
    assert_eq!(
        node_3.get_invoice_status(ckb_invoice.payment_hash()),
        Some(CkbInvoiceStatus::Cancelled)
    );
    assert!(node_3
        .get_payment_preimage(ckb_invoice.payment_hash())
        .is_some());
}

#[tokio::test]
async fn test_send_payment_will_succeed_with_large_tlc_expiry_limit() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/367

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 2000, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (100000000000, 100000000000)),
            ((2, 3), (100000000000, 100000000000)),
        ],
        4,
    )
    .await;
    let [mut node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("4 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_3.pubkey;

    let expected_minimal_tlc_expiry_limit =
        DEFAULT_TLC_EXPIRY_DELTA * 2 + DEFAULT_FINAL_TLC_EXPIRY_DELTA;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(999),
            tlc_expiry_limit: Some(expected_minimal_tlc_expiry_limit - 1),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.unwrap_err().contains("Failed to build route"));

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(999),
            tlc_expiry_limit: Some(expected_minimal_tlc_expiry_limit),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    // expect send payment to succeed
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;
}

#[tokio::test]
async fn test_abandon_failed_channel_without_accept() {
    let [mut node_a, node_b] = NetworkNode::new_n_interconnected_nodes().await;

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

    eprintln!("open_channel_result: {:?}", open_channel_result);

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let temp_channel_id = open_channel_result.channel_id;
    let node_a_channel_actor_state = node_a.get_channel_actor_state_unchecked(temp_channel_id);
    assert!(node_a_channel_actor_state.is_none());

    let res = node_a.send_abandon_channel(temp_channel_id).await;
    assert!(res.is_ok());
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    node_a.expect_debug_event("ChannelActorStopped").await;
}

#[tokio::test]
async fn test_open_channel_with_invalid_commitment_delay() {
    async fn test_with_commitment_delay_epoch(
        commitment_delay_epoch: Option<EpochNumberWithFraction>,
        expected_error: &str,
    ) {
        let [node_a, node_b] = NetworkNode::new_n_interconnected_nodes().await;

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: node_b.peer_id.clone(),
                    public: false,
                    shutdown_script: None,
                    funding_amount: 100000000000,
                    funding_udt_type_script: None,
                    commitment_fee_rate: None,
                    commitment_delay_epoch,
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

        eprintln!("open_channel_result: {:?}", open_channel_result);
        let error = open_channel_result.unwrap_err().to_string();
        assert!(error.contains(expected_error));
    }

    test_with_commitment_delay_epoch(
        Some(EpochNumberWithFraction::new(
            MIN_COMMITMENT_DELAY_EPOCHS - 1,
            0,
            1,
        )),
        "is less than the minimal value Epoch",
    )
    .await;
    test_with_commitment_delay_epoch(
        Some(EpochNumberWithFraction::new(
            MAX_COMMITMENT_DELAY_EPOCHS + 1,
            0,
            1,
        )),
        "is greater than the maximal value Epoch",
    )
    .await;
}

#[tokio::test]
async fn test_open_channel_tlc_expiry_is_smaller_than_commitment_delay() {
    let [node_a, node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: Some(EpochNumberWithFraction::new(10, 0, 1)),
                funding_fee_rate: None,
                tlc_expiry_delta: Some(
                    (10.0 * MILLI_SECONDS_PER_EPOCH as f64 * 2.0 / 3.0) as u64 - 1,
                ),
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message).expect("node_a alive");

    eprintln!("open_channel_result: {:?}", open_channel_result);
    let error = open_channel_result.unwrap_err().to_string();
    assert!(error
        .contains("TLC expiry delta 13332 is smaller than 2/3 commitment_delay_epoch delay 13333"));

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount: 100000000000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: Some(EpochNumberWithFraction::new(10, 0, 1)),
                funding_fee_rate: None,
                tlc_expiry_delta: Some((10.0 * MILLI_SECONDS_PER_EPOCH as f64 * 2.0 / 3.0) as u64),
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message).expect("node_a alive");

    assert!(open_channel_result.is_ok(), "open channel should succeed");
}

#[tokio::test]
async fn test_abandon_channel_with_peer_accept() {
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

    eprintln!("open_channel_result: {:?}", open_channel_result);

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let temp_channel_id = open_channel_result.channel_id;
    let node_a_channel_actor_state = node_a.get_channel_actor_state_unchecked(temp_channel_id);
    assert!(node_a_channel_actor_state.is_none());

    // stop ckb chain actor to make sure funding tx is not sent
    node_a.send_ckb_chain_message(crate::ckb::CkbChainMessage::Stop);

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

    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");

    let new_channel_id = accept_channel_result.new_channel_id;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // temp_channel_id will not work now, since it has been replaced by the real channel_id
    let res = node_a.send_abandon_channel(temp_channel_id).await;
    assert!(res.is_err());

    let channel_actor_state = node_a
        .get_channel_actor_state_unchecked(new_channel_id)
        .expect("channel actor state");
    eprintln!("channel_actor_state: {:?}", channel_actor_state.state);
    let res = node_a.send_abandon_channel(new_channel_id).await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // make sure the channel actor is stopped
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    node_a.expect_debug_event("ChannelActorStopped").await;

    // make sure the channel is removed from DB
    let channel = node_a.get_channel_actor_state_unchecked(new_channel_id);
    assert!(channel.is_none());

    // ----------------------------------------------------------------------------

    // Node_b can also abandon channel, node_a's CKB chain actor is stopped,
    // so node_b will not received `TxCollaborationCommand::TxUpdate` message
    // the channel_actor_state haven't been inserted into DB
    let channel_actor_state = node_b.get_channel_actor_state_unchecked(new_channel_id);
    assert!(channel_actor_state.is_none());

    let res = node_b.send_abandon_channel(new_channel_id).await;
    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // make sure the channel actor is stopped
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    node_b.expect_debug_event("ChannelActorStopped").await;
}

#[tokio::test]
async fn test_channel_with_malicious_peer_send_channel_msg() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let [node_0, node_1, node_2] = nodes.try_into().expect("expected nodes");

    let test_with_node = async |target_node: &NetworkNode| {
        // channels[0] is between node_0 and node_1,
        let wrong_channel_id = channels[0];

        let preimage_a = [1; 32];
        let algorithm = HashAlgorithm::Sha256;
        let digest = algorithm.hash(preimage_a);

        node_2
            .network_actor
            .send_message(NetworkActorMessage::Command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId {
                    peer_id: target_node.peer_id.clone(),
                    message: FiberMessage::add_tlc(AddTlc {
                        channel_id: wrong_channel_id,
                        amount: 1000,
                        tlc_id: 0,
                        hash_algorithm: algorithm,
                        payment_hash: digest.into(),
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: None,
                    }),
                }),
            ))
            .expect("send message");
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        let channel_state = target_node
            .get_channel_actor_state_unchecked(wrong_channel_id)
            .expect("channel actor state");
        assert_eq!(channel_state.tlc_state.all_tlcs().count(), 0);
    };

    test_with_node(&node_0).await;
    test_with_node(&node_1).await;
}

#[tokio::test]
async fn test_funding_timeout() {
    let funding_amount: u128 = 100000000000;
    let mut nodes = NetworkNode::new_n_interconnected_nodes_with_config(2, |i| {
        NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("node-{}", i)))
            .base_dir_prefix(&format!("test-fnn-node-{}-", i))
            .fiber_config_updater(|config| {
                // funding amount + 1
                config.open_channel_auto_accept_min_ckb_funding_amount = Some(100000000001);
                config.funding_timeout_seconds = 1;
            })
            .build()
    })
    .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: nodes[1].peer_id.clone(),
                public: false,
                shutdown_script: None,
                funding_amount,
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
    call!(nodes[0].network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    // Auto closed because of timeout
    nodes[0]
        .expect_event(|event| matches!(event, NetworkServiceEvent::ChannelFundingAborted(_)))
        .await;
}

#[tokio::test]
async fn test_channel_one_peer_check_active_fail() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    node_0
        .send_shutdown_command_to_channel(
            channels[0],
            ShutdownCommand {
                force: true,
                close_script: None,
                fee_rate: None,
            },
        )
        .await;

    let mut wait_time = 0;
    for _ in 0..50 {
        if matches!(
            node_0.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
        ) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        wait_time += 1;
    }
    if wait_time >= 50 {
        panic!("node_0 channel did not reach Closed state in time");
    }

    wait_time = 0;
    for _ in 0..50 {
        let node_1_state = node_1.get_channel_actor_state(channels[0]);
        if matches!(node_1_state.state, ChannelState::ChannelReady) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        wait_time += 1;
    }
    if wait_time >= 50 {
        panic!("node_1 channel did not reach ChannelReady state in time");
    }
}

/// Test for issue #938: Channel funding is aborted after restart when stuck in NegotiatingFunding
///
/// This test verifies that the fix works correctly:
/// 1. A channel is opened and accepted (entering NegotiatingFunding(INIT_SENT) state)
/// 2. CKB RPC becomes unavailable (simulated by stopping the chain actor)
/// 3. The node is restarted
/// 4. The channel should be aborted (goes to Closed(FUNDING_ABORTED) state) instead of staying stuck
#[tokio::test]
async fn test_channel_aborts_funding_after_restart_when_stuck_in_negotiating_funding() {
    init_tracing();

    // Create two interconnected nodes
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    // Step 1: Open a channel from node_a to node_b
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: true,
                shutdown_script: None,
                funding_amount: 200 * 100000000, // 200 CKB
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

    let temp_channel_id = open_channel_result.channel_id;

    // Wait for node_b to receive the channel pending event
    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(*channel_id, temp_channel_id);
                true
            }
            _ => false,
        })
        .await;

    // Step 2: Stop the CKB chain actor on node_a to simulate CKB RPC being unavailable
    // This simulates the scenario where CKB is closed before funding completes
    node_a.send_ckb_chain_message(crate::ckb::CkbChainMessage::Stop);

    // Step 3: Accept the channel on node_b
    // This will trigger ChannelAccepted event on node_a, which attempts to fund the channel
    // Since CKB is stopped, the funding will fail with a temporary error
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id,
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

    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");

    let new_channel_id = accept_channel_result.new_channel_id;

    // Wait a bit for the funding attempt to fail
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify that node_a's channel is in NegotiatingFunding(INIT_SENT) state
    // This state has both OUR_INIT_SENT and THEIR_INIT_SENT flags set
    let channel_state_before_restart = node_a
        .get_channel_actor_state_unchecked(new_channel_id)
        .expect("channel should exist after accept");

    match channel_state_before_restart.state {
        ChannelState::NegotiatingFunding(flags) => {
            // Verify both flags are set (INIT_SENT = OUR_INIT_SENT | THEIR_INIT_SENT)
            assert!(
                flags.contains(NegotiatingFundingFlags::INIT_SENT),
                "Channel should be in NegotiatingFunding(INIT_SENT) state, got flags: {:?}",
                flags
            );
        }
        other => {
            panic!(
                "Expected NegotiatingFunding(INIT_SENT) state before restart, got: {:?}",
                other
            );
        }
    }

    // Step 4: Restart node_a (simulating node restart after CKB was closed)
    node_a.restart().await;

    // Wait a bit for the node to fully restart
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Reconnect the peers (channels are reestablished when peers reconnect)
    node_a.connect_to(&mut node_b).await;

    // Wait a bit for channel reestablishment
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Step 5: Verify the channel funding is aborted after restart
    // When a channel in NegotiatingFunding state is reestablished, it should be aborted
    // Wait for the ChannelFundingAborted event
    node_a
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelFundingAborted(channel_id) => {
                assert_eq!(*channel_id, new_channel_id);
                eprintln!(
                    "SUCCESS: Channel {:?} funding was aborted after restart (fix working correctly)",
                    new_channel_id
                );
                true
            }
            _ => false,
        })
        .await;

    // Wait a bit for the channel actor to stop and state to be deleted from storage
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify the channel has been removed from storage
    // (channels with FUNDING_ABORTED are deleted from storage)
    let channel_state_after_restart = node_a.get_channel_actor_state_unchecked(new_channel_id);

    assert!(
        channel_state_after_restart.is_none(),
        "Channel should be removed from storage after funding abort, but still exists with state: {:?}",
        channel_state_after_restart.map(|s| s.state)
    );
}

#[tokio::test]
async fn test_reestablish_restores_send_nonce() {
    init_tracing();
    let (mut node_a, mut node_b, channel_id) =
        create_nodes_with_established_channel(100000000000, 100000000000, true).await;

    // Trigger payment A -> B
    // This will cause A to send AddTlc + CommitmentSigned
    // B will respond with RevokeAndAck
    // This puts B in a state where remote_revocation_nonce_for_send might be None
    let payment_hash = node_a
        .send_payment_keysend(&node_b, 1000, false)
        .await
        .unwrap()
        .payment_hash;
    let payment_hash2 = node_b
        .send_payment_keysend(&node_a, 1000, false)
        .await
        .unwrap()
        .payment_hash;

    // Wait for B to reach the target state where send is None but verify is Some.
    // This confirms we are in the potential deadlock state if persistent.
    let mut caught = false;
    for _ in 0..100 {
        let state = node_b.get_channel_actor_state(channel_id);
        if state.remote_revocation_nonce_for_verify.is_some()
            && state.remote_revocation_nonce_for_send.is_none()
        {
            debug!("Caught target state on Node B!");
            caught = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        caught,
        "Failed to reach target state where send nonce is None"
    );

    assert_eq!(node_a.get_inflight_payment_count().await, 1);
    assert_eq!(node_b.get_inflight_payment_count().await, 1);

    // Now restart node B to simulate disconnect/reconnect
    node_b.restart().await;
    node_b.connect_to(&mut node_a).await;

    node_a
        .expect_debug_event("Reestablished channel in ChannelReady")
        .await;
    node_b
        .expect_debug_event("Reestablished channel in ChannelReady")
        .await;

    tokio::time::sleep(Duration::from_millis(1000)).await;

    // check inflight until 5s
    let now = std::time::Instant::now();
    loop {
        if node_a.get_inflight_payment_count().await == 0
            && node_b.get_inflight_payment_count().await == 0
        {
            break;
        }
        assert!(now.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(node_a.get_inflight_payment_count().await, 0);
    assert_eq!(node_b.get_inflight_payment_count().await, 0);

    assert_eq!(
        node_a.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );
    assert_eq!(
        node_b.get_payment_status(payment_hash2).await,
        PaymentStatus::Success
    );

    let state = node_b.get_channel_actor_state(channel_id);
    println!("Node B State after wait:");
    println!(
        "  Local Commitment Number: {}",
        state.get_local_commitment_number()
    );
    println!(
        "  Remote Commitment Number: {}",
        state.get_remote_commitment_number()
    );
    println!(
        "  Send Nonce: {:?}",
        state.remote_revocation_nonce_for_send.is_some()
    );
    println!(
        "  Verify Nonce: {:?}",
        state.remote_revocation_nonce_for_verify.is_some()
    );

    let state_a = node_a.get_channel_actor_state(channel_id);
    println!("Node A State:");
    println!(
        "  Local Commitment Number: {}",
        state_a.get_local_commitment_number()
    );
    println!(
        "  Remote Commitment Number: {}",
        state_a.get_remote_commitment_number()
    );
    println!(
        "  Send Nonce: {:?}",
        state_a.remote_revocation_nonce_for_send.is_some()
    );
    println!(
        "  Verify Nonce: {:?}",
        state_a.remote_revocation_nonce_for_verify.is_some()
    );

    // Further verification: A can send another payment.
    // Use a new payment to differentiate.
    let res = node_a.send_payment_keysend(&node_b, 2000, false).await;
    let err_string = res.unwrap_err().to_string();
    println!("err: {err_string}");
    // check error string
    assert!(err_string.contains(
        "Send payment first hop error: Failed to send onion packet with error UnknownNextPeer"
    ));
}

#[tokio::test]
#[ignore] // This test is long-running and tests reestablishment under stress. Run separately in CI.
async fn test_node_restart() {
    init_tracing();
    // Open one channel with 10 billion sats on each side
    let (mut node_a, node_b, _channel_id) =
        create_nodes_with_established_channel(100000000000, 100000000000, true).await;

    // Outer loop: 10 restart cycles
    for cycle in 0..10 {
        debug!("=== Restart cycle {} ===", cycle);

        // Inner loop: 100 payment iterations per cycle
        for _i in 0..10 {
            // Send payment concurrently
            // node_a -> node_b keysend (amount=1)
            let _payment1 = node_a.send_payment_keysend(&node_b, 1, false).await;

            // node_b -> node_a keysend (amount=1)
            let _payment3 = node_b.send_payment_keysend(&node_a, 1, false).await;
        }

        // Restart node_a
        debug!("Stopping node_a for restart cycle {}", cycle);
        node_a.restart().await;
        debug!("Starting node_a after restart cycle {}", cycle);

        // Wait 10 seconds
        tokio::time::sleep(Duration::from_secs(10)).await;
    }

    tracing::info!(
        "✓ test_node_restart completed successfully - check logs for BadSignature or panic"
    );
}
