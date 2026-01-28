#![cfg(feature = "bench")]
use ckb_types::packed::OutPoint;
use ckb_types::prelude::Entity;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fnn::fiber::config::{
    DEFAULT_FINAL_TLC_EXPIRY_DELTA, DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT,
};
use fnn::fiber::features::FeatureVector;
use fnn::fiber::gossip::GossipMessageStore;
use fnn::fiber::graph::{GraphChannelStat, NetworkGraph};
use fnn::fiber::network::get_chain_hash;
use fnn::fiber::types::{
    ChannelAnnouncement, ChannelUpdate, ChannelUpdateChannelFlags, ChannelUpdateMessageFlags,
    NodeAnnouncement, Pubkey,
};
use fnn::store::Store;
use fnn::tests::create_n_nodes_network;
use fnn::MIN_RESERVED_CKB;
use fnn::{gen_rand_secp256k1_keypair_tuple, generate_store, now_timestamp_as_millis_u64};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use secp256k1::{PublicKey, XOnlyPublicKey};
use std::collections::HashSet;
use std::time::Instant;
use tokio::runtime::Runtime;

fn bench_payment_path_finding(c: &mut Criterion) {
    // Create a runtime for async functions
    let rt = Runtime::new().unwrap();

    // Create a benchmark group with minimal/no warmup
    let mut group = c.benchmark_group("payment_path_finding");

    for num_channels in [1, 4, 8] {
        // Add throughput measurement based on the number of paths
        group.throughput(Throughput::Elements(num_channels as u64));

        group.bench_function(BenchmarkId::new("find_path", num_channels), |b| {
            b.iter_custom(|iters| {
                rt.block_on(async move {
                    // Setup the network with varying number of channels between node 1 and node 2
                    let mut channel_configs = Vec::new();

                    for _ in 0..num_channels {
                        channel_configs
                            .push(((0, 1), (13900000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)));
                        channel_configs
                            .push(((1, 2), (14000000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)));
                    }
                    channel_configs
                        .push(((2, 1), (14100000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)));

                    // Create the network
                    let (nodes, _channels) = create_n_nodes_network(&channel_configs, 3).await;

                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    // Start timing
                    let start = std::time::Instant::now();

                    // Run the benchmark iterations
                    for _ in 0..iters {
                        let mut payments = HashSet::new();

                        for _i in 0..5 {
                            let payment_amount = 10;
                            let res = nodes[0]
                                .send_payment_keysend(&nodes[2], payment_amount, false)
                                .await
                                .unwrap();

                            let payment_hash = res.payment_hash;
                            payments.insert(payment_hash);
                        }

                        loop {
                            for payment_hash in &payments.clone() {
                                nodes[0].wait_until_success(*payment_hash).await;
                                payments.remove(payment_hash);
                            }
                            if payments.is_empty() {
                                break;
                            }
                        }
                    }

                    // Return elapsed time
                    start.elapsed()
                })
            });
        });
    }

    group.finish();
}

fn build_graph_for_find_path_bench(
    node_count: usize,
    channel_count: usize,
) -> (NetworkGraph<Store>, Vec<PublicKey>) {
    let (store, _dir) = generate_store();
    let mut keypairs = vec![];
    for _ in 0..node_count {
        keypairs.push(gen_rand_secp256k1_keypair_tuple());
    }

    for (i, (sk, _pk)) in keypairs.iter().enumerate() {
        store.save_node_announcement(NodeAnnouncement::new(
            format!("node{i}").as_str().into(),
            FeatureVector::default(),
            vec![],
            &(*sk).into(),
            now_timestamp_as_millis_u64(),
            0,
        ));
    }

    let mut edges: Vec<(usize, usize, u128)> = Vec::with_capacity(channel_count);
    let mut used_pairs: HashSet<(usize, usize)> = HashSet::with_capacity(channel_count * 2);
    let mut rng = StdRng::seed_from_u64(42);

    // Build a layered network topology to force path finding to traverse many nodes
    // BUT FIRST: Create a guaranteed linear path 0 -> 1 -> 2 -> ... -> (node_count-1)
    // to ensure connectivity
    for i in 0..(node_count - 1) {
        let (low, high) = (i, i + 1);
        used_pairs.insert((low, high));
        // Use high capacity to ensure path exists
        edges.push((low, high, MIN_RESERVED_CKB + 100000));
    }

    let num_layers = 20; // 20 layers
    let nodes_per_layer = node_count / num_layers; // 200 / 20 = 10 nodes per layer

    // Helper to get nodes in a layer
    let get_layer_nodes = |layer: usize| -> Vec<usize> {
        let start = layer * nodes_per_layer;
        let end = ((layer + 1) * nodes_per_layer).min(node_count);
        (start..end).collect()
    };

    // Step 1: Add cross-layer connections to increase exploration
    // Connect each node to 2-3 nodes in next layers (not just i+1)
    for layer in 0..(num_layers - 1) {
        let current_layer_nodes = get_layer_nodes(layer);
        let next_layer_nodes = get_layer_nodes(layer + 1);

        for &node_a in &current_layer_nodes {
            // Skip if we've exceeded channel count
            if edges.len() >= channel_count {
                break;
            }

            // Connect to 2 random nodes in next layer
            let num_connections = rng.gen_range(1..=2.min(next_layer_nodes.len()));
            let mut selected: HashSet<usize> = HashSet::new();

            while selected.len() < num_connections {
                let idx = rng.gen_range(0..next_layer_nodes.len());
                selected.insert(next_layer_nodes[idx]);
            }

            for &node_b in &selected {
                let (low, high) = if node_a < node_b {
                    (node_a, node_b)
                } else {
                    (node_b, node_a)
                };
                if used_pairs.insert((low, high)) {
                    let capacity = MIN_RESERVED_CKB + rng.gen_range(1000..20000);
                    edges.push((low, high, capacity));
                }
            }
        }
    }

    // Step 2: Add intra-layer connections (within same layer)
    // This provides alternative paths and increases exploration
    for layer in 1..(num_layers - 1) {
        let layer_nodes = get_layer_nodes(layer);
        if layer_nodes.len() < 2 {
            continue;
        }

        // Connect some nodes within the same layer
        let num_intra_connections = layer_nodes.len() / 2;
        for _ in 0..num_intra_connections {
            if edges.len() >= channel_count {
                break;
            }
            let idx_a = rng.gen_range(0..layer_nodes.len());
            let idx_b = rng.gen_range(0..layer_nodes.len());
            if idx_a == idx_b {
                continue;
            }
            let node_a = layer_nodes[idx_a];
            let node_b = layer_nodes[idx_b];
            let (low, high) = if node_a < node_b {
                (node_a, node_b)
            } else {
                (node_b, node_a)
            };
            if used_pairs.insert((low, high)) {
                let capacity = MIN_RESERVED_CKB + rng.gen_range(1000..3000);
                edges.push((low, high, capacity));
            }
        }
    }

    // Step 3: Fill remaining capacity with random edges
    // Prefer connections between adjacent or nearby layers
    while edges.len() < channel_count {
        let layer_a = rng.gen_range(0..num_layers);
        // Prefer connecting to nearby layers (within 1-2 hops)
        let layer_distance = rng.gen_range(1..=2);
        let layer_b = if rng.gen_bool(0.5) {
            layer_a.saturating_add(layer_distance).min(num_layers - 1)
        } else {
            layer_a.saturating_sub(layer_distance)
        };

        let nodes_a = get_layer_nodes(layer_a);
        let nodes_b = get_layer_nodes(layer_b);

        if nodes_a.is_empty() || nodes_b.is_empty() {
            continue;
        }

        let node_a = nodes_a[rng.gen_range(0..nodes_a.len())];
        let node_b = nodes_b[rng.gen_range(0..nodes_b.len())];

        if node_a == node_b {
            continue;
        }

        let (low, high) = if node_a < node_b {
            (node_a, node_b)
        } else {
            (node_b, node_a)
        };
        if used_pairs.insert((low, high)) {
            let capacity = MIN_RESERVED_CKB + rng.gen_range(500..4000);
            edges.push((low, high, capacity));
        }
    }

    let mut idx: u32 = 1;
    for (node_a, node_b, capacity) in edges {
        let public_key1 = keypairs[node_a].1;
        let public_key2 = keypairs[node_b].1;
        let node_a_is_node1 = public_key1 < public_key2;

        let mut bytes = [0u8; 36];
        bytes[0..4].copy_from_slice(&idx.to_le_bytes());
        idx = idx.saturating_add(1);
        let channel_outpoint = OutPoint::from_slice(&bytes).unwrap();

        let (node_a_key, node_b_key) = if node_a_is_node1 {
            (public_key1, public_key2)
        } else {
            (public_key2, public_key1)
        };

        store.save_channel_announcement(
            now_timestamp_as_millis_u64(),
            ChannelAnnouncement {
                chain_hash: get_chain_hash(),
                node1_id: node_a_key.into(),
                node2_id: node_b_key.into(),
                channel_outpoint: channel_outpoint.clone(),
                node1_signature: None,
                node2_signature: None,
                capacity,
                ckb_key: XOnlyPublicKey::from_slice([0x01; 32].as_ref()).unwrap(),
                ckb_signature: None,
                udt_type_script: None,
                features: 0,
            },
        );

        store.save_channel_update(ChannelUpdate::new_unsigned(
            channel_outpoint.clone(),
            now_timestamp_as_millis_u64(),
            if node_a_is_node1 {
                ChannelUpdateMessageFlags::UPDATE_OF_NODE1
            } else {
                ChannelUpdateMessageFlags::UPDATE_OF_NODE2
            },
            ChannelUpdateChannelFlags::empty(),
            DEFAULT_TLC_EXPIRY_DELTA, // 4 hours, not 24 hours
            0,                        // tlc_minimum_value
            0, // tlc_fee_proportional_millionths (fee rate, should be small, not capacity!)
        ));
        store.save_channel_update(ChannelUpdate::new_unsigned(
            channel_outpoint.clone(),
            now_timestamp_as_millis_u64(),
            if node_a_is_node1 {
                ChannelUpdateMessageFlags::UPDATE_OF_NODE2
            } else {
                ChannelUpdateMessageFlags::UPDATE_OF_NODE1
            },
            ChannelUpdateChannelFlags::empty(),
            DEFAULT_TLC_EXPIRY_DELTA, // 4 hours, not 24 hours
            0,                        // tlc_minimum_value
            0, // tlc_fee_proportional_millionths (fee rate, should be small, not capacity!)
        ));
    }

    let source = keypairs[0].1;
    let mut graph = NetworkGraph::new(store, source.into(), true);
    graph.always_process_gossip_message = true;
    graph.reload_from_store();

    let keys = keypairs.into_iter().map(|x| x.1).collect::<Vec<_>>();
    (graph, keys)
}

fn bench_find_path_single_call_large_graph(c: &mut Criterion) {
    let (graph, keys) = build_graph_for_find_path_bench(200, 2000);
    let source: Pubkey = keys[0].into();
    let target: Pubkey = keys[199].into();

    let channel_stats = GraphChannelStat::default();
    let hop_hints: Vec<fnn::fiber::payment::HopHint> = Vec::new();
    let amount = 500;

    let mut group = c.benchmark_group("find_path_single_call_large_graph");
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(10));
    group.sample_size(10);

    group.bench_function("find_path_200n_2000c", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let _ = graph.find_path(
                    source,
                    target,
                    Some(amount),
                    None,
                    None,
                    DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                    MAX_PAYMENT_TLC_EXPIRY_LIMIT,
                    false,
                    &hop_hints,
                    &channel_stats,
                    true,
                );
            }
            start.elapsed()
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_millis(1500))
        .measurement_time(std::time::Duration::from_secs(300))
        .sample_size(10);
    targets = bench_payment_path_finding, bench_find_path_single_call_large_graph
}
criterion_main!(benches);
