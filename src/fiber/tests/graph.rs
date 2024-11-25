use crate::fiber::config::{DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT};
use crate::fiber::graph::{PathFindError, SessionRoute};
use crate::fiber::types::Pubkey;
use crate::now_timestamp_as_millis_u64;
use crate::{
    fiber::{
        graph::{ChannelInfo, NetworkGraph, NodeInfo, PathEdge},
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
        let store = Store::new(temp_path.path()).expect("create store failed");
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

    fn set_source(&mut self, source: PublicKey) {
        self.graph.set_source(source.into());
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
        other_fee_rate: Option<u128>,
    ) {
        let public_key1 = self.keys[node_a];
        let public_key2 = self.keys[node_b];
        let node_a_is_node1 = public_key1 < public_key2;
        let idx = self.edges.len() + 1;
        let channel_outpoint = OutPoint::from_slice(&[idx as u8; 36]).unwrap();
        self.edges.push((node_a, node_b, channel_outpoint.clone()));
        let (node_a_key, node_b_key) = if node_a_is_node1 {
            (public_key1, public_key2)
        } else {
            (public_key2, public_key1)
        };
        let channel_info = ChannelInfo {
            funding_tx_block_number: 0,
            funding_tx_index: 0,
            announcement_msg: ChannelAnnouncement {
                chain_hash: get_chain_hash(),
                node1_id: node_a_key.into(),
                node2_id: node_b_key.into(),
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
        self.graph.add_channel(channel_info.clone());
        let channel_update = ChannelUpdate {
            signature: None,
            chain_hash: get_chain_hash(),
            version: 0,
            message_flags: if node_a_is_node1 { 1 } else { 0 },
            channel_flags: 0,
            tlc_expiry_delta: 11,
            tlc_fee_proportional_millionths: fee_rate.unwrap_or(0),
            tlc_maximum_value: max_htlc_value.unwrap_or(10000),
            tlc_minimum_value: min_htlc_value.unwrap_or(0),
            channel_outpoint: channel_outpoint.clone(),
        };
        self.graph.process_channel_update(channel_update).unwrap();
        if let Some(fee_rate) = other_fee_rate {
            let channel_update = ChannelUpdate {
                signature: None,
                chain_hash: get_chain_hash(),
                version: 0,
                message_flags: if node_a_is_node1 { 0 } else { 1 },
                channel_flags: 0,
                tlc_expiry_delta: 22,
                tlc_fee_proportional_millionths: fee_rate,
                tlc_maximum_value: max_htlc_value.unwrap_or(10000),
                tlc_minimum_value: min_htlc_value.unwrap_or(0),
                channel_outpoint: channel_outpoint.clone(),
            };
            eprintln!("add rev channel_update: {:?}", channel_update);
            self.graph.process_channel_update(channel_update).unwrap();
        }
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
            None,
        );
    }

    pub fn find_path(
        &self,
        source: usize,
        target: usize,
        amount: u128,
        max_fee: u128,
    ) -> Result<Vec<PathEdge>, PathFindError> {
        let source = self.keys[source].into();
        let target = self.keys[target].into();
        self.graph.find_path(
            source,
            target,
            amount,
            Some(max_fee),
            None,
            DEFAULT_TLC_EXPIRY_DELTA,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
        )
    }

    pub fn find_path_udt(
        &self,
        source: usize,
        target: usize,
        amount: u128,
        max_fee: u128,
        udt_type_script: Script,
    ) -> Result<Vec<PathEdge>, PathFindError> {
        let source = self.keys[source].into();
        let target = self.keys[target].into();
        self.graph.find_path(
            source,
            target,
            amount,
            Some(max_fee),
            Some(udt_type_script),
            DEFAULT_TLC_EXPIRY_DELTA,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
        )
    }

    pub fn build_route_with_expect(&self, payment_data: &SendPaymentData, expect: Vec<usize>) {
        let route = self.graph.build_route(payment_data.clone());
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

    let route = network.find_path(1, 2, 100, 1000);
    assert!(route.is_err());

    network.add_edge(1, 2, Some(120), Some(2));
    let route = network.find_path(1, 2, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[0].channel_outpoint, network.edges[1].2);

    let route = network.find_path(1, 3, 10, 100);
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
    let route = network.find_path(1, 3, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[1].target, node3.into());
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);
    assert_eq!(route[1].channel_outpoint, network.edges[1].2);

    // Test route from node 1 to node 2
    let route = network.find_path(1, 2, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);

    // Test route from node 2 to node 3
    let route = network.find_path(2, 3, 100, 1000);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node3.into());
    assert_eq!(route[0].channel_outpoint, network.edges[1].2);

    // Test route from node 3 to node 1 (should fail)
    let route = network.find_path(3, 1, 100, 1000);
    assert!(route.is_err());
}

#[test]
fn test_graph_find_path_fee() {
    let mut network = MockNetworkGraph::new(5);

    network.add_edge(1, 2, Some(1000), Some(30000));
    network.add_edge(2, 4, Some(1000), Some(10000));

    network.add_edge(1, 3, Some(1000), Some(20000));
    network.add_edge(3, 4, Some(1000), Some(10000));

    let route = network.find_path(1, 4, 100, 1000);

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

    let route = network.find_path(1, 5, 100, 1000);

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

    let route = network.find_path(1, 3, 100, 1000);

    assert!(route.is_ok());

    network.add_edge(3, 4, Some(1000), Some(2));
    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_path(1, 5, 100, 1000);
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

    let route = network.find_path(1, 5, 100, 1000);
    assert!(route.is_ok());
}

#[test]
fn test_graph_find_path_loop_exit() {
    let mut network = MockNetworkGraph::new(6);

    // node2 and node3 are connected with each other, node1 is disconnected
    network.add_edge(2, 3, Some(1000), Some(3));
    network.add_edge(3, 2, Some(1000), Some(2));

    let route = network.find_path(1, 3, 100, 1000);
    assert!(route.is_err());

    // now add a path from node1 to node2, so that node1 can reach node3
    network.add_edge(1, 2, Some(1000), Some(4));
    let route = network.find_path(1, 3, 100, 1000);
    assert!(route.is_ok());
}

#[test]
fn test_graph_find_path_amount_failed() {
    let mut network = MockNetworkGraph::new(6);

    network.add_edge(1, 2, Some(1000), Some(4));
    network.add_edge(2, 3, Some(1000), Some(4));
    network.add_edge(3, 4, Some(1000), Some(4));
    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_path(1, 5, 1000, 10);
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

    // Check that the algorithm chose the longer path with lower fees
    let route = network.find_path(1, 5, 1000, 1000).unwrap();
    assert_eq!(route.len(), 4);
    for (i, edge_index) in (1..=4).enumerate() {
        assert_eq!(route[i].channel_outpoint, network.edges[edge_index].2);
    }

    // Test with a smaller amount that allows using the direct path
    let small_route = network.find_path(1, 5, 100, 100);
    assert!(small_route.is_ok());
    let small_route = small_route.unwrap();

    // Check that the algorithm chose the direct path for a smaller amount
    assert_eq!(small_route.len(), 2);
    assert_eq!(small_route[0].channel_outpoint, network.edges[5].2);
    assert_eq!(small_route[1].channel_outpoint, network.edges[6].2);
}

#[test]
fn test_graph_build_router_is_ok_with_fee_rate() {
    let mut network = MockNetworkGraph::new(6);

    // Direct path with high fee
    network.add_edge(1, 5, Some(2000), Some(50000));

    // Longer path with lower total fee
    network.add_edge(1, 2, Some(2000), Some(10000));
    // this node has a very low fee rate
    network.add_edge(2, 3, Some(2000), Some(1));
    network.add_edge(3, 4, Some(2000), Some(10000));
    network.add_edge(4, 5, Some(2000), Some(10000));

    // check the fee rate
    let source = network.keys[1];
    network.set_source(source);
    let node5 = network.keys[5];
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node5.into(),
        amount: 1000,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: None,
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());
    let route = route.unwrap();
    let amounts = route.iter().map(|x| x.amount).collect::<Vec<_>>();
    assert_eq!(amounts, vec![1022, 1011, 1010, 1000, 1000]);
}

#[test]
fn test_graph_build_router_fee_rate_optimize() {
    let mut network = MockNetworkGraph::new(10);

    // Direct path with low total fee rate
    network.add_edge(1, 6, Some(2000), Some(50000));
    network.add_edge(6, 5, Some(2000), Some(50000));

    // Longer path with lower total fee
    network.add_edge(1, 2, Some(2000), Some(10000));
    network.add_edge(2, 3, Some(2000), Some(20000));
    network.add_edge(3, 4, Some(2000), Some(30000));
    network.add_edge(4, 5, Some(2000), Some(40000));

    // check the fee rate
    let source = network.keys[1];
    network.set_source(source);
    let node5 = network.keys[5];
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node5.into(),
        amount: 1000,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: None,
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    eprintln!("route: {:?}", route);
    assert!(route.is_ok());
    let route = route.unwrap();
    let amounts = route.iter().map(|x| x.amount).collect::<Vec<_>>();
    assert_eq!(amounts, vec![1050, 1000, 1000]);
}

#[test]
fn test_graph_build_router_no_fee_with_direct_pay() {
    let mut network = MockNetworkGraph::new(10);

    network.add_edge(1, 5, Some(2000), Some(50000));

    // check the fee rate
    let source = network.keys[1];
    network.set_source(source);
    let node5 = network.keys[5];
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node5.into(),
        amount: 1000,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: None,
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());
    let route = route.unwrap();
    let amounts = route.iter().map(|x| x.amount).collect::<Vec<_>>();
    assert_eq!(amounts, vec![1000, 1000]);
}

#[test]
fn test_graph_find_path_err() {
    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];

    network.add_edge(1, 2, Some(1000), Some(4));
    let route = network.find_path(1, 1, 100, 1000);
    assert!(route.is_err());

    let no_exits_public_key = network.keys[0];
    let route = network.graph.find_path(
        node1.into(),
        no_exits_public_key.into(),
        100,
        Some(1000),
        None,
        DEFAULT_TLC_EXPIRY_DELTA,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
    );
    assert!(route.is_err());

    let route = network.graph.find_path(
        no_exits_public_key.into(),
        node1.into(),
        100,
        Some(1000),
        None,
        DEFAULT_TLC_EXPIRY_DELTA,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
    );
    assert!(route.is_err());
}

#[test]
fn test_graph_find_path_node_order() {
    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node2 = network.keys[2];
    let node3 = network.keys[3];

    network.add_edge(1, 2, Some(1000), Some(4));
    network.add_edge(2, 3, Some(1000), Some(4));

    let route = network.graph.find_path(
        node1.into(),
        node3.into(),
        100,
        Some(1000),
        None,
        DEFAULT_TLC_EXPIRY_DELTA,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
    );
    assert!(route.is_ok());
    // check the order of nodes in router is node1 -> node2 -> node3
    let route = route.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[1].target, node3.into());
}

#[test]
fn test_graph_build_route_with_expiry_limit() {
    let mut network = MockNetworkGraph::new(6);
    let (node1, node2) = (network.keys[1], network.keys[2]);

    network.add_edge(1, 2, Some(1000), Some(4));

    let route = network.graph.find_path(
        node1.into(),
        node2.into(),
        100,
        Some(1000),
        None,
        DEFAULT_TLC_EXPIRY_DELTA,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
    );
    assert!(route.is_ok());

    let route = network.graph.find_path(
        node1.into(),
        node2.into(),
        100,
        Some(1000),
        None,
        DEFAULT_TLC_EXPIRY_DELTA,
        100,
        false,
    );
    assert!(route.is_err());
}

#[test]
fn test_graph_build_route_three_nodes() {
    let mut network = MockNetworkGraph::new(3);
    network.add_edge(0, 2, Some(500), Some(200000));
    network.add_edge(2, 3, Some(500), Some(2));
    let node2 = network.keys[2];
    let node3 = network.keys[3];
    // Test build route from node1 to node3
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0].channel_outpoint, Some(network.edges[0].2.clone()));
    assert_eq!(route[1].channel_outpoint, Some(network.edges[1].2.clone()));

    assert_eq!(route[0].next_hop, Some(node2.into()));
    assert_eq!(route[1].next_hop, Some(node3.into()));
    assert_eq!(route[2].next_hop, None);

    assert_eq!(route[0].amount, 120);
    assert_eq!(route[1].amount, 100);
    assert_eq!(route[2].amount, 100);
}

#[test]
fn test_graph_build_route_exceed_max_htlc_value() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with max_htlc_value set to 50
    network.add_edge_with_config(0, 2, Some(500), Some(2), None, Some(50), None, None);
    network.add_edge_with_config(2, 3, Some(500), Some(2), None, Some(50), None, None);
    let node3 = network.keys[3];

    // Test build route from node1 to node3 with amount exceeding max_htlc_value
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100, // Exceeds max_htlc_value of 50
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_err());
}

#[test]
fn test_graph_build_route_below_min_htlc_value() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_htlc_value set to 50
    network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None, None);
    network.add_edge_with_config(2, 3, Some(500), Some(2), Some(50), None, None, None);
    let node3 = network.keys[3];

    // Test build route from node1 to node3 with amount below min_htlc_value
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 10, // Below min_htlc_value of 50
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_err());
}

#[test]
fn test_graph_find_path_udt() {
    let mut network = MockNetworkGraph::new(3);
    let udt_type_script = Script::default();
    network.add_edge_udt(1, 2, Some(1000), Some(1), udt_type_script.clone());
    let node2 = network.keys[2];

    let route = network.find_path_udt(1, 2, 100, 1000, udt_type_script.clone());
    assert!(route.is_ok());

    let route = route.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);

    let route = network.find_path(1, 3, 10, 100);
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
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_err());

    network.add_edge(0, 5, Some(500), Some(2));
    network.add_edge(5, 3, Some(500), Some(2));

    // Test build route from node1 to node3
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());
}

#[test]
fn test_graph_session_router() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 2, Some(500), Some(50000));
    network.add_edge(2, 3, Some(500), Some(20000));
    network.add_edge(3, 4, Some(500), Some(2));

    let node0 = network.keys[0];
    let node2 = network.keys[2];
    let node3 = network.keys[3];
    let node4 = network.keys[4];

    // Test build route from node1 to node4 should be Ok
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node4.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());

    let route = route.unwrap();
    let session_route = SessionRoute::new(node0.into(), node4.into(), &route);
    let fee = session_route.fee();
    assert_eq!(fee, 8);
    let session_route_keys: Vec<_> = session_route.nodes.iter().map(|x| x.pubkey).collect();
    assert_eq!(
        session_route_keys,
        vec![node0.into(), node2.into(), node3.into(), node4.into()]
    );
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
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());

    // Test build route from node1 to node4 should be Ok
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node4.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,

        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_ok());

    network.mark_node_failed(2);

    // Test build route from node1 to node3
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node3.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,

        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
    });
    assert!(route.is_err());

    // Test build route from node1 to node4
    let route = network.graph.build_route(SendPaymentData {
        target_pubkey: node4.into(),
        amount: 100,
        payment_hash: Hash256::default(),
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        dry_run: false,
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
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: false,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());

    let route = network.graph.build_route(payment_data.unwrap());
    assert!(route.is_err());
    let message = route.unwrap_err().to_string();
    assert!(message.contains("allow_self_payment is not enable, can not pay to self"));
}

#[test]
fn test_graph_payment_pay_single_path() {
    let mut network = MockNetworkGraph::new(9);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));
    network.add_edge(4, 5, Some(500), Some(2));
    network.add_edge(5, 6, Some(500), Some(2));

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[6].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command);
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
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());
    let payment_data = payment_data.unwrap();

    let route = network.graph.build_route(payment_data);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route[1].next_hop, Some(node0.into()));
}

#[test]
fn test_graph_build_route_with_double_edge_node() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_htlc_value set to 50
    // A <-> B, A is initiator, and A -> B with fee rate 5000, B -> A with fee rate 600000
    network.add_edge_with_config(
        0,
        2,
        Some(500),
        Some(5000),
        Some(50),
        None,
        None,
        Some(600000),
    );
    // A -> B, B is initiator, B -> A with fee rate 100000, A -> B with fee rate 200
    network.add_edge_with_config(
        2,
        0,
        Some(500),
        Some(100000),
        Some(50),
        None,
        None,
        Some(200),
    );

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command).unwrap();
    let route = network.graph.build_route(payment_data);
    assert!(route.is_ok());
}

#[test]
fn test_graph_build_route_with_other_node_maybe_better() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_htlc_value set to 50
    // A <-> B, A is initiator, and A -> B with fee rate 5000, B -> A with fee rate 600000
    network.add_edge_with_config(
        0,
        2,
        Some(500),
        Some(600000),
        Some(50),
        None,
        None,
        Some(600000),
    );
    // A -> B, B is initiator, B -> A with fee rate 100000, A -> B with fee rate 200
    network.add_edge_with_config(
        2,
        0,
        Some(500),
        Some(100000),
        Some(50),
        None,
        None,
        Some(600000),
    );
    // B <-> C, B is initiator
    network.add_edge_with_config(2, 3, Some(500), Some(2), Some(50), None, None, Some(1));

    let node0 = network.keys[0];
    let node1 = network.keys[2];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command).unwrap();
    let route = network.graph.build_route(payment_data);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route[0].next_hop, Some(node1.into()));
    assert_eq!(route[1].next_hop, Some(node0.into()));
}

#[test]
fn test_graph_payment_pay_self_will_ok() {
    let mut network = MockNetworkGraph::new(9);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));
    network.add_edge(4, 5, Some(500), Some(2));
    network.add_edge(5, 6, Some(500), Some(2));

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());
    let payment_data = payment_data.unwrap();

    let route = network.graph.build_route(payment_data.clone());
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

#[test]
fn test_graph_build_route_with_path_limits() {
    let mut network = MockNetworkGraph::new(100);
    // Add edges with min_htlc_value set to 50
    let mut fee_rate = 100000;
    for i in 0..99 {
        fee_rate -= 1000;
        network.add_edge_with_config(
            i,
            i + 1,
            Some(5000000),
            Some(fee_rate),
            Some(50),
            Some(10000000),
            None,
            Some(100),
        );
    }

    let node99 = network.keys[99];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(node99.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(10000000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command).unwrap();
    let route = network.graph.build_route(payment_data);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 100);
    assert_eq!(route[98].next_hop, Some(node99.into()));

    // make sure the fee is decreasing
    let mut fees = vec![];
    for i in 0..98 {
        fees.push(route[i].amount - route[i + 1].amount);
    }
    assert!(fees.windows(2).all(|x| x[0] >= x[1]));
}

#[test]
fn test_graph_build_route_with_path_limit_fail_with_fee_not_enough() {
    let mut network = MockNetworkGraph::new(100);
    // Add edges with min_htlc_value set to 50
    for i in 0..99 {
        network.add_edge_with_config(
            i,
            i + 1,
            Some(100), // the capacity can not provide the fee with long path
            Some(500),
            Some(50),
            None,
            None,
            Some(100),
        );
    }

    let node99 = network.keys[99];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(node99.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: true,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command).unwrap();
    let route = network.graph.build_route(payment_data);
    assert!(route.is_err());
}

#[test]
fn test_graph_payment_expiry_is_in_right_order() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 1, Some(500), Some(2));
    network.add_edge(1, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));

    let node3 = network.keys[3];

    let command = SendPaymentCommand {
        target_pubkey: Some(node3.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: Some(false),
        udt_type_script: None,
        allow_self_payment: false,
        dry_run: false,
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());

    let route = network.graph.build_route(payment_data.unwrap());
    assert!(route.is_ok());
    let route = route.unwrap();
    let expiries = route.iter().map(|e| e.expiry).collect::<Vec<_>>();
    // we set 11 as tlc expiry delta in the test
    assert_eq!(expiries.len(), 4);
    assert_eq!(expiries[0] - expiries[1], 11);
    assert_eq!(expiries[1] - expiries[2], 11);
    assert_eq!(expiries[3], 0);
    let expected_timestamp = now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA;
    assert!(expiries[2] <= expected_timestamp);
}
