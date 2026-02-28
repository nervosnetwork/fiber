#![allow(clippy::needless_range_loop)]
use crate::fiber::config::{DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT};
use crate::fiber::features::FeatureVector;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::PathFindError;
use crate::fiber::graph::SendPaymentState;
use crate::fiber::payment::SessionRoute;
use crate::fiber::payment::{SendPaymentData, SendPaymentDataBuilder};
use crate::fiber::types::{
    new_channel_update_unsigned, ChannelUpdateChannelFlags, ChannelUpdateMessageFlags, Pubkey,
    TrampolineOnionPacket,
};
use crate::{
    fiber::{
        graph::{NetworkGraph, RouterHop},
        network::get_chain_hash,
        payment::SendPaymentCommand,
        types::{new_node_announcement, ChannelAnnouncement, Hash256},
    },
    store::Store,
};
use ckb_types::{
    packed::{OutPoint, Script},
    prelude::Entity,
};
use secp256k1::{PublicKey, SecretKey, XOnlyPublicKey, SECP256K1};
use tracing::debug;

use crate::{
    gen_rand_secp256k1_keypair_tuple, generate_store, init_tracing, now_timestamp_as_millis_u64,
};

// Default tlc expiry delta used in this test environment.
// Should be a value larger than the running duration of the unit tests.
// The value below is 42 minutes.
const TLC_EXPIRY_DELTA_IN_TESTS: u64 = 42 * 60 * 1000;
// Default final tlc expiry delta used in this test environment.
const FINAL_TLC_EXPIRY_DELTA_IN_TESTS: u64 = 43 * 60 * 1000;

fn generate_key_pairs(num: usize) -> Vec<(SecretKey, PublicKey)> {
    let mut keys = vec![];
    for _ in 0..num {
        keys.push(gen_rand_secp256k1_keypair_tuple());
    }
    keys
}

struct MockNetworkGraph {
    pub keys: Vec<PublicKey>,
    pub secret_keys: Vec<SecretKey>,
    pub edges: Vec<(usize, usize, OutPoint)>,
    pub store: Store,
    pub graph: NetworkGraph<Store>,
}

impl MockNetworkGraph {
    pub fn new(node_num: usize) -> Self {
        let (store, _dir) = generate_store();
        let keypairs = generate_key_pairs(node_num + 1);
        let (secret_key1, public_key1) = keypairs[0];
        store.save_node_announcement(new_node_announcement(
            "node0".into(),
            FeatureVector::default(),
            vec![],
            &secret_key1.into(),
            now_timestamp_as_millis_u64(),
            0,
        ));
        for (i, keypair) in keypairs.iter().enumerate().skip(1) {
            let (sk, _pk) = keypair;
            store.save_node_announcement(new_node_announcement(
                format!("node{i}").as_str().into(),
                FeatureVector::default(),
                vec![],
                &(*sk).into(),
                now_timestamp_as_millis_u64(),
                0,
            ));
        }
        let mut graph = NetworkGraph::new(store.clone(), public_key1.into(), true);
        graph.always_process_gossip_message = true;

        Self {
            keys: keypairs.iter().map(|x| x.1).collect(),
            secret_keys: keypairs.iter().map(|x| x.0).collect(),
            edges: vec![],
            store,
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
            self.graph.mark_channel_failed(outpoint);
        }
    }

    // Add an directed edge from node_a to node_b with the given configuration.
    // The capacity is the capacity of the channel, the fee_rate is the fee rate
    // that node_b will charge when forwarding tlc for node_a. The min_tlc_value
    // is the minimum tlc value that node_b will accept when forwarding tlc for node_a.
    // The udt_type_script is the udt type script of the channel. The other_fee_rate
    // is the fee rate that node_a will charge when forwarding tlc for node_b.
    #[allow(clippy::too_many_arguments)]
    pub fn add_edge_with_config(
        &mut self,
        node_a: usize,
        node_b: usize,
        capacity: Option<u128>,
        fee_rate: Option<u128>,
        min_tlc_value: Option<u128>,
        udt_type_script: Option<Script>,
        other_fee_rate: Option<u128>,
        expiry_delta: Option<u64>,
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
        self.store.save_channel_announcement(
            now_timestamp_as_millis_u64(),
            ChannelAnnouncement {
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
        );
        self.store.save_channel_update(new_channel_update_unsigned(
            channel_outpoint.clone(),
            now_timestamp_as_millis_u64(),
            if node_a_is_node1 {
                ChannelUpdateMessageFlags::UPDATE_OF_NODE1
            } else {
                ChannelUpdateMessageFlags::UPDATE_OF_NODE2
            },
            ChannelUpdateChannelFlags::empty(),
            expiry_delta.unwrap_or(TLC_EXPIRY_DELTA_IN_TESTS),
            min_tlc_value.unwrap_or(0),
            fee_rate.unwrap_or(0),
        ));
        if let Some(fee_rate) = other_fee_rate {
            self.store.save_channel_update(new_channel_update_unsigned(
                channel_outpoint.clone(),
                now_timestamp_as_millis_u64(),
                if node_a_is_node1 {
                    ChannelUpdateMessageFlags::UPDATE_OF_NODE2
                } else {
                    ChannelUpdateMessageFlags::UPDATE_OF_NODE1
                },
                ChannelUpdateChannelFlags::empty(),
                22,
                min_tlc_value.unwrap_or(0),
                fee_rate,
            ));
        }
        self.graph.reload_from_store();
    }

    // Add an directed edge from node_a to node_b with the given capacity and fee rate.
    // The fee rate is the fee rate that node_b will charge when forwarding tlc for node_a.
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
            None,
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
            Some(udt_type_script),
            None,
            None,
        );
    }

    pub fn find_path(
        &self,
        source: usize,
        target: usize,
        amount: u128,
        max_fee: u128,
    ) -> Result<Vec<RouterHop>, PathFindError> {
        let source = self.keys[source].into();
        let target = self.keys[target].into();
        self.graph.find_path(
            source,
            target,
            Some(amount),
            Some(max_fee),
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
    }

    pub fn find_path_udt(
        &self,
        source: usize,
        target: usize,
        amount: u128,
        max_fee: u128,
        udt_type_script: Script,
    ) -> Result<Vec<RouterHop>, PathFindError> {
        let source = self.keys[source].into();
        let target = self.keys[target].into();
        self.graph.find_path(
            source,
            target,
            Some(amount),
            Some(max_fee),
            Some(udt_type_script),
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
    }

    pub fn build_route_with_expect(&self, payment_state: &SendPaymentState, expect: Vec<usize>) {
        let route = self
            .graph
            .build_route(payment_state.amount, None, None, payment_state);
        assert!(route.is_ok());
        let route = route.unwrap();
        let nodes = route.iter().filter_map(|x| x.next_hop).collect::<Vec<_>>();
        let expecptected_nodes: Vec<Pubkey> = expect
            .iter()
            .map(|x| self.keys[*x].into())
            .collect::<Vec<_>>();
        assert_eq!(nodes, expecptected_nodes);
    }

    pub fn build_route_with_possible_expects(
        &self,
        payment_state: &SendPaymentState,
        expects: &[Vec<usize>],
    ) {
        let route = self
            .graph
            .build_route(payment_state.amount, None, None, payment_state);
        assert!(route.is_ok());
        let route = route.unwrap();
        let nodes = route.iter().filter_map(|x| x.next_hop).collect::<Vec<_>>();
        let expecptected_nodes: Vec<Vec<Pubkey>> = expects
            .iter()
            .map(|x| x.iter().map(|i| self.keys[*i].into()).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert!(expecptected_nodes.contains(&nodes));
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_channel_info() {
    let mut mock_network = MockNetworkGraph::new(1);
    mock_network.add_edge(0, 1, Some(1000), Some(1));
    for i in 1..=mock_network.edges.len() {
        let channel_info = mock_network
            .graph
            .get_channel(&OutPoint::from_slice(&[i as u8; 36]).unwrap());
        assert!(channel_info.is_some());
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_graph_apis() {
    let mut mock_network = MockNetworkGraph::new(4);
    let node1 = mock_network.keys[1];
    let node2 = mock_network.keys[2];
    let node3 = mock_network.keys[3];
    assert!(mock_network.graph.get_node(&node1.into()).is_some());
    assert!(mock_network.graph.get_node(&node2.into()).is_some());

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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_find_path_fee() {
    let mut network = MockNetworkGraph::new(5);

    network.add_edge(1, 2, Some(1000), Some(30000));
    // means node 2 will charge fee_rate 10000 when forwarding tlc

    network.add_edge(2, 4, Some(1000), Some(10000));

    network.add_edge(1, 3, Some(1000), Some(30000));
    // means node 3 will charge fee_rate 30000 when forwarding tlc
    network.add_edge(3, 4, Some(1000), Some(30000));

    let route = network.find_path(1, 4, 100, 1000);

    assert!(route.is_ok());
    let route = route.unwrap();
    eprintln!("route: {:?}", route);

    // make sure we choose the path with lower fees
    assert_eq!(route.len(), 2);

    // assert we choose the second path
    assert_eq!(route[0].channel_outpoint, network.edges[0].2);
    assert_eq!(route[1].channel_outpoint, network.edges[1].2);

    // assert that we have the correct amount received
    assert_eq!(route[0].amount_received, 101);
    assert_eq!(route[1].amount_received, 100);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_find_path_expiry() {
    let mut network = MockNetworkGraph::new(5);

    network.add_edge(1, 2, Some(1000), Some(10000));
    // means node 2 will charge fee_rate 10000 when forwarding tlc
    network.add_edge(2, 3, Some(1000), Some(30000));

    let route = network.find_path(1, 3, 100, 1000);

    assert!(route.is_ok());
    let route = route.unwrap();
    eprintln!("route: {:?}", route);

    // make sure we choose the path with lower fees
    assert_eq!(route.len(), 2);
    // assert we choose the second path
    assert_eq!(
        route[0].incoming_tlc_expiry - route[1].incoming_tlc_expiry,
        TLC_EXPIRY_DELTA_IN_TESTS
    );
    assert_eq!(
        route[1].incoming_tlc_expiry,
        FINAL_TLC_EXPIRY_DELTA_IN_TESTS
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_find_path_amount_failed() {
    let mut network = MockNetworkGraph::new(6);

    network.add_edge(1, 2, Some(1000), Some(4));
    network.add_edge(2, 3, Some(1000), Some(4));
    network.add_edge(3, 4, Some(1000), Some(4));
    network.add_edge(4, 5, Some(1000), Some(1));

    let route = network.find_path(1, 5, 1000, 10);
    assert!(route.is_err());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_insufficient_outbound_liquidity_non_mpp() {
    // For a non-MPP payment the whole amount must flow through a single channel from
    // the source.  If every outbound channel from source has less capacity than the
    // requested amount, build_route should return InsufficientBalance early.
    let mut network = MockNetworkGraph::new(5);

    // Source node is 1.  It has one outbound channel with capacity 100.
    network.add_edge(1, 2, Some(100), Some(1));
    // There is also a longer path via node 3, but its first hop is still capacity 100.
    network.add_edge(1, 3, Some(100), Some(1));
    network.add_edge(3, 2, Some(1000), Some(1));

    let source = network.keys[1];
    let target = network.keys[2];
    network.set_source(source);

    // Trying to pay 500 should fail early because max outbound from source is 100.
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(target.into(), 500, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(10))
            .build()
            .expect("valid payment_data")
            .into();
    let result = network.graph.build_route(500, None, None, &payment_state);
    assert!(
        matches!(result, Err(PathFindError::InsufficientBalance(_))),
        "expected InsufficientBalance, got {:?}",
        result
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("max outbound liquidity"),
        "Expected 'max outbound liquidity' in: {err_msg}"
    );

    // Adding a channel from source with sufficient capacity makes the payment possible.
    network.add_edge(1, 4, Some(1000), Some(1));
    network.add_edge(4, 2, Some(1000), Some(1));
    let result = network.graph.build_route(500, None, None, &payment_state);
    assert!(result.is_ok(), "expected Ok, got {:?}", result);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_insufficient_outbound_liquidity_mpp() {
    // For an MPP payment the amount can be split across multiple channels, so the
    // relevant bound is the *total* outbound liquidity from source, not the max single
    // channel.  If the total is still insufficient, build_route should fail early.
    let mut network = MockNetworkGraph::new(4);

    // Source node is 1.  It has two channels, each with capacity 300.
    network.add_edge(1, 2, Some(300), Some(1));
    network.add_edge(1, 3, Some(300), Some(1));
    network.add_edge(2, 4, Some(1000), Some(1));
    network.add_edge(3, 4, Some(1000), Some(1));

    let source = network.keys[1];
    let target = network.keys[4];
    network.set_source(source);

    // Build MPP-enabled payment data (allow_mpp=true, max_parts=4).
    let mpp_payment = |amount: u128| -> SendPaymentState {
        SendPaymentDataBuilder::new(target.into(), amount, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .allow_mpp(true)
            .max_parts(Some(4))
            .max_fee_amount(Some(100))
            .build()
            .expect("valid payment_data")
            .into()
    };

    // Paying 700 (> single channel 300 but <= total 2×300=600) should fail early for non-MPP…
    let non_mpp: SendPaymentState =
        SendPaymentDataBuilder::new(target.into(), 700, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(100))
            .build()
            .expect("valid payment_data")
            .into();
    let result = network.graph.build_route(700, None, None, &non_mpp);
    assert!(
        matches!(result, Err(PathFindError::InsufficientBalance(_))),
        "non-MPP expected InsufficientBalance, got {:?}",
        result
    );

    // …but should be allowed to proceed with MPP (total 600 >= 500).
    // The early-fail check should NOT return InsufficientBalance here (it may
    // return NoPathFound for other reasons unrelated to the early-fail check).
    let ps = mpp_payment(500);
    let result = network.graph.build_route(500, Some(1), None, &ps);
    assert!(
        !matches!(result, Err(PathFindError::InsufficientBalance(_))),
        "MPP 500 must not be rejected by early-fail when total outbound (600) >= amount (500), got {:?}",
        result
    );

    // Paying 700 total with MPP also fails: total outbound is only 600.
    let ps = mpp_payment(700);
    let result = network.graph.build_route(700, Some(1), None, &ps);
    assert!(
        matches!(result, Err(PathFindError::InsufficientBalance(_))),
        "MPP 700 expected InsufficientBalance, got {:?}",
        result
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("total outbound liquidity"),
        "Expected 'total outbound liquidity' in: {err_msg}"
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_find_optimal_path() {
    let mut network = MockNetworkGraph::new(6);

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
    for (i, edge_index) in (0..=3).enumerate() {
        assert_eq!(route[i].channel_outpoint, network.edges[edge_index].2);
    }

    // Test with a smaller amount that allows using the direct path
    let small_route = network.find_path(1, 5, 100, 100);
    assert!(small_route.is_ok());
    let small_route = small_route.unwrap();

    // Check that the algorithm chose the direct path for a smaller amount
    assert_eq!(small_route.len(), 2);
    assert_eq!(small_route[0].channel_outpoint, network.edges[4].2);
    assert_eq!(small_route[1].channel_outpoint, network.edges[5].2);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_router_is_ok_with_fee_rate() {
    let mut network = MockNetworkGraph::new(6);

    // Direct path with high fee, but there are no middle hop nodes, so there will be no fee
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

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node5.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    let amounts = route.iter().map(|x| x.amount).collect::<Vec<_>>();
    assert_eq!(amounts, vec![1000, 1000]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_trampoline_routing_no_sender_precheck_to_final() {
    init_tracing();

    // Topology:
    //   sender(A)=node1  --(public channel)-->  trampoline(C)=node2
    //   final(D)=node3 is unreachable from A (and from the graph) on purpose.
    // Expectation: with explicit trampoline_hops, A only needs to find A->C,
    // and should NOT pre-check whether C can reach D.
    let mut network = MockNetworkGraph::new(3);

    let sender = network.keys[1];
    network.set_source(sender);
    let trampoline = network.keys[2];
    let final_recipient = network.keys[3];

    // Only A<->C exists. D is intentionally disconnected.
    network.add_edge(1, 2, Some(10_000), Some(0));

    let mut payment_data =
        SendPaymentDataBuilder::new(final_recipient.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(500))
            .build()
            .expect("valid payment_data");

    // Without trampoline: no route to final recipient.
    let payment_state: SendPaymentState = payment_data.clone().into();
    assert!(network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state,)
        .is_err());

    // With trampoline: should succeed by routing to C only.
    payment_data.trampoline_hops = Some(vec![trampoline.into()]);
    let payment_state: SendPaymentState = payment_data.clone().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state)
        .expect("trampoline route should be built");

    assert_eq!(route.len(), 2);
    assert_eq!(route[0].next_hop, Some(trampoline.into()));
    assert!(route[0].amount >= payment_state.amount);

    let last = route.last().expect("last hop");
    assert!(last.next_hop.is_none());
    let trampoline_bytes = last
        .trampoline_onion()
        .expect("trampoline payload should be present");
    // The payload is now an inner trampoline onion packet.
    assert!(
        TrampolineOnionPacket::new(trampoline_bytes)
            .into_sphinx_onion_packet()
            .is_ok(),
        "trampoline_onion should be a valid sphinx onion packet"
    );
    // The amount sent to the trampoline includes the remaining fee budget.
    // With 1000 amount and 500 max fee, we expect 1500.
    assert_eq!(last.amount, 1500);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node5.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();

    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    let amounts = route.iter().map(|x| x.amount).collect::<Vec<_>>();
    assert_eq!(amounts, vec![1050, 1000, 1000]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_trampoline_routing_trampoline_hops_specified() {
    init_tracing();

    // Topology:
    //   sender(A)=node1 --(public)--> t1=node2 --(public)--> t2=node3 --(public)--> t3=node4
    //   final=node5 is intentionally disconnected from the graph.
    // Expectation: build_route succeeds by routing to t1, and the inner trampoline onion encodes
    // the explicit chain t1->t2->t3->final.
    let mut network = MockNetworkGraph::new(5);

    // Make expiries deterministic for assertions.
    network.graph.set_add_rand_expiry_delta(false);

    let sender = network.keys[1];
    network.set_source(sender);
    let t1 = network.keys[2];
    let t2 = network.keys[3];
    let t3 = network.keys[4];
    let final_recipient = network.keys[5];

    network.add_edge(1, 2, Some(10_000), Some(0));
    network.add_edge(2, 3, Some(10_000), Some(0));
    network.add_edge(3, 4, Some(10_000), Some(0));

    let payment_hash = Hash256::default();
    let mut payment_data = SendPaymentDataBuilder::new(final_recipient.into(), 1000, payment_hash)
        .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .max_fee_amount(Some(500))
        .trampoline_hops(Some(vec![t1.into(), t2.into(), t3.into()]))
        .build()
        .expect("valid payment_data");

    let before = now_timestamp_as_millis_u64();
    let payment_state: SendPaymentState = payment_data.clone().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state)
        .expect("trampoline route should be built");
    let after = now_timestamp_as_millis_u64();

    // The expiry to the first trampoline must include slack for each trampoline hop.
    let expected_delta = FINAL_TLC_EXPIRY_DELTA_IN_TESTS
        + (payment_state
            .trampoline_hops
            .as_ref()
            .expect("trampoline_hops")
            .len() as u64)
            * DEFAULT_TLC_EXPIRY_DELTA;
    let last_expiry = route.last().expect("last hop").expiry;
    assert!(
        last_expiry >= before + expected_delta && last_expiry <= after + expected_delta,
        "last_expiry={last_expiry} expected_delta={expected_delta} now=[{before},{after}]"
    );

    // outer route should only reach t1
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].next_hop, Some(t1.into()));

    let last = route.last().expect("last hop");
    let trampoline_bytes = last
        .trampoline_onion()
        .expect("trampoline payload should be present");

    let assoc = Some(payment_hash.as_ref());

    // Peel inner trampoline onion hop-by-hop and validate the chain.
    let peeled1 = TrampolineOnionPacket::new(trampoline_bytes)
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[2]),
            assoc,
            SECP256K1,
        )
        .expect("peel t1");
    assert!(matches!(
        peeled1.current,
        crate::fiber::types::TrampolineHopPayload::Forward { next_node_id, tlc_expiry_limit, .. }
            if next_node_id == t2.into() && tlc_expiry_limit == payment_state.tlc_expiry_limit
    ));

    let peeled2 = peeled1
        .next
        .expect("next packet to t2")
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[3]),
            assoc,
            SECP256K1,
        )
        .expect("peel t2");
    assert!(matches!(
        peeled2.current,
        crate::fiber::types::TrampolineHopPayload::Forward { next_node_id, tlc_expiry_limit, .. }
            if next_node_id == t3.into() && tlc_expiry_limit == payment_state.tlc_expiry_limit
    ));

    let peeled3 = peeled2
        .next
        .expect("next packet to t3")
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[4]),
            assoc,
            SECP256K1,
        )
        .expect("peel t3");
    assert!(matches!(
        peeled3.current,
        crate::fiber::types::TrampolineHopPayload::Forward { next_node_id, tlc_expiry_limit, .. }
            if next_node_id == final_recipient.into() && tlc_expiry_limit == payment_state.tlc_expiry_limit
    ));

    let peeled_final = peeled3
        .next
        .expect("next packet to final")
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[5]),
            assoc,
            SECP256K1,
        )
        .expect("peel final");
    assert!(matches!(
        peeled_final.current,
        crate::fiber::types::TrampolineHopPayload::Final { .. }
    ));
    assert!(peeled_final.next.is_none());

    // sanity: shortening trampoline_hops should shorten the chain (t1->final).
    payment_data.trampoline_hops = Some(vec![t1.into()]);
    let payment_state: SendPaymentState = payment_data.clone().into();
    let route_short = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state)
        .expect("trampoline route should be built");
    let trampoline_short = route_short.last().unwrap().trampoline_onion().unwrap();
    let peeled_short_1 = TrampolineOnionPacket::new(trampoline_short)
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[2]),
            assoc,
            SECP256K1,
        )
        .expect("peel short t1");
    assert!(matches!(
        peeled_short_1.current,
        crate::fiber::types::TrampolineHopPayload::Forward { next_node_id, tlc_expiry_limit, .. }
            if next_node_id == final_recipient.into() && tlc_expiry_limit == payment_state.tlc_expiry_limit
    ));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_trampoline_routing_tlc_expiry_limit_too_small_fails() {
    init_tracing();

    // Topology: sender(A)=node1 --(public)--> t1=node2 --(public)--> t2=node3 --(public)--> t3=node4
    // final=node5 disconnected. We set tlc_expiry_limit too small for the required trampoline slack.
    let mut network = MockNetworkGraph::new(5);
    network.graph.set_add_rand_expiry_delta(false);

    let sender = network.keys[1];
    network.set_source(sender);
    let t1 = network.keys[2];
    let t2 = network.keys[3];
    let t3 = network.keys[4];
    let final_recipient = network.keys[5];

    network.add_edge(1, 2, Some(10_000), Some(0));
    network.add_edge(2, 3, Some(10_000), Some(0));
    network.add_edge(3, 4, Some(10_000), Some(0));

    // Required delta is FINAL + 3*DEFAULT (since we have 3 trampoline hops).
    // Set a limit that is smaller than that.
    let too_small_limit = FINAL_TLC_EXPIRY_DELTA_IN_TESTS + 2 * DEFAULT_TLC_EXPIRY_DELTA;

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(final_recipient.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(too_small_limit)
            .max_fee_amount(Some(500))
            .trampoline_hops(Some(vec![t1.into(), t2.into(), t3.into()]))
            .build()
            .expect("valid payment_data")
            .into();

    let err = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state)
        .expect_err("should fail due to tlc_expiry_limit too small for trampoline slack");
    let msg = err.to_string();
    assert!(
        msg.contains("trampoline tlc_expiry_delta exceeds tlc_expiry_limit"),
        "unexpected error: {msg}"
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_trampoline_routing_service_fee_budget_too_low_fails() {
    init_tracing();

    // Topology: sender(A)=node1 -> t1=node2 (public). final=node3 disconnected.
    // With an explicit trampoline hop fee_rate, if max_fee_amount is too low to cover
    // trampoline service fees, building the trampoline route should fail.
    let mut network = MockNetworkGraph::new(3);
    network.graph.set_add_rand_expiry_delta(false);

    let sender = network.keys[1];
    network.set_source(sender);
    let t1 = network.keys[2];
    let final_recipient = network.keys[3];

    network.add_edge(1, 2, Some(10_000), Some(0));

    let hop: Pubkey = t1.into();

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(final_recipient.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(0))
            .trampoline_hops(Some(vec![hop]))
            .build()
            .expect("valid payment_data")
            .into();

    let err = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state)
        .expect_err("should fail due to insufficient max_fee_amount for service fees");
    let msg = err.to_string();
    assert!(
        msg.contains("max_fee_amount is too low for trampoline routing"),
        "unexpected error: {msg}"
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_trampoline_routing_fee_rate_explicit_zero_allows_zero_fee_budget() {
    init_tracing();

    // If trampoline hop fee_rate is explicitly set to 0. With max_fee_amount=0,
    // building a trampoline route should still succeed (route only needs to reach the first trampoline).
    let mut network = MockNetworkGraph::new(3);
    network.graph.set_add_rand_expiry_delta(false);

    let sender = network.keys[1];
    network.set_source(sender);
    let t1 = network.keys[2];
    let final_recipient = network.keys[3];

    network.add_edge(1, 2, Some(10_000), Some(0));

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(final_recipient.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(0))
            .trampoline_hops(Some(vec![t1.into()]))
            .build()
            .expect("valid payment_data")
            .into();

    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    let err = route.unwrap_err().to_string();
    debug!("route err: {}", err);
    assert!(err.contains("max_fee_amount is too low for trampoline routing"));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_trampoline_routing_fee_fields_match_precompute() {
    init_tracing();

    // Topology:
    //   sender(A)=node1 --(public)--> t1=node2 --(public)--> t2=node3 --(public)--> t3=node4
    //   final=node5 disconnected.
    // We validate that the inner trampoline onion encodes:
    // - amount_to_forward ladder derived from per-hop fee_rate
    // - build_max_fee_amount derived from remaining fee budget allocation.
    let mut network = MockNetworkGraph::new(5);
    network.graph.set_add_rand_expiry_delta(false);

    let sender = network.keys[1];
    network.set_source(sender);
    let t1 = network.keys[2];
    let t2 = network.keys[3];
    let t3 = network.keys[4];
    let final_recipient = network.keys[5];

    network.add_edge(1, 2, Some(10_000), Some(0));
    network.add_edge(2, 3, Some(10_000), Some(0));
    network.add_edge(3, 4, Some(10_000), Some(0));

    let payment_hash = Hash256::default();
    let final_amount: u128 = 1000;
    let max_fee_amount: u128 = 15;

    let h1: Pubkey = t1.into();
    let h2: Pubkey = t2.into();
    let h3: Pubkey = t3.into();

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(final_recipient.into(), final_amount, payment_hash)
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(max_fee_amount))
            .trampoline_hops(Some(vec![h1, h2, h3]))
            .build()
            .expect("valid payment_data")
            .into();

    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state)
        .expect("trampoline route should be built");
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].next_hop, Some(t1.into()));

    // Compute expected values following `NetworkGraph::find_trampoline_route` logic.
    let hops = [h1, h2, h3];
    let mut fees = vec![0u128; hops.len()];
    let remaining_fee = max_fee_amount;
    let slots = fees.len() as u128;
    let base = remaining_fee / slots;
    let remainder = (remaining_fee % slots) as usize;
    fees.iter_mut().for_each(|b| *b = base);
    for fee in fees.iter_mut().take(remainder) {
        *fee = fee.saturating_add(1);
    }

    let mut forward_amounts = vec![0u128; hops.len()];
    for idx in 0..hops.len() {
        forward_amounts[idx] = final_amount + fees[(idx + 1)..].iter().sum::<u128>();
    }

    let mut exp_build_max_fee_amounts = vec![0u128; hops.len()];
    for idx in 0..hops.len() {
        exp_build_max_fee_amounts[idx] = fees.get(idx).copied().unwrap_or(0);
    }

    let assoc = Some(payment_hash.as_ref());
    let trampoline_bytes = route.last().unwrap().trampoline_onion().unwrap();

    let peeled1 = TrampolineOnionPacket::new(trampoline_bytes)
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[2]),
            assoc,
            SECP256K1,
        )
        .expect("peel t1");
    match peeled1.current {
        crate::fiber::types::TrampolineHopPayload::Forward {
            next_node_id,
            amount_to_forward,
            build_max_fee_amount,
            ..
        } => {
            assert_eq!(next_node_id, t2.into());
            assert_eq!(amount_to_forward, forward_amounts[0]);
            assert_eq!(build_max_fee_amount, exp_build_max_fee_amounts[0]);
        }
        other => panic!("unexpected payload at t1: {other:?}"),
    }

    let peeled2 = peeled1
        .next
        .unwrap()
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[3]),
            assoc,
            SECP256K1,
        )
        .expect("peel t2");
    match peeled2.current {
        crate::fiber::types::TrampolineHopPayload::Forward {
            next_node_id,
            amount_to_forward,
            build_max_fee_amount,
            ..
        } => {
            assert_eq!(next_node_id, t3.into());
            assert_eq!(amount_to_forward, forward_amounts[1]);
            assert_eq!(build_max_fee_amount, exp_build_max_fee_amounts[1]);
        }
        other => panic!("unexpected payload at t2: {other:?}"),
    }

    let peeled3 = peeled2
        .next
        .unwrap()
        .peel(
            &crate::fiber::types::Privkey(network.secret_keys[4]),
            assoc,
            SECP256K1,
        )
        .expect("peel t3");
    match peeled3.current {
        crate::fiber::types::TrampolineHopPayload::Forward {
            next_node_id,
            amount_to_forward,
            build_max_fee_amount,
            ..
        } => {
            assert_eq!(next_node_id, final_recipient.into());
            assert_eq!(amount_to_forward, forward_amounts[2]);
            assert_eq!(build_max_fee_amount, exp_build_max_fee_amounts[2]);
        }
        other => panic!("unexpected payload at t3: {other:?}"),
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_router_no_fee_with_direct_pay() {
    let mut network = MockNetworkGraph::new(10);

    network.add_edge(1, 5, Some(2000), Some(50000));

    // check the fee rate
    let source = network.keys[1];
    network.set_source(source);
    let node5 = network.keys[5];
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node5.into(), 1000, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    let amounts = route.iter().map(|x| x.amount).collect::<Vec<_>>();
    assert_eq!(amounts, vec![1000, 1000]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        Some(100),
        Some(1000),
        None,
        FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
        &[],
        &Default::default(),
        true,
    );
    assert!(route.is_err());

    let route = network.graph.find_path(
        no_exits_public_key.into(),
        node1.into(),
        Some(100),
        Some(1000),
        None,
        FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
        &[],
        &Default::default(),
        true,
    );
    assert!(route.is_err());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        Some(100),
        Some(1000),
        None,
        FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
        &[],
        &Default::default(),
        true,
    );
    assert!(route.is_ok());
    // check the order of nodes in router is node1 -> node2 -> node3
    let route = route.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0].target, node2.into());
    assert_eq!(route[1].target, node3.into());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_with_expiry_limit() {
    let mut network = MockNetworkGraph::new(6);
    let (node1, node2) = (network.keys[1], network.keys[2]);

    network.add_edge(1, 2, Some(1000), Some(4));

    let route = network.graph.find_path(
        node1.into(),
        node2.into(),
        Some(100),
        Some(1000),
        None,
        FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
        MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        false,
        &[],
        &Default::default(),
        true,
    );
    assert!(route.is_ok());

    let route = network.graph.find_path(
        node1.into(),
        node2.into(),
        Some(100),
        Some(1000),
        None,
        FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
        100,
        false,
        &[],
        &Default::default(),
        true,
    );
    assert!(route.is_err());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_three_nodes_amount() {
    let mut network = MockNetworkGraph::new(3);
    network.add_edge(0, 2, Some(500), Some(200000));
    network.add_edge(2, 3, Some(500), Some(20000));
    let node2 = network.keys[2];
    let node3 = network.keys[3];
    // Test build route from node1 to node3
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node3.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(
        route[0].funding_tx_hash,
        network.edges[0].2.tx_hash().into()
    );
    assert_eq!(
        route[1].funding_tx_hash,
        network.edges[1].2.tx_hash().into()
    );

    assert_eq!(route[0].next_hop, Some(node2.into()));
    assert_eq!(route[1].next_hop, Some(node3.into()));
    assert_eq!(route[2].next_hop, None);

    assert_eq!(route[0].amount, 102);
    assert_eq!(route[1].amount, 100);
    assert_eq!(route[2].amount, 100);
}

// TODO: pass randomized input to this function.
fn do_test_graph_build_route_expiry(n_nodes: usize) {
    let mut network = MockNetworkGraph::new(n_nodes);
    let ns = (0..n_nodes).collect::<Vec<_>>();
    for window in ns.windows(2) {
        let source = window[0];
        let target = window[1];
        network.add_edge(source, target, Some(500000), Some(0));
    }
    let last_node = network.keys[n_nodes - 1];
    let timestamp_before_building_route = now_timestamp_as_millis_u64();
    // Send a payment from the first node to the last node

    network.graph.set_add_rand_expiry_delta(false);
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(last_node.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    let timestamp_after_building_route = now_timestamp_as_millis_u64();
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), n_nodes);
    for i in 0..(n_nodes - 1) {
        assert_eq!(
            route[i].funding_tx_hash,
            network.edges[i].2.tx_hash().into()
        );
        assert_eq!(route[i].next_hop, Some(network.keys[i + 1].into()));
    }

    assert_eq!(route[n_nodes - 1].next_hop, None);
    assert_eq!(route[n_nodes - 1].funding_tx_hash, Default::default());

    for i in 0..n_nodes - 1 {
        assert!(
            route[i].expiry
                <= timestamp_after_building_route
                    + TLC_EXPIRY_DELTA_IN_TESTS * ((n_nodes - i - 2) as u64)
                    + FINAL_TLC_EXPIRY_DELTA_IN_TESTS
        );
        assert!(
            route[i].expiry
                >= timestamp_before_building_route
                    + TLC_EXPIRY_DELTA_IN_TESTS * ((n_nodes - i - 2) as u64)
                    + FINAL_TLC_EXPIRY_DELTA_IN_TESTS
        );
    }
    assert_eq!(route[n_nodes - 1].expiry, route[n_nodes - 2].expiry);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_2_nodes_expiry() {
    do_test_graph_build_route_expiry(2);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_3_nodes_expiry() {
    do_test_graph_build_route_expiry(3);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_4_nodes_expiry() {
    do_test_graph_build_route_expiry(4);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_99_nodes_expiry() {
    do_test_graph_build_route_expiry(99);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_below_min_tlc_value() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_tlc_value set to 50
    network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None, None);
    network.add_edge_with_config(2, 3, Some(500), Some(2), Some(50), None, None, None);
    let node3 = network.keys[3];

    // Test build route from node1 to node3 with amount below min_tlc_value
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node3.into(), 10, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_err());
}

#[test]
fn test_graph_build_route_select_edge_with_latest_timestamp() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_tlc_value set to 50
    network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None, None);
    // sleep 100 ms
    std::thread::sleep(crate::time::Duration::from_millis(100));
    network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None, None);
    let node2 = network.keys[2];

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node2.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    eprintln!("got route {:?}", route);
    let route = route.unwrap();
    assert_eq!(route.len(), 2);
    // assert we use the second added edge
    assert_eq!(
        route[0].funding_tx_hash,
        network.edges[1].2.tx_hash().into()
    );
}

#[test]
fn test_graph_build_route_select_edge_with_large_capacity() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_tlc_value set to 50
    network.add_edge_with_config(0, 2, Some(501), Some(2), Some(50), None, None, None);
    // sleep 100 ms
    std::thread::sleep(std::time::Duration::from_millis(100));
    network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None, None);
    let node2 = network.keys[2];

    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node2.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 2);
    // assert we use the first edge with large capacity
    assert_eq!(
        route[0].funding_tx_hash,
        network.edges[0].2.tx_hash().into()
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_mark_failed_channel() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    let node3 = network.keys[3];

    network.mark_channel_failed(2, 3);
    // Test build route from node1 to node3
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node3.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_err());

    network.add_edge(0, 5, Some(500), Some(2));
    network.add_edge(5, 3, Some(500), Some(2));

    // Test build route from node1 to node3
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node3.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node4.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());

    let route = route.unwrap();
    let session_route = SessionRoute::new(node0.into(), node4.into(), &route);
    let fee = session_route.fee();
    // round_up(101 * 2000 / 1000000) = 3, so the total amount = 101 + 3 = 104
    assert_eq!(route[0].amount, 104);
    assert_eq!(route[1].amount, 101);
    assert_eq!(route[2].amount, 100);
    assert_eq!(route[3].amount, 100);
    assert_eq!(fee, 4);
    let session_route_keys: Vec<_> = session_route.nodes.iter().map(|x| x.pubkey).collect();
    assert_eq!(
        session_route_keys,
        vec![node0.into(), node2.into(), node3.into(), node4.into()]
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_mark_failed_node() {
    let mut network = MockNetworkGraph::new(5);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 3, Some(500), Some(2));
    network.add_edge(2, 4, Some(500), Some(2));

    let node3 = network.keys[3];
    let node4 = network.keys[4];

    // Test build route from node1 to node3
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node3.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());

    // Test build route from node1 to node4 should be Ok
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node4.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());

    network.mark_node_failed(2);

    // Test build route from node1 to node3
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node3.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_err());

    // Test build route from node1 to node4
    let payment_state: SendPaymentState =
        SendPaymentDataBuilder::new(node4.into(), 100, Hash256::default())
            .final_tlc_expiry_delta(FINAL_TLC_EXPIRY_DELTA_IN_TESTS)
            .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
            .timeout(Some(10))
            .max_fee_amount(Some(1000))
            .build()
            .expect("valid payment_data")
            .into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_err());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());

    let payment_state: SendPaymentState = payment_data.unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_err());
    let message = route.unwrap_err().to_string();
    assert!(message.contains("allow_self_payment is not enabled, can not pay to self"));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());
    let payment_state: SendPaymentState = payment_data.unwrap().into();

    network.add_edge(4, 0, Some(1000), Some(2));
    network.build_route_with_expect(&payment_state, vec![2, 4, 5, 6]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());
    let payment_state: SendPaymentState = payment_data.unwrap().into();

    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route[1].next_hop, Some(node0.into()));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_payment_pay_self_with_one_node_fee_rate() {
    let mut network = MockNetworkGraph::new(9);
    network.add_edge(0, 2, Some(500), Some(2));
    network.add_edge(2, 0, Some(500), Some(200000));

    let node0 = network.keys[0];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());
    let payment_state: SendPaymentState = payment_data.unwrap().into();

    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0].amount, 120);
    assert_eq!(route[1].amount, 100);
    assert_eq!(route[2].amount, 100);
    assert_eq!(route[1].next_hop, Some(node0.into()));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_with_double_edge_node() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_tlc_value set to 50
    // A <-> B, A is initiator, and A -> B with fee rate 5000, B -> A with fee rate 600000
    network.add_edge_with_config(
        0,
        2,
        Some(500),
        Some(5000),
        Some(50),
        None,
        Some(600000),
        None,
    );
    // A -> B, B is initiator, B -> A with fee rate 100000, A -> B with fee rate 200
    network.add_edge_with_config(
        2,
        0,
        Some(500),
        Some(100000),
        Some(50),
        None,
        Some(200),
        None,
    );

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_state: SendPaymentState = SendPaymentData::new(command).unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_with_other_node_maybe_better() {
    let mut network = MockNetworkGraph::new(3);
    // Add edges with min_tlc_value set to 50
    // A <-> B, A is initiator, and A -> B with fee rate 5000, B -> A with fee rate 600000
    network.add_edge_with_config(
        0,
        2,
        Some(500),
        Some(600000),
        Some(50),
        None,
        Some(600000),
        None,
    );
    // A -> B, B is initiator, B -> A with fee rate 100000, A -> B with fee rate 200
    network.add_edge_with_config(
        2,
        0,
        Some(500),
        Some(100000),
        Some(50),
        None,
        Some(600000),
        None,
    );
    // B <-> C, B is initiator
    network.add_edge_with_config(2, 3, Some(500), Some(2), Some(50), None, Some(1), None);

    let node0 = network.keys[0];
    let node1 = network.keys[2];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(network.keys[0].into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_state: SendPaymentState = SendPaymentData::new(command).unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    assert_eq!(route[0].next_hop, Some(node1.into()));
    assert_eq!(route[1].next_hop, Some(node0.into()));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());
    let payment_state: SendPaymentState = payment_data.unwrap().into();

    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    // since we don't have a route to node0, it will fail
    assert!(route.is_err());

    // add a long path
    let mut possible_expects = vec![];
    network.add_edge(6, 0, Some(500), Some(2));
    possible_expects.push(vec![2, 4, 5, 6, 0]);
    network.build_route_with_possible_expects(&payment_state, &possible_expects);

    // now add another shorter path
    network.add_edge(4, 0, Some(1000), Some(2));
    possible_expects.push(vec![2, 4, 0]);
    network.build_route_with_possible_expects(&payment_state, &possible_expects);

    // now add another shorter path
    network.add_edge(2, 0, Some(1000), Some(2));
    possible_expects.push(vec![2, 0]);
    network.build_route_with_possible_expects(&payment_state, &possible_expects);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_with_path_limits() {
    let mut network = MockNetworkGraph::new(100);
    // Add edges with min_tlc_value set to 50
    let mut fee_rate = 100000;
    for i in 0..99 {
        fee_rate -= 1000;
        network.add_edge_with_config(
            i,
            i + 1,
            Some(5000000),
            Some(fee_rate),
            Some(50),
            None,
            Some(100),
            None,
        );
    }

    let node99 = network.keys[99];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(node99.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        timeout: Some(10),
        max_fee_amount: Some(10000000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_state: SendPaymentState = SendPaymentData::new(command).unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_build_route_with_path_limit_fail_with_fee_not_enough() {
    let mut network = MockNetworkGraph::new(100);
    // Add edges with min_tlc_value set to 50
    for i in 0..99 {
        network.add_edge_with_config(
            i,
            i + 1,
            Some(100), // the capacity can not provide the fee with long path
            Some(500),
            Some(50),
            None,
            Some(100),
            None,
        );
    }

    let node99 = network.keys[99];

    // node0 is the source node
    let command = SendPaymentCommand {
        target_pubkey: Some(node99.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        allow_self_payment: true,
        ..Default::default()
    };
    let payment_state: SendPaymentState = SendPaymentData::new(command).unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_err());
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
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
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());

    let current_time = now_timestamp_as_millis_u64();
    let payment_state: SendPaymentState = payment_data.unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    let expiries = route.iter().map(|e| e.expiry).collect::<Vec<_>>();
    assert_eq!(expiries.len(), 4);
    assert_eq!(expiries[0] - expiries[1], TLC_EXPIRY_DELTA_IN_TESTS);
    assert_eq!(expiries[1] - expiries[2], TLC_EXPIRY_DELTA_IN_TESTS);
    assert_eq!(expiries[2], expiries[3]);
    assert!(expiries[3] >= current_time + FINAL_TLC_EXPIRY_DELTA_IN_TESTS);

    let final_tlc_expiry_delta = 987654;
    let command = SendPaymentCommand {
        target_pubkey: Some(node3.into()),
        amount: Some(100),
        payment_hash: Some(Hash256::default()),
        final_tlc_expiry_delta: Some(final_tlc_expiry_delta),
        timeout: Some(10),
        max_fee_amount: Some(1000),
        keysend: Some(false),
        ..Default::default()
    };
    let payment_data = SendPaymentData::new(command);
    assert!(payment_data.is_ok());

    let current_time = now_timestamp_as_millis_u64();
    let payment_state: SendPaymentState = payment_data.unwrap().into();
    let route = network
        .graph
        .build_route(payment_state.amount, None, None, &payment_state);
    assert!(route.is_ok());
    let route = route.unwrap();
    let expiries = route.iter().map(|e| e.expiry).collect::<Vec<_>>();
    assert_eq!(expiries.len(), 4);
    assert!(expiries[3] >= current_time + final_tlc_expiry_delta);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_find_path_source_with_multiple_edges_fee_rate() {
    init_tracing();

    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node3 = network.keys[3];

    network.add_edge(1, 2, Some(1000), Some(1000));
    network.add_edge(1, 2, Some(1001), Some(1000));
    network.add_edge(1, 2, Some(1002), Some(1000000));
    network.add_edge(2, 3, Some(1000), Some(1000));

    let route = network
        .graph
        .find_path(
            node1.into(),
            node3.into(),
            Some(100),
            Some(1000),
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
        .unwrap();

    eprintln!("router: {:?}", route);
    // source node will not charge fee, even the edges[2] fee rate is high
    assert_eq!(route[0].channel_outpoint, network.edges[2].2);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_graph_find_path_source_with_multiple_edges_with_different_fee_rate() {
    init_tracing();

    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node3 = network.keys[3];

    network.add_edge_with_config(1, 2, Some(1000), Some(1000), None, None, None, Some(1000));
    network.add_edge_with_config(1, 2, Some(1001), Some(1000), None, None, None, Some(1000));
    network.add_edge_with_config(
        1,
        2,
        Some(1002),
        Some(1005), // high fee rate
        None,
        None,
        None,
        Some(1000),
    );
    network.add_edge(2, 3, Some(1000), Some(1000));

    let route = network
        .graph
        .find_path(
            node1.into(),
            node3.into(),
            Some(100),
            Some(1000),
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
        .unwrap();

    eprintln!("router: {:?}", route);
    // source node will not charge fee, even the edges[2] fee rate is high
    assert_eq!(route[0].channel_outpoint, network.edges[2].2);
}

#[test]
fn test_graph_find_path_source_with_multiple_edges_with_invalid_tlc_delta() {
    init_tracing();

    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node3 = network.keys[3];

    network.add_edge_with_config(1, 2, Some(1000), Some(1000), None, None, None, Some(1000));
    network.add_edge_with_config(1, 2, Some(1001), Some(1000), None, None, None, Some(1000));
    network.add_edge_with_config(
        1,
        2,
        Some(1002),
        Some(1000),
        None,
        None,
        None,
        Some(10000000000), // invalid tlc expiry delta,
                           // should not be used in path finding
    );
    network.add_edge(2, 3, Some(1000), Some(1000));

    let route = network
        .graph
        .find_path(
            node1.into(),
            node3.into(),
            Some(100),
            Some(1000),
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
        .unwrap();

    eprintln!("router: {:?}", route);
    // we will not consider the edge with invalid tlc expiry delta
    assert_eq!(route[0].channel_outpoint, network.edges[1].2);
}

#[test]
fn test_graph_find_path_will_consider_tlc_expiry_delta() {
    init_tracing();

    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node3 = network.keys[3];

    network.add_edge_with_config(1, 2, Some(100100), Some(1000), None, None, None, Some(1000));
    network.add_edge_with_config(2, 3, Some(100200), Some(1000), None, None, None, Some(1000));
    network.add_edge_with_config(
        2,
        3,
        Some(100500), // most large amount
        Some(1000),
        None,
        None,
        None,
        Some(1000000000), // large tlc expiry delta
    );
    network.add_edge_with_config(
        2,
        3,
        Some(100200),
        Some(1000),
        None,
        None,
        None,
        Some(100000), // second large tlc expiry delta
    );

    let route = network
        .graph
        .find_path(
            node1.into(),
            node3.into(),
            Some(100000),
            Some(1000),
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
        .unwrap();

    eprintln!("router: {:?}", route);
    // source node will not charge fee, even the edges[2] fee rate is high
    assert_eq!(route[1].channel_outpoint, network.edges[1].2);
}

#[test]
fn test_graph_find_path_max_capacity() {
    init_tracing();

    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node4 = network.keys[4];

    // Path 1: 1 -> 2 -> 4, capacity 100
    network.add_edge(1, 2, Some(100), Some(0));
    network.add_edge(2, 4, Some(100), Some(0));

    // Path 2: 1 -> 3 -> 4, capacity 500
    network.add_edge(1, 3, Some(500), Some(0));
    network.add_edge(3, 4, Some(500), Some(0));

    let route = network
        .graph
        .find_path(
            node1.into(),
            node4.into(),
            None, // Find max capacity path
            None,
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
        .unwrap();

    eprintln!("router: {:?}", route);
    assert_eq!(route.len(), 2);
    // Should choose path via node 3 (index 1 in route, since route is reversed? No, route is source to target)
    // Route[0] is 1->3, Route[1] is 3->4.
    // We expect edges[2] and edges[3]
    assert_eq!(route[0].channel_outpoint, network.edges[2].2);
    assert_eq!(route[1].channel_outpoint, network.edges[3].2);

    // Check amount_received (capacity)
    // The bottleneck capacity is 500.
    assert_eq!(route[0].amount_received, 500);
    assert_eq!(route[1].amount_received, 500);
}

#[test]
fn test_graph_find_path_max_capacity_with_fee_rate() {
    init_tracing();

    let mut network = MockNetworkGraph::new(6);
    let node1 = network.keys[1];
    let node4 = network.keys[4];

    // Path 1: 1 -> 2 -> 4, capacity 100
    network.add_edge(1, 2, Some(100), Some(10));
    network.add_edge(2, 4, Some(100), Some(10));

    // Path 2: 1 -> 3 -> 4, capacity 500
    network.add_edge(1, 3, Some(500), Some(10));
    network.add_edge(3, 4, Some(500), Some(10));

    let route = network
        .graph
        .find_path(
            node1.into(),
            node4.into(),
            None, // Find max capacity path
            None,
            None,
            FINAL_TLC_EXPIRY_DELTA_IN_TESTS,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &Default::default(),
            true,
        )
        .unwrap();

    eprintln!("router: {:?}", route);
    assert_eq!(route.len(), 2);
    // Should choose path via node 3 (index 1 in route, since route is reversed? No, route is source to target)
    // Route[0] is 1->3, Route[1] is 3->4.
    // We expect edges[2] and edges[3]
    assert_eq!(route[0].channel_outpoint, network.edges[2].2);
    assert_eq!(route[1].channel_outpoint, network.edges[3].2);

    // Check amount_received (capacity)
    // Now we don't consider fee rate for max_capacity_search
    assert_eq!(route[0].amount_received, 500);
    assert_eq!(route[1].amount_received, 500);
}
