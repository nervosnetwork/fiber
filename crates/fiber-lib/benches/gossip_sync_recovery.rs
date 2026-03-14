#![cfg(feature = "bench")]

use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};

use ckb_types::packed::OutPoint;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fnn::fiber::channel::UpdateCommand;
use fnn::fiber::graph::ChannelInfo;
use fnn::fiber::types::{Hash256, Pubkey};
use fnn::now_timestamp_as_millis_u64;
use fnn::tests::{
    create_n_nodes_network_with_gossip_intervals, init_tracing, NetworkNode, HUGE_CKB_AMOUNT,
};
use tokio::runtime::Runtime;
use tracing::info;

const DEFAULT_NODE_COUNTS: &[usize] = &[10];
const DEFAULT_GOSSIP_NETWORK_INTERVAL_MS: u64 = 50;
const DEFAULT_GOSSIP_STORE_INTERVAL_MS: u64 = 50;
const DEFAULT_BACKLOG_UPDATES: usize = 120;
const DEFAULT_RECONNECT_PEERS: usize = 3;
const DEFAULT_PARITY_TIMEOUT_SECS: u64 = 240;
const DEFAULT_PARITY_STABLE_ROUNDS: usize = 3;
const DEFAULT_OFFLINE_NODE_INDEX: usize = 0;

const NODE_COUNTS_ENV: &str = "GOSSIP_SYNC_RECOVERY_NODE_COUNTS";
const NETWORK_INTERVAL_ENV: &str = "GOSSIP_BENCH_NETWORK_INTERVAL_MS";
const STORE_INTERVAL_ENV: &str = "GOSSIP_BENCH_STORE_INTERVAL_MS";
const BACKLOG_UPDATES_ENV: &str = "GOSSIP_SYNC_RECOVERY_BACKLOG_UPDATES";
const RECONNECT_PEERS_ENV: &str = "GOSSIP_SYNC_RECOVERY_RECONNECT_PEERS";
const PARITY_TIMEOUT_ENV: &str = "GOSSIP_SYNC_RECOVERY_PARITY_TIMEOUT_SECS";
const PARITY_STABLE_ROUNDS_ENV: &str = "GOSSIP_SYNC_RECOVERY_PARITY_STABLE_ROUNDS";
const OFFLINE_NODE_INDEX_ENV: &str = "GOSSIP_SYNC_RECOVERY_OFFLINE_NODE_INDEX";

fn make_update_command(fee_rate: u64) -> UpdateCommand {
    UpdateCommand {
        enabled: None,
        tlc_expiry_delta: None,
        tlc_minimum_value: None,
        tlc_fee_proportional_millionths: Some(fee_rate as u128),
    }
}

fn channel_update_fee(channel: &ChannelInfo, updated_by: Pubkey) -> Option<u64> {
    channel
        .get_channel_update_of(updated_by)
        .map(|update| update.fee_rate)
}

async fn current_observed_fee(
    observer: &NetworkNode,
    channel_outpoint: &OutPoint,
    updated_by: Pubkey,
) -> u64 {
    let channel = observer
        .get_network_graph_channel(channel_outpoint)
        .await
        .expect("channel should exist in observer graph");
    channel_update_fee(&channel, updated_by).expect("channel update should exist")
}

fn parse_env_usize_list(key: &str) -> Option<Vec<usize>> {
    let raw = std::env::var(key).ok()?;
    let mut values = Vec::new();
    for item in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        if let Ok(parsed) = item.parse::<usize>() {
            values.push(parsed);
        }
    }
    (!values.is_empty()).then_some(values)
}

fn parse_env_positive_u64(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn parse_env_positive_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn node_counts() -> Vec<usize> {
    let mut values =
        parse_env_usize_list(NODE_COUNTS_ENV).unwrap_or_else(|| DEFAULT_NODE_COUNTS.to_vec());
    values.retain(|v| *v >= 3);
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        DEFAULT_NODE_COUNTS.to_vec()
    } else {
        values
    }
}

fn gossip_intervals_ms() -> (Option<u64>, Option<u64>) {
    (
        Some(
            parse_env_positive_u64(NETWORK_INTERVAL_ENV)
                .unwrap_or(DEFAULT_GOSSIP_NETWORK_INTERVAL_MS),
        ),
        Some(
            parse_env_positive_u64(STORE_INTERVAL_ENV).unwrap_or(DEFAULT_GOSSIP_STORE_INTERVAL_MS),
        ),
    )
}

#[derive(Clone, Debug)]
struct SyncRecoveryConfig {
    backlog_updates: usize,
    reconnect_peers: usize,
    parity_timeout: Duration,
    parity_stable_rounds: usize,
    offline_node_index: usize,
}

fn sync_recovery_config() -> SyncRecoveryConfig {
    SyncRecoveryConfig {
        backlog_updates: parse_env_positive_usize(BACKLOG_UPDATES_ENV)
            .unwrap_or(DEFAULT_BACKLOG_UPDATES),
        reconnect_peers: parse_env_positive_usize(RECONNECT_PEERS_ENV)
            .unwrap_or(DEFAULT_RECONNECT_PEERS),
        parity_timeout: Duration::from_secs(
            parse_env_positive_u64(PARITY_TIMEOUT_ENV).unwrap_or(DEFAULT_PARITY_TIMEOUT_SECS),
        ),
        parity_stable_rounds: parse_env_positive_usize(PARITY_STABLE_ROUNDS_ENV)
            .unwrap_or(DEFAULT_PARITY_STABLE_ROUNDS),
        offline_node_index: std::env::var(OFFLINE_NODE_INDEX_ENV)
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(DEFAULT_OFFLINE_NODE_INDEX),
    }
}

fn normalize_edge(a: usize, b: usize) -> (usize, usize) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

fn add_edge(edges: &mut HashSet<(usize, usize)>, a: usize, b: usize) {
    if a != b {
        edges.insert(normalize_edge(a, b));
    }
}

fn build_realistic_topology_edges(node_count: usize) -> Vec<(usize, usize)> {
    assert!(node_count >= 3, "node_count must be >= 3");
    let mut edges = HashSet::new();

    for i in 0..node_count {
        add_edge(&mut edges, i, (i + 1) % node_count);
    }

    for i in (0..node_count).step_by(2) {
        add_edge(&mut edges, i, (i + 2) % node_count);
    }

    let half = node_count / 2;
    if half >= 2 {
        for i in (0..node_count).step_by(3) {
            add_edge(&mut edges, i, (i + half) % node_count);
        }
    }

    let mut sorted = edges.into_iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateSemanticSnapshot {
    enabled: bool,
    tlc_expiry_delta: u64,
    tlc_minimum_value: u128,
    fee_rate: u64,
}

impl From<fnn::fiber::graph::ChannelUpdateInfo> for UpdateSemanticSnapshot {
    fn from(update: fnn::fiber::graph::ChannelUpdateInfo) -> Self {
        Self {
            enabled: update.enabled,
            tlc_expiry_delta: update.tlc_expiry_delta,
            tlc_minimum_value: update.tlc_minimum_value,
            fee_rate: update.fee_rate,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChannelSnapshot {
    capacity: u128,
    features: u64,
    endpoint_a: String,
    endpoint_b: String,
    updates: BTreeMap<String, UpdateSemanticSnapshot>,
}

async fn channel_graph_snapshot(node: &NetworkNode) -> BTreeMap<String, ChannelSnapshot> {
    let mut snapshots = BTreeMap::new();
    for channel in node.get_network_graph_channels().await {
        let node1_key = format!("{:?}", channel.node1);
        let node2_key = format!("{:?}", channel.node2);
        let (endpoint_a, endpoint_b) = if node1_key <= node2_key {
            (node1_key.clone(), node2_key.clone())
        } else {
            (node2_key.clone(), node1_key.clone())
        };
        let mut updates = BTreeMap::new();
        if let Some(update) = channel.update_of_node1 {
            updates.insert(node1_key, UpdateSemanticSnapshot::from(update));
        }
        if let Some(update) = channel.update_of_node2 {
            updates.insert(node2_key, UpdateSemanticSnapshot::from(update));
        }
        snapshots.insert(
            format!("{:?}", channel.channel_outpoint),
            ChannelSnapshot {
                capacity: channel.capacity,
                features: channel.features,
                endpoint_a,
                endpoint_b,
                updates,
            },
        );
    }
    snapshots
}

fn first_graph_difference(
    recovered: &BTreeMap<String, ChannelSnapshot>,
    reference: &BTreeMap<String, ChannelSnapshot>,
) -> String {
    for (key, recovered_snapshot) in recovered {
        match reference.get(key) {
            None => {
                return format!("channel {} missing in reference", key);
            }
            Some(reference_snapshot) if reference_snapshot != recovered_snapshot => {
                return format!(
                    "channel {} differs; recovered={:?}, reference={:?}",
                    key, recovered_snapshot, reference_snapshot
                );
            }
            _ => {}
        }
    }

    for key in reference.keys() {
        if !recovered.contains_key(key) {
            return format!("channel {} missing in recovered", key);
        }
    }

    "no visible difference".to_string()
}

async fn wait_for_graph_parity(
    recovered: &NetworkNode,
    reference: &NetworkNode,
    parity_timeout: Duration,
    stable_rounds: usize,
) {
    let started = Instant::now();
    let mut stable_matches = 0usize;

    loop {
        let recovered_snapshot = channel_graph_snapshot(recovered).await;
        let reference_snapshot = channel_graph_snapshot(reference).await;

        if recovered_snapshot == reference_snapshot {
            stable_matches = stable_matches.saturating_add(1);
            if stable_matches >= stable_rounds {
                return;
            }
        } else {
            stable_matches = 0;
        }

        if started.elapsed() > parity_timeout {
            let diff = first_graph_difference(&recovered_snapshot, &reference_snapshot);
            panic!(
                "sync recovery parity timeout after {:?}: recovered_channels={}, reference_channels={}, required_stable_rounds={}, first_difference={}",
                parity_timeout,
                recovered_snapshot.len(),
                reference_snapshot.len(),
                stable_rounds,
                diff,
            );
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Clone, Debug)]
struct RecoveryWorkload {
    channel_id: Hash256,
    updater_idx: usize,
    next_fee: u64,
}

struct SyncRecoveryScenario {
    nodes: Vec<NetworkNode>,
    offline_idx: usize,
    reference_idx: usize,
    reconnect_peer_indices: Vec<usize>,
    workloads: Vec<RecoveryWorkload>,
}

async fn connect_nodes_by_index(nodes: &mut [NetworkNode], from_idx: usize, to_idx: usize) {
    assert_ne!(from_idx, to_idx);
    if from_idx < to_idx {
        let (left, right) = nodes.split_at_mut(to_idx);
        left[from_idx].connect_to(&mut right[0]).await;
    } else {
        let (left, right) = nodes.split_at_mut(from_idx);
        right[0].connect_to(&mut left[to_idx]).await;
    }
}

async fn prepare_sync_recovery_scenario(
    node_count: usize,
    config: &SyncRecoveryConfig,
    gossip_network_interval_ms: Option<u64>,
    gossip_store_interval_ms: Option<u64>,
) -> SyncRecoveryScenario {
    let topology_edges = build_realistic_topology_edges(node_count);
    let channel_plan = topology_edges
        .iter()
        .map(|(a, b)| ((*a, *b), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)))
        .collect::<Vec<_>>();

    let (nodes, channel_ids) = create_n_nodes_network_with_gossip_intervals(
        &channel_plan,
        node_count,
        gossip_network_interval_ms,
        gossip_store_interval_ms,
    )
    .await;

    let offline_idx = config.offline_node_index % node_count;
    let reconnect_peer_indices = (0..node_count)
        .filter(|idx| *idx != offline_idx)
        .take(config.reconnect_peers.min(node_count - 1))
        .collect::<Vec<_>>();
    let reference_idx = *reconnect_peer_indices
        .first()
        .expect("at least one reconnect peer should exist");

    let mut workloads = Vec::new();
    for (idx, (a, b)) in topology_edges.iter().copied().enumerate() {
        if a == offline_idx || b == offline_idx {
            continue;
        }

        let channel_id = channel_ids[idx];
        let updater_idx = a;
        let updater = &nodes[updater_idx];
        let updater_pubkey = updater.get_public_key();
        let channel_outpoint = updater
            .get_channel_outpoint(&channel_id)
            .expect("channel outpoint should exist");
        let initial_fee = current_observed_fee(updater, &channel_outpoint, updater_pubkey).await;

        workloads.push(RecoveryWorkload {
            channel_id,
            updater_idx,
            next_fee: initial_fee,
        });
    }

    assert!(
        !workloads.is_empty(),
        "sync recovery workloads should not be empty"
    );

    info!(
        node_count,
        channel_count = channel_plan.len(),
        offline_idx,
        reference_idx,
        reconnect_peers = reconnect_peer_indices.len(),
        backlog_updates = config.backlog_updates,
        "prepared gossip sync recovery scenario"
    );

    SyncRecoveryScenario {
        nodes,
        offline_idx,
        reference_idx,
        reconnect_peer_indices,
        workloads,
    }
}

async fn apply_backlog_updates(scenario: &mut SyncRecoveryScenario, updates: usize) {
    // ChannelUpdate uses millisecond timestamps. If we enqueue multiple updates
    // within the same millisecond for the same channel, some peers may keep
    // different final versions due to equal-timestamp tie handling.
    // Enforce a monotonic millisecond boundary between updates in this bench
    // so parity can converge deterministically.
    let mut last_update_finished_at_ms = now_timestamp_as_millis_u64();

    for index in 0..updates {
        while now_timestamp_as_millis_u64() <= last_update_finished_at_ms {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let workload_idx = index % scenario.workloads.len();
        let workload = &mut scenario.workloads[workload_idx];
        workload.next_fee = workload.next_fee.saturating_add(1);

        scenario.nodes[workload.updater_idx]
            .update_channel_with_command(
                workload.channel_id,
                make_update_command(workload.next_fee),
            )
            .await;

        last_update_finished_at_ms = now_timestamp_as_millis_u64();
    }
}

async fn run_sync_recovery_iteration(
    scenario: &mut SyncRecoveryScenario,
    config: &SyncRecoveryConfig,
) -> Duration {
    scenario.nodes[scenario.offline_idx].stop().await;

    apply_backlog_updates(scenario, config.backlog_updates).await;

    scenario.nodes[scenario.offline_idx].start().await;

    let started = Instant::now();
    for peer_idx in scenario.reconnect_peer_indices.clone() {
        connect_nodes_by_index(&mut scenario.nodes, scenario.offline_idx, peer_idx).await;
    }

    wait_for_graph_parity(
        &scenario.nodes[scenario.offline_idx],
        &scenario.nodes[scenario.reference_idx],
        config.parity_timeout,
        config.parity_stable_rounds,
    )
    .await;

    started.elapsed()
}

fn bench_gossip_sync_recovery(c: &mut Criterion) {
    init_tracing();
    let rt = Runtime::new().expect("create runtime");
    let mut group = c.benchmark_group("gossip_sync_recovery");

    let node_counts = node_counts();
    let config = sync_recovery_config();
    let (gossip_network_interval_ms, gossip_store_interval_ms) = gossip_intervals_ms();

    info!(
        ?node_counts,
        ?config,
        gossip_network_interval_ms,
        gossip_store_interval_ms,
        "configured gossip sync recovery benchmark"
    );

    for node_count in node_counts {
        let mut scenario = rt.block_on(prepare_sync_recovery_scenario(
            node_count,
            &config,
            gossip_network_interval_ms,
            gossip_store_interval_ms,
        ));

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("reconnect_to_parity", node_count), |b| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut measured = Duration::ZERO;
                    for _ in 0..iters {
                        measured += run_sync_recovery_iteration(&mut scenario, &config).await;
                    }
                    measured
                })
            });
        });
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(20))
        .sample_size(10);
    targets = bench_gossip_sync_recovery
}
criterion_main!(benches);
