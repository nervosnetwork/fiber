use crate::fiber::types::Pubkey;
use crate::{
    fiber::{
        graph::{ChannelInfo, GraphError, NetworkGraph, NodeInfo, PathEdge},
        network::{get_chain_hash, SendPaymentCommand, SendPaymentData},
        types::{ChannelAnnouncement, ChannelUpdate, Hash256, NodeAnnouncement},
    },
    store::Store,
};
use ckb_types::{
    packed::{OutPoint, Script},
    prelude::Entity,
};
use secp256k1::{PublicKey, SecretKey, XOnlyPublicKey};

use super::test_utils::generate_keypair;

fn generate_key_pairs(num: usize) -> Vec<(SecretKey, PublicKey)> {
    let mut keys = vec![];
    for _ in 0..num {
        keys.push(generate_keypair());
    }
    keys
}

struct MockNetworkGraph {
    pub keys: Vec<PublicKey>,
    pub edges: Vec<(usize, usize, OutPoint)>,
    pub graph: NetworkGraph<Store>,
}

impl MockNetworkGraph {
    pub fn new(node_num: usize) -> Self {
        let temp_path = tempfile::tempdir().unwrap();
        let store = Store::new(temp_path.path());
        let keypairs = generate_key_pairs(node_num + 1);
        let (secret_key1, public_key1) = keypairs[0];
        let mut graph = NetworkGraph::new(store, public_key1.into());
        graph.add_node(NodeInfo {
            node_id: public_key1.into(),
            timestamp: 0,
            anouncement_msg: NodeAnnouncement::new(
                "node0".into(),
                vec![],
                &secret_key1.into(),
                0,
                0,
            ),
        });
        for i in 1..keypairs.len() {
            let (sk, pk) = keypairs[i];
            let node = NodeInfo {
                node_id: pk.into(),
                timestamp: 0,
                anouncement_msg: NodeAnnouncement::new(
                    format!("node{i}").as_str().into(),
                    vec![],
                    &sk.into(),
                    0,
                    0,
                ),
            };
            graph.add_node(node);
        }
        Self {
            keys: keypairs.into_iter().map(|x| x.1).collect(),
            edges: vec![],
            graph,
        }
    }

    pub fn mark_node_failed(&mut self, node: usize) {
        self.graph.mark_node_failed(self.keys[node].into());
    }

    pub fn mark_channel_failed(&mut self, node_a: usize, node_b: usize) {
        let outpoint = self
            .edges
            .iter()
            .find(|(a, b, _)| (*a == node_a && *b == node_b) || (*a == node_b && *b == node_a));
        if let Some((_, _, outpoint)) = outpoint {
            self.graph.mark_channel_failed(&outpoint);
        }
    }

    pub fn add_edge_with_config(
        &mut self,
        node_a: usize,
        node_b: usize,
        capacity: Option<u128>,
        fee_rate: Option<u128>,
        min_htlc_value: Option<u128>,
        max_htlc_value: Option<u128>,
        udt_type_script: Option<Script>,
    ) {
        let public_key1 = self.keys[node_a];
        let public_key2 = self.keys[node_b];
        let idx = self.edges.len() + 1;
        let channel_outpoint = OutPoint::from_slice(&[idx as u8; 36]).unwrap();
        self.edges.push((node_a, node_b, channel_outpoint.clone()));
        let channel_info = ChannelInfo {
            funding_tx_block_number: 0,
            funding_tx_index: 0,
            announcement_msg: ChannelAnnouncement {
                chain_hash: get_chain_hash(),
                node1_id: public_key1.into(),
                node2_id: public_key2.into(),
                channel_outpoint: channel_outpoint.clone(),
                node1_signature: None,
                node2_signature: None,
                capacity: capacity.unwrap_or(1000),
                ckb_key: XOnlyPublicKey::from_slice([0x01; 32].as_ref()).unwrap(),
                ckb_signature: None,
                udt_type_script,
                features: 0,
            },
            timestamp: 0,
            node1_to_node2: None,
            node2_to_node1: None,
        };
        self.graph.add_channel(channel_info);
        let channel_update = ChannelUpdate {
            signature: None,
            chain_hash: get_chain_hash(),
            version: 0,
            message_flags: 1,
            channel_flags: 0,
            tlc_expiry_delta: 144,
            tlc_fee_proportional_millionths: fee_rate.unwrap_or(0),
            tlc_maximum_value: max_htlc_value.unwrap_or(10000),
            tlc_minimum_value: min_htlc_value.unwrap_or(0),
            channel_outpoint: channel_outpoint.clone(),
        };
        self.graph.process_channel_update(channel_update).unwrap();
    }

    pub fn add_edge(
        &mut self,
        node_a: usize,
        node_b: usize,
        capacity: Option<u128>,
        fee_rate: Option<u128>,
    ) {
        self.add_edge_with_config(
            node_a,
            node_b,
            capacity,
            fee_rate,
            Some(0),
            Some(10000),
            None,
        );
    }

    pub fn add_edge_udt(
        &mut self,
        node_a: usize,
        node_b: usize,
        capacity: Option<u128>,
        fee_rate: Option<u128>,
        udt_type_script: Script,
    ) {
        self.add_edge_with_config(
            node_a,
            node_b,
            capacity,
            fee_rate,
            Some(0),
            Some(10000),
            Some(udt_type_script),
        );
    }

    pub fn find_route(
        &self,
        source: usize,
        target: usize,
        amount: u128,
        max_fee: u128,
    ) -> Result<Vec<PathEdge>, GraphError> {
        let source = self.keys[source].into();
        let target = self.keys[target].into();
        self.graph
            .find_route(source, target, amount, Some(max_fee), None, false)
    }

    pub fn find_route_udt(
        &self,
        source: usize,
        target: usize,
        amount: u128,
        max_fee: u128,
        udt_type_script: Script,
    ) -> Result<Vec<PathEdge>, GraphError> {
        let source = self.keys[source].into();
        let target = self.keys[target].into();
        self.graph.find_route(
            source,
            target,
            amount,
            Some(max_fee),
            Some(udt_type_script),
            false,
        )
    }

    pub fn build_route_with_expect(&self, payment_data: &SendPaymentData, expect: Vec<usize>) {
        let route = self.graph.build_route(payment_data);
        assert!(route.is_ok());
        let route = route.unwrap();
        let nodes = route.iter().filter_map(|x| x.next_hop).collect::<Vec<_>>();
        let expecptected_nodes: Vec<Pubkey> = expect
            .iter()
            .map(|x| self.keys[*x].into())
            .collect::<Vec<_>>();
        assert_eq!(nodes, expecptected_nodes);
    }
}

#[test]
fn test_graph_channel_info() {
    let mut mock_network = MockNetworkGraph::new(1);
    mock_network.add_edge(0, 1, Some(1000), Some(1));
    for i in 1..=mock_network.edges.len() {
        let channel_info = mock_network
            .graph
            .get_channel(&OutPoint::from_slice(&[i as u8; 36]).unwrap());
        assert!(channel_info.is_some());

        let channel_info = channel_info.unwrap();
        let channel_info_ser = serde_json::to_string(&channel_info).unwrap();
        let channel_info_de: ChannelInfo = serde_json::from_str(&channel_info_ser).unwrap();
        assert_eq!(*channel_info, channel_info_de);
    }
}

#[test]
fn test_graph_graph_apis() {
    let mut mock_network = MockNetworkGraph::new(4);
    let node1 = mock_network.keys[1];
    let node2 = mock_network.keys[2];
    let node3 = mock_network.keys[3];
    assert!(mock_network.graph.get_node(node1.into()).is_some());
    assert!(mock_network.graph.get_node(node2.into()).is_some());

    let node1_channels = mock_network.graph.get_channels_by_peer(node1.into());
    assert_eq!(node1_channels.count(), 0);
    let node2_channels = mock_network.graph.get_channels_by_peer(node2.into());
    assert_eq!(node2_channels.count(), 0);

    mock_network.add_edge(1, 2, Some(1000), Some(1));
    let node1_channels = mock_network.graph.get_channels_by_peer(node1.into());
    assert_eq!(node1_channels.count(), 1);
    let node2_channels = mock_network.graph.get_channels_by_peer(node2.into());
    assert_eq!(node2_channels.count(), 1);

    mock_network.add_edge(1, 3, Some(1000), Some(1));
    let node1_channels = mock_network.graph.get_channels_by_peer(node1.into());
    assert_eq!(node1_channels.count(), 2);

    let node1_channels = mock_network.graph.get_channels_by_peer(node3.into());
    assert_eq!(node1_channels.count(), 1);
}

#[test]
fn test_graph_find_path_basic() {
    let mut network = MockNetworkGraph::new(4);
    network.add_edge(1, 2, Some(1), Some(2));
    let node2 = network.keys[2];

    let route = network.find_route(1, 2, 100, 1000);
    assert!(route.is_err());

    network.add_edge(1, 2, Some(120), Some(2));
    let route = network.find_route(1, 2, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[0].channel_outpoint, network.edges[1].2);

    let route = network.find_route(1, 3, 10, 100);
    assert!(route.is_err());
}

#[test]
fn test_graph_find_path_three_nodes() {
    let mut network = MockNetworkGraph::new(3);
    network.add_edge(1, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    let node2 = network.keys[2];
    let node3 = network.keys[3];

    // Test route from node 1 to node 3
    let route = network.find_route(1, 3, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[1].target, node3.into());
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);
    assert_eq!(route[1].channel_outpoint, network.edges[1].2);

    // Test route from node 1 to node 2
    let route = network.find_route(1, 2, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);

    // Test route from node 2 to node 3
    let route = network.find_route(2, 3, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node3.into());
    assert_eq!(route[0].channel_outpoint, network.edges[1].2);

    // Test route from node 3 to node 1 (should fail)
    let route = network.find_route(3, 1, 100, 1000);
    assert!(route.is_err());
}

#[test]
fn test_graph_find_path_fee() {
    let mut network = MockNetworkGraph::new(5);

    network.add_edge(1, 2, Some(1000), Some(30000));
    network.add_edge(2, 4, Some(1000), Some(10000));

    network.add_edge(1, 3, Some(1000), Some(20000));
    network.add_edge(3, 4, Some(1000), Some(10000));

    let route = network.find_route(1, 4, 100, 1000);

    assert!(route.is_ok());
    let route = route.unwrap();

    // make sure we choose the path with lower fees
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].channel_outpoint, network.edges[2].2);
    assert_eq!(route[1].channel_outpoint, network.edges[3].2);
}

#[test]
fn test_graph_find_path_direct_linear() {
    let mut network = MockNetworkGraph::new(6);

    network.add_edge(1, 2, Some(1000), Some(4));
    network.add_edge(2, 3, Some(1000), Some(3));
    network.add_edge(3, 4, Some(1000), Some(2));
    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_route(1, 5, 100, 1000);

    assert!(route.is_ok());
    let route = route.unwrap();

    assert_eq!(route.len(), 4);
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);
    assert_eq!(route[1].channel_outpoint, network.edges[1].2);
    assert_eq!(route[2].channel_outpoint, network.edges[2].2);
    assert_eq!(route[3].channel_outpoint, network.edges[3].2);
}

#[test]
fn test_graph_find_path_cycle() {
    let mut network = MockNetworkGraph::new(6);

    network.add_edge(1, 2, Some(1000), Some(4));
    network.add_edge(2, 3, Some(1000), Some(3));
    network.add_edge(3, 1, Some(1000), Some(2));

    let route = network.find_route(1, 3, 100, 1000);

    assert!(route.is_ok());

    network.add_edge(3, 4, Some(1000), Some(2));
    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_route(1, 5, 100, 1000);
    assert!(route.is_ok());
}

#[test]
fn test_graph_find_path_cycle_in_middle() {
    let mut network = MockNetworkGraph::new(6);

    network.add_edge(1, 2, Some(1000), Some(4));

    network.add_edge(2, 3, Some(1000), Some(3));
    network.add_edge(3, 4, Some(1000), Some(2));
    network.add_edge(4, 2, Some(1000), Some(2));

    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_route(1, 5, 100, 1000);
    assert!(route.is_ok());
}

#[test]
fn test_graph_find_path_loop_exit() {
    let mut network = MockNetworkGraph::new(6);

    // node2 and node3 are connected with each other, node1 is disconnected
    network.add_edge(2, 3, Some(1000), Some(3));
    network.add_edge(3, 2, Some(1000), Some(2));

    let route = network.find_route(1, 3, 100, 1000);
    assert!(route.is_err());

    // now add a path from node1 to node2, so that node1 can reach node3
    network.add_edge(1, 2, Some(1000), Some(4));
    let route = network.find_route(1, 3, 100, 1000);
    assert!(route.is_ok());
}

#[test]
fn test_graph_find_path_amount_failed() {
    let mut network = MockNetworkGraph::new(6);

    network.add_edge(1, 2, Some(1000), Some(4));
    network.add_edge(2, 3, Some(1000), Some(4));
    network.add_edge(3, 4, Some(1000), Some(4));
    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_route(1, 5, 1000, 10);
    assert!(route.is_err());
}

#[test]
fn test_graph_find_optimal_path() {
    let mut network = MockNetworkGraph::new(6);

    // Direct path with high fee
    network.add_edge(1, 5, Some(2000), Some(50000));

    // Longer path with lower total fee
    network.add_edge(1, 2, Some(2000), Some(10000));
    network.add_edge(2, 3, Some(2000), Some(10000));
    network.add_edge(3, 4, Some(2000), Some(10000));
    network.add_edge(4, 5, Some(2000), Some(10000));

    // Path with insufficient capacity
    network.add_edge(1, 6, Some(500), Some(10000));
    network.add_edge(6, 5, Some(500), Some(10000));

    let route = network.find_route(1, 5, 1000, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();

    // Check that the algorithm chose the longer path with lower fees
    assert_eq!(route.len(), 4);
    assert_eq!(route[0].channel_outpoint, network.edges[1].2);
    assert_eq!(route[1].channel_outpoint, network.edges[2].2);
    assert_eq!(route[2].channel_outpoint, network.edges[3].2);
    assert_eq!(route[3].channel_outpoint, network.edges[4].2);

    // Test with a smaller amount that allows using the direct path
    let small_route = network.find_route(1, 5, 100, 100);
    assert!(small_route.is_ok());
    let small_route = small_route.unwrap();

    // Check that the algorithm chose the direct path for a smaller amount
    assert_eq!(small_route.len(), 2);
    assert_eq!(small_route[0].channel_outpoint, network.edges[5].2);
    assert_eq!(small_route[1].channel_outpoint, network.edges[6].2);
}

#[test]
fn test_graph_find_path_err() {
    let mut network = MockNetworkGraph::new(6);
    let (node1, _node5) = (network.keys[1], network.keys[5]);

    network.add_edge(1, 2, Some(1000), Some(4));
    let route = network.find_route(1, 1, 100, 1000);
    assert!(route.is_err());

    let no_exits_public_key = network.keys[0];
    let route = network.graph.find_route(
        node1.into(),
        no_exits_public_key.into(),
        100,
        Some(1000),
        None,
        false,
    );
    assert!(route.is_err());

    let route = network.graph.find_route(
        no_exits_public_key.into(),
        node1.into(),
        100,
        Some(1000),
        None,
        false,
    );
    assert!(route.is_err());
}

#[test]
fn test_graph_build_route_three_nodes() {
    let mut network = MockNetworkGraph::new(3);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    let _node0 = network.keys[0];
    let node2 = network.keys[2];
    let node3 = network.keys[3];
    // Test build route from node1 to node3
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0].channel_outpoint, Some(network.edges[0].2.clone()));
    assert_eq!(route[1].channel_outpoint, Some(network.edges[1].2.clone()));

    assert_eq!(route[0].next_hop, Some(node2.into()));
    assert_eq!(route[1].next_hop, Some(node3.into()));
    assert_eq!(route[2].next_hop, None);

    assert_eq!(route[0].amount, 101);
    assert_eq!(route[1].amount, 100);
    assert_eq!(route[2].amount, 100);
}

#[test]
fn test_graph_build_route_exceed_max_htlc_value() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with max_htlc_value set to 50
    network.add_edge_with_config(0, 2, Some(500), Some(2), None, Some(50), None);
    network.add_edge_with_config(2, 3, Some(500), Some(2), None, Some(50), None);
    let node3 = network.keys[3];

    // Test build route from node1 to node3 with amount exceeding max_htlc_value
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100, // Exceeds max_htlc_value of 50
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_err());
}

#[test]
fn test_graph_build_route_below_min_htlc_value() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_htlc_value set to 50
    network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None);
    network.add_edge_with_config(2, 3, Some(500), Some(2), Some(50), None, None);
    let node3 = network.keys[3];

    // Test build route from node1 to node3 with amount below min_htlc_value
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 10, // Below min_htlc_value of 50
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_err());
}

#[test]
fn test_graph_find_path_udt() {
    let mut network = MockNetworkGraph::new(3);
    let udt_type_script = Script::default();
    network.add_edge_udt(1, 2, Some(1000), Some(1), udt_type_script.clone());
    let node2 = network.keys[2];

    let route = network.find_route_udt(1, 2, 100, 1000, udt_type_script.clone());
    assert!(route.is_ok());

    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);

    let route = network.find_route(1, 3, 10, 100);
    assert!(route.is_err());
}

#[test]
fn test_graph_mark_failed_channel() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    let node3 = network.keys[3];

    network.mark_channel_failed(2, 3);
    // Test build route from node1 to node3
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_err());

    network.add_edge(0, 5, Some(500), Some(2));
    network.add_edge(5, 3, Some(500), Some(2));

    // Test build route from node1 to node3
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_ok());
}

#[test]
fn test_graph_mark_failed_node() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));

    let node3 = network.keys[3];
    let node4 = network.keys[4];

    // Test build route from node1 to node3
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_ok());

    // Test build route from node1 to node4 should be Ok
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node4.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_ok());

    network.mark_node_failed(2);

    // Test build route from node1 to node3
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_err());

    // Test build route from node1 to node4
    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node4.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_err());
}

#[test]
fn test_graph_payment_self_default_is_false() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));

    let node0 = network.keys[0];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(node0.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_htlc_expiry_delta: Some(100),
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: false,
    };
    let payment_data = SendPaymentData::new(command, node0.into());
    let error = payment_data.unwrap_err().to_string();
    assert!(error.contains("can not pay self"));

    let route = network.graph.build_route(&SendPaymentData {
        target_pubkey: node0.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_htlc_expiry_delta: Some(100),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
    });
    assert!(route.is_err());
}

#[test]
fn test_graph_payment_pay_single_path() {
    let mut network = MockNetworkGraph::new(9);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));
    network.add_edge(4, 5, Some(500), Some(2));
    network.add_edge(5, 6, Some(500), Some(2));

    let node0 = network.keys[0];
    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[6].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_htlc_expiry_delta: Some(100),
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
    };
    let payment_data = SendPaymentData::new(command, node0.into());
    assert!(payment_data.is_ok());
    let payment_data = payment_data.unwrap();

    network.add_edge(4, 0, Some(1000), Some(2));
    network.build_route_with_expect(&payment_data, vec![2, 4, 5, 6]);
}

#[test]
fn test_graph_payment_pay_self_with_one_node() {
    let mut network = MockNetworkGraph::new(9);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 0, Some(500), Some(2));

    let node0 = network.keys[0];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_htlc_expiry_delta: Some(100),
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
    };
    let payment_data = SendPaymentData::new(command, node0.into());
    assert!(payment_data.is_ok());
    let payment_data = payment_data.unwrap();

    let route = network.graph.build_route(&payment_data);
    assert!(route.is_ok());
}

#[test]
fn test_graph_payment_pay_self_will_ok() {
    let mut network = MockNetworkGraph::new(9);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));
    network.add_edge(4, 5, Some(500), Some(2));
    network.add_edge(5, 6, Some(500), Some(2));

    let node0 = network.keys[0];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_htlc_expiry_delta: Some(100),
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
    };
    let payment_data = SendPaymentData::new(command, node0.into());
    assert!(payment_data.is_ok());
    let payment_data = payment_data.unwrap();

    let route = network.graph.build_route(&payment_data);
    // since we don't have a route to node0, it will fail
    assert!(route.is_err());

    // add a long path
    network.add_edge(6, 0, Some(500), Some(2));
    network.build_route_with_expect(&payment_data, vec![2, 4, 5, 6, 0]);

    // now add another shorter path
    network.add_edge(4, 0, Some(1000), Some(2));
    network.build_route_with_expect(&payment_data, vec![2, 4, 0]);

    // now add another shorter path
    network.add_edge(2, 0, Some(1000), Some(2));
    network.build_route_with_expect(&payment_data, vec![2, 0]);
}
