use super::test_utils::{init_tracing, NetworkNode};
use crate::{
    fiber::{
        channel::ShutdownInfo,
        config::DEFAULT_TLC_EXPIRY_DELTA,
        graph::ChannelUpdateInfo,
        network::{NetworkActorStateStore, SendPaymentCommand, SendPaymentData},
        tests::test_utils::NetworkNodeConfigBuilder,
        types::{
            BroadcastMessage, ChannelAnnouncement, ChannelUpdateChannelFlags, NodeAnnouncement,
            Privkey, Pubkey,
        },
        NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    },
    gen_rand_fiber_public_key, gen_rand_secp256k1_keypair_tuple, gen_rand_sha256_hash,
    invoice::InvoiceBuilder,
    now_timestamp_as_millis_u64, ChannelTestContext, NetworkServiceEvent,
};
use ckb_hash::blake2b_256;
use ckb_jsonrpc_types::Status;
use ckb_types::{
    core::TransactionView,
    packed::{CellOutput, ScriptBuilder},
};
use ckb_types::{
    packed::OutPoint,
    prelude::{Builder, Entity, Pack},
};
use musig2::PartialSignature;
use std::{borrow::Cow, str::FromStr, time::Duration};
use tentacle::{
    multiaddr::{MultiAddr, Multiaddr, Protocol},
    secio::PeerId,
};

fn get_test_priv_key() -> Privkey {
    Privkey::from_slice(&[42u8; 32])
}

fn get_test_pub_key() -> Pubkey {
    get_test_priv_key().pubkey()
}

fn get_test_peer_id() -> PeerId {
    let pub_key = get_test_pub_key().into();
    PeerId::from_public_key(&pub_key)
}

fn get_fake_peer_id_and_address() -> (PeerId, MultiAddr) {
    let peer_id = PeerId::random();
    let mut address = MultiAddr::from_str(&format!(
        "/ip4/{}.{}.{}.{}/tcp/{}",
        rand::random::<u8>(),
        rand::random::<u8>(),
        rand::random::<u8>(),
        rand::random::<u8>(),
        rand::random::<u16>()
    ))
    .expect("valid multiaddr");
    address.push(Protocol::P2P(Cow::Owned(peer_id.clone().into_bytes())));
    (peer_id, address)
}

fn create_fake_channel_announcement_mesage(
    priv_key: Privkey,
    capacity: u64,
    outpoint: OutPoint,
) -> (NodeAnnouncement, NodeAnnouncement, ChannelAnnouncement) {
    let x_only_pub_key = priv_key.x_only_pub_key();
    let sk1 = Privkey::from([1u8; 32]);
    let node_announcement1 = create_node_announcement_mesage_with_priv_key(&sk1);
    let sk2 = Privkey::from([2u8; 32]);
    let node_announcement2 = create_node_announcement_mesage_with_priv_key(&sk2);

    let mut channel_announcement = ChannelAnnouncement::new_unsigned(
        &sk1.pubkey(),
        &sk2.pubkey(),
        outpoint,
        &x_only_pub_key,
        capacity as u128,
        None,
    );
    let message = channel_announcement.message_to_sign();

    channel_announcement.ckb_signature = Some(priv_key.sign_schnorr(message));
    channel_announcement.node1_signature = Some(sk1.sign(message));
    channel_announcement.node2_signature = Some(sk2.sign(message));
    (node_announcement1, node_announcement2, channel_announcement)
}

fn create_node_announcement_mesage_with_priv_key(priv_key: &Privkey) -> NodeAnnouncement {
    let node_name = "fake node";
    let addresses =
        vec!["/ip4/1.1.1.1/tcp/8346/p2p/QmaFDJb9CkMrXy7nhTWBY5y9mvuykre3EzzRsCJUAVXprZ"]
            .iter()
            .map(|x| MultiAddr::from_str(x).expect("valid multiaddr"))
            .collect();
    NodeAnnouncement::new(
        node_name.into(),
        addresses,
        priv_key,
        now_timestamp_as_millis_u64(),
        0,
    )
}

fn create_fake_node_announcement_mesage() -> NodeAnnouncement {
    let priv_key = get_test_priv_key();
    create_node_announcement_mesage_with_priv_key(&priv_key)
}

#[tokio::test]
async fn test_save_our_own_node_announcement_to_graph() {
    let mut node = NetworkNode::new().await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node.stop().await;
    let nodes = node.get_network_graph_nodes().await;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].node_id, node.get_public_key());
}

#[tokio::test]
#[should_panic]
async fn test_set_announced_addrs_with_invalid_peer_id() {
    let mut node = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .fiber_config_updater(|config| {
                config.announced_addrs = vec![
                    "/ip4/1.1.1.1/tcp/8346/p2p/QmaFDJb9CkMrXy7nhTWBY5y9mvuykre3EzzRsCJUAVXprZ"
                        .to_string(),
                ];
            })
            .build(),
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node.stop().await;
    let nodes = node.get_network_graph_nodes().await;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].node_id, node.get_public_key());
}

#[tokio::test]
async fn test_set_announced_addrs_with_valid_peer_id() {
    let mut node = NetworkNode::new().await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node.stop().await;

    let peer_id = node.get_peer_id();
    let addr = format!("/ip4/1.1.1.1/tcp/8346/p2p/{}", peer_id);
    let multiaddr = Multiaddr::from_str(&addr).expect("valid multiaddr");
    let mut node = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .base_dir(node.base_dir.clone())
            .fiber_config_updater(move |config| {
                config.announced_addrs = vec![addr.clone()];
            })
            .build(),
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node.stop().await;
    let nodes = node.get_network_graph_nodes().await;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].node_id, node.get_public_key());
    assert_eq!(
        nodes[0].addresses.iter().find(|x| *x == &multiaddr),
        Some(&multiaddr)
    );
}

#[tokio::test]
async fn test_set_announced_addrs_without_p2p() {
    let addr = "/ip4/1.1.1.1/tcp/8346".to_string();
    let cloned_addr = addr.clone();
    let mut node = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .fiber_config_updater(move |config| {
                config.announced_addrs = vec![cloned_addr];
            })
            .build(),
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    node.stop().await;
    let peer_id = node.get_peer_id();
    let peer_id_bytes = peer_id.clone().into_bytes();
    let multiaddr =
        Multiaddr::from_str(&format!("{}/p2p/{}", addr, peer_id)).expect("valid multiaddr");
    let nodes = node.get_network_graph_nodes().await;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].node_id, node.get_public_key());
    assert!(nodes[0].addresses.clone().iter_mut().all(|multiaddr| {
        match multiaddr.pop() {
            Some(Protocol::P2P(peer_id)) => peer_id.as_ref() == peer_id_bytes.as_slice(),
            _ => false,
        }
    }));
    assert_eq!(
        nodes[0].addresses.iter().find(|x| *x == &multiaddr),
        Some(&multiaddr)
    );
}

#[tokio::test]
async fn test_sync_channel_announcement_on_startup() {
    init_tracing();

    let mut node1 = NetworkNode::new_with_node_name("node1").await;
    let mut node2 = NetworkNode::new_with_node_name("node2").await;

    let capacity = 42;
    let priv_key: Privkey = get_test_priv_key();
    let pubkey = priv_key.x_only_pub_key().serialize();
    let pubkey_hash = &blake2b_256(pubkey.as_slice())[0..20];
    let tx = TransactionView::new_advanced_builder()
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(ScriptBuilder::default().args(pubkey_hash.pack()).build())
                .build(),
        )
        .output_data(vec![0u8; 8].pack())
        .build();
    let outpoint = tx.output_pts()[0].clone();
    let (node_announcement_1, node_announcement_2, channel_announcement) =
        create_fake_channel_announcement_mesage(priv_key, capacity, outpoint);

    assert_eq!(node1.submit_tx(tx.clone()).await, Status::Committed);

    for message in [
        BroadcastMessage::NodeAnnouncement(node_announcement_1.clone()),
        BroadcastMessage::NodeAnnouncement(node_announcement_2.clone()),
        BroadcastMessage::ChannelAnnouncement(channel_announcement.clone()),
    ] {
        node1
            .network_actor
            .send_message(NetworkActorMessage::Event(
                NetworkActorEvent::GossipMessage(
                    get_test_peer_id(),
                    message.create_broadcast_messages_filter_result(),
                ),
            ))
            .expect("send message to network actor");
    }

    node1.connect_to(&node2).await;

    assert_eq!(node2.submit_tx(tx.clone()).await, Status::Committed);

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let channels = node2.get_network_graph_channels().await;
    assert!(!channels.is_empty());
}

#[tokio::test]
async fn test_node1_node2_channel_update() {
    let channel_context = ChannelTestContext::gen();
    let funding_tx = channel_context.funding_tx.clone();
    let out_point = channel_context.channel_outpoint().clone();
    let channel_announcement = channel_context.channel_announcement.clone();
    let mut node = NetworkNode::new().await;
    node.submit_tx(funding_tx).await;
    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelAnnouncement(channel_announcement)
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let channel_update_of_node1 =
        channel_context.create_channel_update_of_node1(ChannelUpdateChannelFlags::empty(), 1, 1, 1);
    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelUpdate(channel_update_of_node1.clone())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let new_channel_info = node.get_network_graph_channel(&out_point).await.unwrap();
    assert_eq!(
        new_channel_info.update_of_node1,
        Some(ChannelUpdateInfo::from(&channel_update_of_node1))
    );

    let channel_update_of_node2 =
        channel_context.create_channel_update_of_node2(ChannelUpdateChannelFlags::empty(), 2, 2, 2);
    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelUpdate(channel_update_of_node2.clone())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let new_channel_info = node.get_network_graph_channel(&out_point).await.unwrap();
    assert_eq!(
        new_channel_info.update_of_node1,
        Some(ChannelUpdateInfo::from(&channel_update_of_node1))
    );
    assert_eq!(
        new_channel_info.update_of_node2,
        Some(ChannelUpdateInfo::from(&channel_update_of_node2))
    );
}

#[tokio::test]
async fn test_channel_update_version() {
    let channel_context = ChannelTestContext::gen();
    let funding_tx = channel_context.funding_tx.clone();
    let out_point = channel_context.channel_outpoint().clone();
    let mut node = NetworkNode::new().await;
    node.submit_tx(funding_tx).await;
    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelAnnouncement(channel_context.channel_announcement.clone())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let mut channel_updates = vec![];
    for i in 0u8..3 {
        // Make sure the timestamp is different.
        tokio::time::sleep(tokio::time::Duration::from_millis(3)).await;
        channel_updates.push(channel_context.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            i.into(),
            i.into(),
            i.into(),
        ))
    }
    let [channel_update_1, channel_update_2, channel_update_3] =
        channel_updates.try_into().expect("3 channel updates");

    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelUpdate(channel_update_2.clone())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let new_channel_info = node.get_network_graph_channel(&out_point).await.unwrap();
    assert_eq!(
        new_channel_info.update_of_node1,
        Some(ChannelUpdateInfo::from(&channel_update_2))
    );

    // Old channel update will not replace the new one.
    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelUpdate(channel_update_1.clone())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let new_channel_info = node.get_network_graph_channel(&out_point).await.unwrap();
    assert_eq!(
        new_channel_info.update_of_node1,
        Some(ChannelUpdateInfo::from(&channel_update_2))
    );

    // New channel update will replace the old one.
    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                get_test_peer_id(),
                BroadcastMessage::ChannelUpdate(channel_update_3.clone())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let new_channel_info = node.get_network_graph_channel(&out_point).await.unwrap();
    assert_eq!(
        new_channel_info.update_of_node1,
        Some(ChannelUpdateInfo::from(&channel_update_3))
    );
}

#[tokio::test]
async fn test_sync_node_announcement_version() {
    init_tracing();

    let node = NetworkNode::new_with_node_name("node").await;
    let test_pub_key = get_test_pub_key();
    let test_peer_id = get_test_peer_id();

    let [node_announcement_message_version1, node_announcement_message_version2, node_announcement_message_version3] = [
        create_fake_node_announcement_mesage(),
        create_fake_node_announcement_mesage(),
        create_fake_node_announcement_mesage(),
    ];
    let timestamp_version2 = node_announcement_message_version2.timestamp;
    let timestamp_version3 = node_announcement_message_version3.timestamp;

    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                test_peer_id.clone(),
                BroadcastMessage::NodeAnnouncement(node_announcement_message_version2)
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");

    // Wait for the broadcast message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let node_info = node.get_network_graph_node(&test_pub_key).await;
    match node_info {
        Some(n) if n.timestamp == timestamp_version2 => {}
        _ => panic!(
            "Must have version 2 announcement message, found {:?}",
            &node_info
        ),
    }

    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                test_peer_id.clone(),
                BroadcastMessage::NodeAnnouncement(node_announcement_message_version1)
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");

    // Wait for the broadcast message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let node_info = node.get_network_graph_node(&test_pub_key).await;
    match node_info {
        Some(n) if n.timestamp == timestamp_version2 => {}
        _ => panic!(
            "Must have version 2 announcement message, found {:?}",
            &node_info
        ),
    }

    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                test_peer_id.clone(),
                BroadcastMessage::NodeAnnouncement(node_announcement_message_version3)
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");
    // Wait for the broadcast message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let node_info = node.get_network_graph_node(&test_pub_key).await;
    match node_info {
        Some(n) if n.timestamp == timestamp_version3 => {}
        _ => panic!(
            "Must have version 3 announcement message, found {:?}",
            &node_info
        ),
    }
}

// Test that we can sync the network graph with peers.
// We will first create a node and announce a fake node announcement to the network.
// Then we will create another node and connect to the first node.
// We will see if the second node has the fake node announcement.
#[tokio::test]
async fn test_sync_node_announcement_on_startup() {
    init_tracing();

    let mut node1 = NetworkNode::new_with_node_name("node1").await;
    let node2 = NetworkNode::new_with_node_name("node2").await;
    let test_pub_key = get_test_pub_key();
    let test_peer_id = get_test_peer_id();

    node1
        .network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                test_peer_id.clone(),
                BroadcastMessage::NodeAnnouncement(create_fake_node_announcement_mesage())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");

    node1.connect_to(&node2).await;

    // Wait for the broadcast message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let node_info = node1.get_network_graph_node(&test_pub_key).await;
    assert!(node_info.is_some());

    let node_info = node2.get_network_graph_node(&test_pub_key).await;
    assert!(node_info.is_some());
}

#[tokio::test]
async fn test_sync_node_announcement_of_connected_nodes() {
    let [node1, node2] = NetworkNode::new_n_interconnected_nodes().await;

    // Wait for the broadcast message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let node_info = node1.get_network_graph_node(&node2.get_public_key()).await;
    assert!(node_info.is_some());

    let node_info = node2.get_network_graph_node(&node1.get_public_key()).await;
    assert!(node_info.is_some());
}

// Test that we can sync the network graph with peers.
// We will first create a node and announce a fake node announcement to the network.
// Then we will create another node and connect to the first node.
// We will see if the second node has the fake node announcement.
#[tokio::test]
async fn test_sync_node_announcement_after_restart() {
    init_tracing();

    let [node1, mut node2] = NetworkNode::new_n_interconnected_nodes().await;

    node2.stop().await;

    let test_pub_key = get_test_pub_key();
    let test_peer_id = get_test_peer_id();
    node1
        .network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                test_peer_id.clone(),
                BroadcastMessage::NodeAnnouncement(create_fake_node_announcement_mesage())
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");

    node2.start().await;
    node2.connect_to(&node1).await;

    // Wait for the broadcast message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let node_info = node1.get_network_graph_node(&test_pub_key).await;
    assert!(node_info.is_some());

    let node_info = node2.get_network_graph_node(&test_pub_key).await;
    assert!(node_info.is_some());
}

#[tokio::test]
async fn test_persisting_network_state() {
    let mut node = NetworkNode::new().await;
    let state = node.store.clone();
    let peer_id = node.peer_id.clone();
    node.stop().await;
    assert!(state.get_network_actor_state(&peer_id).is_some())
}

#[tokio::test]
async fn test_persisting_bootnode() {
    let (boot_peer_id, address) = get_fake_peer_id_and_address();
    let address_string = format!("{}", &address);

    let mut node = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .fiber_config_updater(move |config| config.bootnode_addrs = vec![address_string])
            .build(),
    )
    .await;
    let state = node.store.clone();
    let peer_id = node.peer_id.clone();
    node.stop().await;

    let state = state.get_network_actor_state(&peer_id).unwrap();
    let peers = state.sample_n_peers_to_connect(1);
    assert_eq!(peers.get(&boot_peer_id), Some(&vec![address]));
}

#[tokio::test]
async fn test_persisting_announced_nodes() {
    init_tracing();

    let mut node = NetworkNode::new_with_node_name("test").await;

    let announcement = create_fake_node_announcement_mesage();
    let node_pk = announcement.node_id;
    let peer_id = node_pk.tentacle_peer_id();

    node.network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::GossipMessage(
                peer_id.clone(),
                BroadcastMessage::NodeAnnouncement(announcement)
                    .create_broadcast_messages_filter_result(),
            ),
        ))
        .expect("send message to network actor");

    // Wait for the above message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    node.stop().await;
    let peers = node
        .with_network_graph(|graph| graph.sample_n_peers_to_connect(1))
        .await;
    assert!(peers.get(&peer_id).is_some());
}

#[tokio::test]
async fn test_connecting_to_bootnode() {
    let boot_node = NetworkNode::new().await;
    let boot_node_address = format!("{}", boot_node.get_node_address());
    let boot_node_id = &boot_node.peer_id;

    let mut node = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .fiber_config_updater(move |config| config.bootnode_addrs = vec![boot_node_address])
            .build(),
    )
    .await;

    node.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == boot_node_id),
    )
    .await;
}

#[tokio::test]
async fn test_saving_and_connecting_to_node() {
    init_tracing();

    let node1 = NetworkNode::new().await;
    let node1_address = node1.get_node_address().clone();
    let node1_id = &node1.peer_id;

    let mut node2 = NetworkNode::new().await;

    node2
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::SavePeerAddress(node1_address),
        ))
        .expect("send message to network actor");

    // Wait for the above message to be processed.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    node2.restart().await;

    node2.expect_event(
        |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == node1_id),
    )
    .await;
}

#[test]
fn test_announcement_message_serialize() {
    let capacity = 42;
    let priv_key: Privkey = get_test_priv_key();
    let pubkey = priv_key.x_only_pub_key().serialize();
    let pubkey_hash = &blake2b_256(pubkey.as_slice())[0..20];
    let tx = TransactionView::new_advanced_builder()
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(ScriptBuilder::default().args(pubkey_hash.pack()).build())
                .build(),
        )
        .output_data(vec![0u8; 8].pack())
        .build();
    let outpoint = tx.output_pts()[0].clone();
    let (_, _, mut channel_announcement) =
        create_fake_channel_announcement_mesage(priv_key, capacity, outpoint);

    channel_announcement.udt_type_script = Some(ScriptBuilder::default().build());

    let serialized = bincode::serialize(&channel_announcement).unwrap();
    let deserialized: ChannelAnnouncement = bincode::deserialize(&serialized).unwrap();
    assert_eq!(channel_announcement, deserialized);

    let shutdown_info = ShutdownInfo {
        close_script: ScriptBuilder::default().build(),
        fee_rate: 100 as u64,
        signature: Some(PartialSignature::max()),
    };
    let serialized = bincode::serialize(&shutdown_info).unwrap();
    let deserialized: ShutdownInfo = bincode::deserialize(&serialized).unwrap();
    assert_eq!(shutdown_info, deserialized);
}

#[test]
fn test_send_payment_validate_payment_hash() {
    let send_command = SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(10000),
        payment_hash: None,
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
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("payment_hash is missing"));
}

#[test]
fn test_send_payment_validate_amount() {
    let send_command = SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: None,
        payment_hash: None,
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
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("amount is missing"));
}

#[test]
fn test_send_payment_validate_invoice() {
    use crate::invoice::Attribute;
    use crate::invoice::Currency;
    use secp256k1::Secp256k1;

    let gen_payment_hash = gen_rand_sha256_hash();
    let (private_key, public_key) = gen_rand_secp256k1_keypair_tuple();

    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .fallback_address("address".to_string())
        .expiry_time(Duration::from_secs(1024))
        .payee_pub_key(public_key)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcMinimumExpiryDelta(
            DEFAULT_TLC_EXPIRY_DELTA,
        ))
        .add_attr(Attribute::Description("description".to_string()))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let invoice_encoded = invoice.to_string();
    let send_command = SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: None,
        payment_hash: None,
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: Some(invoice_encoded.clone()),
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: None,
        udt_type_script: None,
        allow_self_payment: false,
        hop_hints: None,
        dry_run: false,
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("target_pubkey does not match the invoice"));

    let send_command = SendPaymentCommand {
        target_pubkey: None,
        amount: Some(10),
        payment_hash: None,
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: Some(invoice_encoded.clone()),
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: None,
        udt_type_script: None,
        allow_self_payment: false,
        hop_hints: None,
        dry_run: false,
    };

    // keysend is set with invoice, should be error
    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("amount does not match the invoice"));

    let send_command = SendPaymentCommand {
        target_pubkey: None,
        amount: None,
        payment_hash: None,
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: Some(invoice_encoded.clone()),
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: Some(true),
        udt_type_script: None,
        allow_self_payment: false,
        hop_hints: None,
        dry_run: false,
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());

    // normal invoice send payment
    let send_command = SendPaymentCommand {
        target_pubkey: None,
        amount: None,
        payment_hash: None,
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: Some(invoice_encoded.clone()),
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: None,
        udt_type_script: None,
        allow_self_payment: false,
        hop_hints: None,
        dry_run: false,
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_ok());

    // normal keysend send payment
    let send_command = SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(10),
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
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_ok());

    // invoice with invalid final_tlc_expiry_delta
    let send_command = SendPaymentCommand {
        target_pubkey: None,
        amount: None,
        payment_hash: None,
        final_tlc_expiry_delta: Some(11),
        tlc_expiry_limit: None,
        invoice: Some(invoice_encoded.clone()),
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: None,
        udt_type_script: None,
        allow_self_payment: false,
        hop_hints: None,
        dry_run: false,
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("invalid final_tlc_expiry_delta"));

    // invoice with invalid final_tlc_expiry_delta
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .fallback_address("address".to_string())
        .expiry_time(Duration::from_secs(1024))
        .payee_pub_key(public_key)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcMinimumExpiryDelta(11))
        .add_attr(Attribute::Description("description".to_string()))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    let invoice_encoded = invoice.to_string();
    let send_command = SendPaymentCommand {
        target_pubkey: None,
        amount: None,
        payment_hash: None,
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: Some(invoice_encoded.clone()),
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: None,
        udt_type_script: None,
        allow_self_payment: false,
        hop_hints: None,
        dry_run: false,
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("invalid final_tlc_expiry_delta"));
}

#[test]
fn test_send_payment_validate_htlc_expiry_delta() {
    let send_command = SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(1000),
        payment_hash: Some(gen_rand_sha256_hash()),
        final_tlc_expiry_delta: Some(100),
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
    };

    let result = SendPaymentData::new(send_command);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("invalid final_tlc_expiry_delta"));
}
