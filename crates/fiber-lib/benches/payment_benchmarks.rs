#![cfg(feature = "bench")]
use ckb_types::packed::OutPoint;
use ckb_types::prelude::Entity;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fnn::fiber::config::{DEFAULT_TLC_EXPIRY_DELTA, DEFAULT_FINAL_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT};
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
use secp256k1::{PublicKey, SecretKey, XOnlyPublicKey};
use std::collections::HashSet;
use std::time::Instant;
use tokio::runtime::Runtime;

fn bench_payment_path_finding(c: &mut Criterion) {
    // Create a runtime for async functions
    let rt = Runtime::new().unwrap();

    // Create a benchmark group with minimal/no warmup
    let mut group = c.benchmark_group("payment_path_finding");

    for num_channels in [1, 2, 4, 8] {
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

fn bench_payment_path_finding_basic_mpp(c: &mut Criterion) {
    // Create a runtime for async functions
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("payment_path_finding_basic_mpp");

    // Use many parallel channels between sender and receiver (a known-good basic MPP setup),
    // and also add extra nodes/channels to make the graph larger.
    for parallel_channels in [2_usize, 4, 8, 16] {
        group.throughput(Throughput::Elements(parallel_channels as u64));

        group.bench_function(BenchmarkId::new("basic_mpp", parallel_channels), |b| {
            b.iter_custom(|iters| {
                rt.block_on(async move {
                    const PART_AMOUNT: u128 = 10_000_000_000;
                    const EXTRA_NODES: usize = 30;

                    let sender = 0;
                    let receiver = 1;
                    let node_count = std::cmp::max(EXTRA_NODES, 2);
                    let max_parts = std::cmp::min(parallel_channels, 4) as u64;
                    let payment_amount = (max_parts as u128) * PART_AMOUNT;

                    let mut channel_configs = Vec::new();

                    // Parallel channels between sender and receiver so MPP must split.
                    for _ in 0..parallel_channels {
                        channel_configs.push((
                            (sender, receiver),
                            (MIN_RESERVED_CKB + PART_AMOUNT, MIN_RESERVED_CKB),
                        ));
                    }

                    // Add extra channels among other nodes to enlarge the graph.
                    for i in 2..(node_count - 1) {
                        channel_configs.push((
                            (i, i + 1),
                            (MIN_RESERVED_CKB + PART_AMOUNT, MIN_RESERVED_CKB),
                        ));
                        if i + 2 < node_count {
                            channel_configs.push((
                                (i, i + 2),
                                (MIN_RESERVED_CKB + PART_AMOUNT, MIN_RESERVED_CKB),
                            ));
                        }
                    }

                    let (nodes, _channels) =
                        create_n_nodes_network(&channel_configs, node_count).await;

                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let start = std::time::Instant::now();

                    for _ in 0..iters {
                        let res = nodes[sender]
                            .send_mpp_payment(&nodes[receiver], payment_amount, Some(max_parts))
                            .await
                            .unwrap();
                        nodes[sender].wait_until_success(res.payment_hash).await;
                    }

                    start.elapsed()
                })
            });
        });
    }

    group.finish();
}

fn bench_build_payment_routes_large_graph(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("payment_build_routes_large_graph");

    group.bench_function(BenchmarkId::new("mpp_dry_run", "200n_2000c"), |b| {
        b.iter_custom(|iters| {
            rt.block_on(async move {
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    let node_count = 200usize;
                    let channel_count = 2000usize;
                    let mut edges: Vec<((usize, usize), (u128, u128))> =
                        Vec::with_capacity(channel_count);
                    let mut used_pairs: HashSet<(usize, usize)> =
                        HashSet::with_capacity(channel_count * 2);

                    for i in 0..(node_count - 1) {
                        let pair = (i, i + 1);
                        used_pairs.insert(pair);
                        edges.push((pair, (MIN_RESERVED_CKB + 1_000_000, MIN_RESERVED_CKB)));
                    }
                    let last_pair = (0, node_count - 1);
                    if used_pairs.insert(last_pair) {
                        edges.push((last_pair, (MIN_RESERVED_CKB + 1_000_000, MIN_RESERVED_CKB)));
                    }

                    let mut rng = StdRng::seed_from_u64(42);
                    while edges.len() < channel_count {
                        let mut a = rng.gen_range(0..node_count);
                        let mut b = rng.gen_range(0..node_count);
                        if a == b {
                            continue;
                        }
                        if a > b {
                            std::mem::swap(&mut a, &mut b);
                        }
                        let pair = (a, b);
                        if used_pairs.insert(pair) {
                            edges.push((pair, (MIN_RESERVED_CKB + 1_000_000, MIN_RESERVED_CKB)));
                        }
                    }

                    let (nodes, _channels) = create_n_nodes_network(&edges, node_count).await;
                    let source_node = &nodes[0];
                    let target_node = &nodes[node_count - 1];

                    let amount: u128 = 100_000_000;
                    let max_parts = Some(16);

                    let res = source_node
                        .send_mpp_payment_with_dry_run_option(target_node, amount, max_parts, true)
                        .await;

                    if let Ok(res) = res {
                        let payment_hash = res.payment_hash;
                        let _ = source_node
                            .with_network_graph(|graph| {
                                graph.get_payment_find_path_count(&payment_hash)
                            })
                            .await;
                    }
                }
                start.elapsed()
            })
        });
    });

    group.finish();
}

fn generate_key_pairs(num: usize) -> Vec<(SecretKey, PublicKey)> {
    let mut keys = vec![];
    for _ in 0..num {
        keys.push(gen_rand_secp256k1_keypair_tuple());
    }
    keys
}

fn build_graph_for_find_path_bench(
    node_count: usize,
    channel_count: usize,
) -> (NetworkGraph<Store>, Vec<PublicKey>) {
    let (store, _dir) = generate_store();
    let keypairs = generate_key_pairs(node_count);

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
    // BUT FIRST: Create a guaranteed linear path 0 -> 1 -> 2 -> ... -> 199
    // to ensure connectivity
    
    eprintln!("Creating guaranteed linear path 0 -> 1 -> 2 -> ... -> 199");
    for i in 0..(node_count - 1) {
        let (low, high) = (i, i + 1);
        used_pairs.insert((low, high));
        // Use high capacity to ensure path exists
        edges.push((low, high, MIN_RESERVED_CKB + 100000));
    }
    
    eprintln!("Linear path created with {} edges", edges.len());
    
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
                let (low, high) = if node_a < node_b { (node_a, node_b) } else { (node_b, node_a) };
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
            let (low, high) = if node_a < node_b { (node_a, node_b) } else { (node_b, node_a) };
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
        
        let (low, high) = if node_a < node_b { (node_a, node_b) } else { (node_b, node_a) };
        if used_pairs.insert((low, high)) {
            let capacity = MIN_RESERVED_CKB + rng.gen_range(500..4000);
            edges.push((low, high, capacity));
        }
    }

    eprintln!("Generated {} edges for {} nodes (layered topology)", edges.len(), node_count);

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
            0, // tlc_minimum_value
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
            0, // tlc_minimum_value
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
    // Build graph once before benchmarking
    let (graph, keys) = build_graph_for_find_path_bench(200, 2000);
    let source: Pubkey = keys[0].into();
    let target: Pubkey = keys[199].into();
    
    // Diagnostic: check graph structure
    eprintln!("\n=== Graph Diagnostic ===");
    
    let all_nodes = graph.get_nodes_with_params(300, None);
    eprintln!("Total nodes loaded: {}", all_nodes.len());
    
    // Check if source and target are in the graph
    let source_in_graph = all_nodes.iter().any(|n| n.node_id == source);
    let target_in_graph = all_nodes.iter().any(|n| n.node_id == target);
    eprintln!("Source {:?} in graph: {}", source, source_in_graph);
    eprintln!("Target {:?} in graph: {}", target, target_in_graph);
    
    // CRITICAL CHECK: Verify if node[1] is among source's neighbors
    eprintln!("\n*** Checking if linear path was created ***");
    let node1_pubkey: Pubkey = keys[1].into();
    eprintln!("Node[1] pubkey: {:?}", node1_pubkey);
    
    // Check source node channels and their peers
    let source_channels: Vec<_> = graph.get_channels_by_peer(source).collect();
    eprintln!("Source node has {} channels", source_channels.len());
    
    let has_node1_neighbor = source_channels.iter().any(|ch| {
        let peer = if ch.node1() == source { ch.node2() } else { ch.node1() };
        peer == node1_pubkey
    });
    eprintln!("Source connected to node[1]? {}", has_node1_neighbor);
    
    if !has_node1_neighbor {
        eprintln!("ERROR: Linear path 0->1 not found in graph!");
        eprintln!("This means our edge creation didn't work as expected.");
    }
    
    // CRITICAL CHECK 2: Verify the END of the linear path (198 -> 199)
    let node198_pubkey: Pubkey = keys[198].into();
    eprintln!("\nNode[198] pubkey: {:?}", node198_pubkey);
    
    let node198_channels: Vec<_> = graph.get_channels_by_peer(node198_pubkey).collect();
    eprintln!("Node[198] has {} channels", node198_channels.len());
    
    let has_node199_neighbor = node198_channels.iter().any(|ch| {
        let peer = if ch.node1() == node198_pubkey { ch.node2() } else { ch.node1() };
        peer == target
    });
    eprintln!("Node[198] connected to node[199]? {}", has_node199_neighbor);
    
    if !has_node199_neighbor {
        eprintln!("ERROR: Linear path 198->199 not found in graph!");
        eprintln!("The linear path is broken somewhere!");
    }
    
    // Check source node channels and their peers
    let source_channels: Vec<_> = graph.get_channels_by_peer(source).collect();
    eprintln!("\nSource node has {} channels", source_channels.len());
    if !source_channels.is_empty() {
        eprintln!("Source neighbors:");
        for (idx, ch) in source_channels.iter().enumerate().take(5) {
            let peer = if ch.node1() == source { ch.node2() } else { ch.node1() };
            eprintln!("  {} -> peer {:?}, capacity={}", idx, peer, ch.capacity());
        }
    }
    
    // Check target node channels and their peers
    let target_channels: Vec<_> = graph.get_channels_by_peer(target).collect();
    eprintln!("\nTarget node has {} channels", target_channels.len());
    if !target_channels.is_empty() {
        eprintln!("Target neighbors:");
        for (idx, ch) in target_channels.iter().enumerate().take(5) {
            let peer = if ch.node1() == target { ch.node2() } else { ch.node1() };
            eprintln!("  {} -> peer {:?}, capacity={}", idx, peer, ch.capacity());
        }
    }
    
    // Try to find if there's a direct path from source
    eprintln!("\nChecking connectivity from source (BFS)...");
    let mut visited = HashSet::new();
    let mut queue = Vec::new();
    queue.push(source);
    visited.insert(source);
    
    let mut depth = 0;
    let max_depth = 25; // Increase to check longer paths
    while !queue.is_empty() && depth < max_depth {
        let current_level = queue.clone();
        queue.clear();
        
        for node in current_level {
            let neighbors: Vec<_> = graph.get_channels_by_peer(node)
                .map(|ch| if ch.node1() == node { ch.node2() } else { ch.node1() })
                .filter(|peer| visited.insert(*peer))
                .collect();
            
            if neighbors.contains(&target) {
                eprintln!("✓ Found path to target at depth {}!", depth + 1);
                eprintln!("  (Linear path 0->1->...->199 requires 199 hops)");
                break;
            }
            
            queue.extend(neighbors);
        }
        
        depth += 1;
        if depth <= 10 || depth % 5 == 0 {
            eprintln!("  Depth {}: visited {} nodes total", depth, visited.len());
        }
    }
    
    if !visited.contains(&target) {
        eprintln!("✗ Target not reachable within {} hops", max_depth);
        eprintln!("  Linear path exists but needs ~199 hops!");
    } else {
        eprintln!("✓ Target IS reachable! Path exists.");
    }
    
    // Analyze why find_path fails despite path existing
    eprintln!("\n=== Analyzing why find_path fails ===");
    if !source_channels.is_empty() {
        eprintln!("Checking source's channel updates:");
        let mut enabled_count = 0;
        let mut disabled_count = 0;
        let mut missing_count = 0;
        
        for ch in source_channels.iter().take(10) {
            let peer = if ch.node1() == source { ch.node2() } else { ch.node1() };
            let update = ch.get_channel_update_of(peer);
            match update {
                Some(u) if u.enabled => {
                    enabled_count += 1;
                    if enabled_count <= 2 {
                        eprintln!("  ✓ Channel to peer: enabled={}, fee={}", u.enabled, u.fee_rate);
                    }
                }
                Some(u) => {
                    disabled_count += 1;
                    if disabled_count <= 2 {
                        eprintln!("  ✗ Channel to peer: DISABLED!");
                    }
                }
                None => {
                    missing_count += 1;
                    if missing_count <= 2 {
                        eprintln!("  ? Channel to peer: NO UPDATE!");
                    }
                }
            }
        }
        eprintln!("Summary: {} enabled, {} disabled, {} missing updates", 
            enabled_count, disabled_count, missing_count);
        
        if enabled_count == 0 {
            eprintln!("*** CRITICAL: All channels disabled or missing updates!");
            eprintln!("*** This is why pathfinding can't proceed!");
        }
    }
    
    // CRITICAL DIAGNOSTIC: Check target's inbounds
    eprintln!("\n*** CRITICAL: Check TARGET inbounds (needed for pathfinding) ***");
    // Pathfinding starts from target and searches backwards to source
    // So we need to check target's inbounds (who can send TO target)
    
    use fnn::fiber::graph::NetworkGraph;
    let target_inbounds: Vec<_> = graph.get_node_inbounds(target).collect();
    eprintln!("Target has {} inbound edges (from get_node_inbounds)", target_inbounds.len());
    
    if target_inbounds.is_empty() {
        eprintln!("!!! CRITICAL ERROR: Target has NO inbound edges !!!");
        eprintln!("!!! This means pathfinding cannot start from target !!!");
        eprintln!("Expected: target (node[199]) should have inbound from node[198]");
        
        // Let's check the channel between node[198] and node[199] manually
        let node198_pubkey: Pubkey = keys[198].into();
        eprintln!("\nManual check: Looking for channel between node[198] and node[199]...");
        let all_channels: Vec<_> = graph.channels().collect();
        let target_related: Vec<_> = all_channels.iter()
            .filter(|ch| ch.node1() == target || ch.node2() == target)
            .collect();
        eprintln!("Total channels involving target: {}", target_related.len());
        
        for ch in target_related.iter().take(5) {
            let peer = if ch.node1() == target { ch.node2() } else { ch.node1() };
            eprintln!("  Channel: node1={:?}, node2={:?}", ch.node1(), ch.node2());
            eprintln!("    Peer of target: {:?}", peer);
            eprintln!("    Is peer node[198]? {}", peer == node198_pubkey);
            eprintln!("    update_of_node1: enabled={:?}", ch.update_of_node1.as_ref().map(|u| u.enabled));
            eprintln!("    update_of_node2: enabled={:?}", ch.update_of_node2.as_ref().map(|u| u.enabled));
            
            // Check if this channel would appear in get_node_inbounds(target)
            // get_node_inbounds checks:
            // 1. if target == node2 && update_of_node1.enabled -> inbound from node1
            // 2. if target == node1 && update_of_node2.enabled -> inbound from node2
            let appears_in_inbound = if target == ch.node2() {
                eprintln!("    Target is node2, checking update_of_node1.enabled");
                ch.update_of_node1.as_ref().map(|u| u.enabled).unwrap_or(false)
            } else if target == ch.node1() {
                eprintln!("    Target is node1, checking update_of_node2.enabled");
                ch.update_of_node2.as_ref().map(|u| u.enabled).unwrap_or(false)
            } else {
                false
            };
            eprintln!("    Would appear in get_node_inbounds(target)? {}", appears_in_inbound);
        }
    } else {
        eprintln!("✓ Target has {} inbound edges, showing first few:", target_inbounds.len());
        for (idx, (from, to, ch, update)) in target_inbounds.iter().take(3).enumerate() {
            eprintln!("  Inbound {}: from={:?}, to={:?}, enabled={}, fee={}", 
                idx, from, to, update.enabled, update.fee_rate);
        }
    }
    
    // Check if we can find ANY path with a very small amount
    let test_amounts = [100, 500, 1000, 5000];
    for &test_amount in &test_amounts {
        let result = graph.find_path(
            source,
            target,
            Some(test_amount),
            None,
            None,
            DEFAULT_FINAL_TLC_EXPIRY_DELTA,
            MAX_PAYMENT_TLC_EXPIRY_LIMIT,
            false,
            &[],
            &GraphChannelStat::default(),
            true,
        );
        
        match result {
            Ok(route) => {
                eprintln!("✓ Found path for amount {}: {} hops", test_amount, route.len());
                break;
            }
            Err(e) => {
                eprintln!("✗ No path for amount {}: {:?}", test_amount, e);
            }
        }
    }
    eprintln!("======================\n");
    
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

async fn run_mpp_find_path_counts(channel_closed: bool) -> u128 {
    let high_capacity = MIN_RESERVED_CKB + 5_000;
    let low_capacity = MIN_RESERVED_CKB + 1_100;
    let channel_configs = vec![
        ((0, 1), (high_capacity, high_capacity)),
        ((1, 3), (high_capacity, high_capacity)),
        ((0, 2), (low_capacity, low_capacity)),
        ((2, 3), (low_capacity, low_capacity)),
    ];

    let (nodes, channels) = create_n_nodes_network(&channel_configs, 4).await;
    let source_node = &nodes[0];
    let target_node = &nodes[3];

    if channel_closed {
        source_node.mark_channel_failed_for_test(channels[0]).await;
    }

    let amount: u128 = 2020;
    let max_parts = Some(4);
    let res = source_node
        .send_mpp_payment_with_dry_run_option(target_node, amount, max_parts, true)
        .await;

    let payment_hash = res
        .expect("dry-run should return a payment hash")
        .payment_hash;
    source_node
        .with_network_graph(|graph| graph.get_payment_find_path_count(&payment_hash))
        .await
}

fn bench_mpp_closed_channel_find_path_iterates(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("payment_mpp_closed_channel");

    group.bench_function(BenchmarkId::new("find_path_iters", "amount_2020"), |b| {
        b.iter_custom(|iters| {
            rt.block_on(async move {
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    let open_count = run_mpp_find_path_counts(false).await;
                    let closed_count = run_mpp_find_path_counts(true).await;
                    assert!(
                        closed_count >= open_count,
                        "expected closed channel to increase find_path iterations: open={}, closed={}",
                        open_count,
                        closed_count
                    );
                }
                start.elapsed()
            })
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
    targets = bench_payment_path_finding,
        bench_payment_path_finding_basic_mpp,
        bench_build_payment_routes_large_graph,
        bench_mpp_closed_channel_find_path_iterates,
        bench_find_path_single_call_large_graph
}
criterion_main!(benches);
