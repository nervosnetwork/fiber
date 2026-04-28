#![cfg(feature = "bench")]

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use ckb_types::packed::OutPoint;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fnn::fiber::channel::UpdateCommand;
use fnn::fiber::graph::ChannelInfo;
use fnn::fiber::types::{Hash256, Pubkey};
use fnn::tests::{
    create_n_nodes_network_with_gossip_intervals, init_tracing, NetworkNode, HUGE_CKB_AMOUNT,
};
use tokio::runtime::Runtime;
use tracing::info;

const DEFAULT_BENCH_NODE_COUNTS: &[usize] = &[10, 20];
const DEFAULT_MAX_OBSERVERS_PER_EDGE: usize = 8;
const GOSSIP_BENCH_NETWORK_INTERVAL_MS_ENV: &str = "GOSSIP_BENCH_NETWORK_INTERVAL_MS";
const GOSSIP_BENCH_STORE_INTERVAL_MS_ENV: &str = "GOSSIP_BENCH_STORE_INTERVAL_MS";
const GOSSIP_BENCH_PROPAGATION_ROUNDS_ENV: &str = "GOSSIP_BENCH_PROPAGATION_ROUNDS";
const GOSSIP_BENCH_WAIT_TIMEOUT_SECS_ENV: &str = "GOSSIP_BENCH_WAIT_TIMEOUT_SECS";
// Benchmark defaults follow production gossip maintenance intervals (60s/20s).
// Use env overrides for faster local iteration when needed.
const DEFAULT_GOSSIP_NETWORK_INTERVAL_MS: u64 = 60_000;
const DEFAULT_GOSSIP_STORE_INTERVAL_MS: u64 = 20_000;
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 180;

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

async fn wait_for_fee_propagated(
    observer: &NetworkNode,
    channel_outpoint: &OutPoint,
    updated_by: Pubkey,
    expected_fee: u64,
    wait_timeout: Duration,
) -> Duration {
    let started = Instant::now();
    loop {
        if started.elapsed() > wait_timeout {
            panic!(
                "gossip propagation timeout for observer {:?} waiting fee {}",
                observer.get_public_key(),
                expected_fee
            );
        }

        if let Some(channel) = observer.get_network_graph_channel(channel_outpoint).await {
            if channel_update_fee(&channel, updated_by) == Some(expected_fee) {
                return started.elapsed();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
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

// Benchmark-only synthetic topology.
//
// This does NOT represent any production Fiber topology policy. It is used to
// build a reproducible multi-node graph with:
// 1) guaranteed connectivity (ring),
// 2) medium-distance shortcuts (i -> i+2),
// 3) a few long-range links (i -> i+N/2).
//
// The combination keeps setup deterministic while avoiding a too-simple chain
// or clique structure.
fn build_realistic_topology_edges(node_count: usize) -> Vec<(usize, usize)> {
    assert!(node_count >= 3, "node_count must be >= 3");
    let mut edges = HashSet::new();

    // Base ring to ensure connectivity.
    for i in 0..node_count {
        add_edge(&mut edges, i, (i + 1) % node_count);
    }

    // Local shortcuts to reduce diameter.
    for i in (0..node_count).step_by(2) {
        add_edge(&mut edges, i, (i + 2) % node_count);
    }

    // A few long-range links for realism.
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

fn bench_node_counts() -> Vec<usize> {
    let mut values = parse_env_usize_list("GOSSIP_BENCH_NODE_COUNTS")
        .unwrap_or_else(|| DEFAULT_BENCH_NODE_COUNTS.to_vec());
    values.retain(|v| *v >= 3);
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        DEFAULT_BENCH_NODE_COUNTS.to_vec()
    } else {
        values
    }
}

fn max_observers_per_edge() -> usize {
    std::env::var("GOSSIP_BENCH_MAX_OBSERVERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_OBSERVERS_PER_EDGE)
}

fn parse_env_positive_u64(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
}

fn bench_gossip_intervals_ms() -> (Option<u64>, Option<u64>) {
    (
        Some(
            parse_env_positive_u64(GOSSIP_BENCH_NETWORK_INTERVAL_MS_ENV)
                .unwrap_or(DEFAULT_GOSSIP_NETWORK_INTERVAL_MS),
        ),
        Some(
            parse_env_positive_u64(GOSSIP_BENCH_STORE_INTERVAL_MS_ENV)
                .unwrap_or(DEFAULT_GOSSIP_STORE_INTERVAL_MS),
        ),
    )
}

fn propagation_rounds(default_rounds: usize) -> usize {
    std::env::var(GOSSIP_BENCH_PROPAGATION_ROUNDS_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|rounds| *rounds > 0)
        .unwrap_or(default_rounds)
}

fn bench_wait_timeout() -> Duration {
    Duration::from_secs(
        std::env::var(GOSSIP_BENCH_WAIT_TIMEOUT_SECS_ENV)
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS),
    )
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    assert!(
        !values.is_empty(),
        "cannot compute percentile for empty latency samples"
    );
    if values.len() == 1 {
        return values[0];
    }

    let rank = quantile.clamp(0.0, 1.0) * (values.len() - 1) as f64;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    if low == high {
        values[low]
    } else {
        let ratio = rank - low as f64;
        values[low] + (values[high] - values[low]) * ratio
    }
}

fn format_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn observer_indices_for_edge(
    node_count: usize,
    a: usize,
    b: usize,
    max_observers: usize,
) -> Vec<usize> {
    // Observers are all nodes except channel endpoints (a, b).
    let candidates = (0..node_count)
        .filter(|idx| *idx != a && *idx != b)
        .collect::<Vec<_>>();
    if candidates.len() <= max_observers {
        return candidates;
    }
    // To cap runtime in larger N, pick observers evenly from the candidate set
    // instead of taking a contiguous prefix.
    (0..max_observers)
        .map(|i| {
            let pos = i * candidates.len() / max_observers;
            candidates[pos]
        })
        .collect::<Vec<_>>()
}

struct ChannelWorkload {
    channel_id: Hash256,
    channel_outpoint: OutPoint,
    updater_idx: usize,
    updater_pubkey: Pubkey,
    observer_indices: Vec<usize>,
    next_fee: u64,
}

struct MultiNodeChannelUpdateTestbed {
    nodes: Vec<NetworkNode>,
    workloads: Vec<ChannelWorkload>,
    next_workload_idx: usize,
}

async fn wait_until_graph_channel_has_update_for_bench(
    node: &NetworkNode,
    node1: &NetworkNode,
    node2: &NetworkNode,
    wait_timeout: Duration,
) {
    let updater_pubkey = node1.get_public_key();
    let peer_pubkey = node2.get_public_key();
    let started = Instant::now();

    loop {
        let has_update = node
            .get_network_graph_channels()
            .await
            .into_iter()
            .any(|c| {
                let is_pair = (c.node1 == updater_pubkey && c.node2 == peer_pubkey)
                    || (c.node1 == peer_pubkey && c.node2 == updater_pubkey);
                let updater_has_update = (c.node1 == updater_pubkey && c.update_of_node1.is_some())
                    || (c.node2 == updater_pubkey && c.update_of_node2.is_some());
                is_pair && updater_has_update
            });
        if has_update {
            return;
        }
        if started.elapsed() > wait_timeout {
            panic!(
                "timeout waiting graph update for pair {:?}<->{:?} after {:?}",
                updater_pubkey, peer_pubkey, wait_timeout
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn prepare_channel_workloads(
    nodes: &[NetworkNode],
    topology_edges: &[(usize, usize)],
    channel_ids: &[Hash256],
    max_observers: usize,
    wait_timeout: Duration,
) -> Vec<ChannelWorkload> {
    let mut workloads = Vec::with_capacity(topology_edges.len());

    for (idx, (updater_idx, peer_idx)) in topology_edges.iter().copied().enumerate() {
        let updater = &nodes[updater_idx];
        let peer = &nodes[peer_idx];
        let observer_indices =
            observer_indices_for_edge(nodes.len(), updater_idx, peer_idx, max_observers);
        let first_observer_idx = observer_indices
            .first()
            .copied()
            .expect("at least one observer should exist");

        for observer_idx in &observer_indices {
            wait_until_graph_channel_has_update_for_bench(
                &nodes[*observer_idx],
                updater,
                peer,
                wait_timeout,
            )
            .await;
        }

        let channel_id = channel_ids[idx];
        let channel_outpoint = updater
            .get_channel_outpoint(&channel_id)
            .expect("channel outpoint should exist");
        let updater_pubkey = updater.get_public_key();
        let initial_fee = current_observed_fee(
            &nodes[first_observer_idx],
            &channel_outpoint,
            updater_pubkey,
        )
        .await;

        workloads.push(ChannelWorkload {
            channel_id,
            channel_outpoint,
            updater_idx,
            updater_pubkey,
            observer_indices,
            next_fee: initial_fee,
        });
    }

    workloads
}

async fn prepare_multi_node_channel_update_testbed(
    node_count: usize,
    max_observers: usize,
    gossip_network_interval_ms: Option<u64>,
    gossip_store_interval_ms: Option<u64>,
    wait_timeout: Duration,
) -> MultiNodeChannelUpdateTestbed {
    let topology_edges = build_realistic_topology_edges(node_count);
    let channel_plan = topology_edges
        .iter()
        .map(|(a, b)| ((*a, *b), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)))
        .collect::<Vec<_>>();

    info!(
        node_count,
        channel_count = channel_plan.len(),
        max_observers,
        gossip_network_interval_ms,
        gossip_store_interval_ms,
        "starting multi-node gossip testbed"
    );

    let (nodes, channel_ids) = create_n_nodes_network_with_gossip_intervals(
        &channel_plan,
        node_count,
        gossip_network_interval_ms,
        gossip_store_interval_ms,
    )
    .await;
    let workloads = prepare_channel_workloads(
        &nodes,
        &topology_edges,
        &channel_ids,
        max_observers,
        wait_timeout,
    )
    .await;

    info!(
        node_count,
        channel_count = workloads.len(),
        "multi-node gossip testbed is ready"
    );

    MultiNodeChannelUpdateTestbed {
        nodes,
        workloads,
        next_workload_idx: 0,
    }
}

async fn bench_multi_node_channel_update_propagation_iters(
    testbed: &mut MultiNodeChannelUpdateTestbed,
    iters: u64,
    wait_timeout: Duration,
) -> Duration {
    let started = Instant::now();

    for _ in 0..iters {
        let workload_idx = testbed.next_workload_idx;
        testbed.next_workload_idx = (testbed.next_workload_idx + 1) % testbed.workloads.len();
        let workload = &mut testbed.workloads[workload_idx];
        workload.next_fee = workload.next_fee.saturating_add(1);

        testbed.nodes[workload.updater_idx]
            .update_channel_with_command(
                workload.channel_id,
                make_update_command(workload.next_fee),
            )
            .await;

        for observer_idx in &workload.observer_indices {
            let _ = wait_for_fee_propagated(
                &testbed.nodes[*observer_idx],
                &workload.channel_outpoint,
                workload.updater_pubkey,
                workload.next_fee,
                wait_timeout,
            )
            .await;
        }
    }

    started.elapsed()
}

async fn collect_observer_propagation_latencies(
    nodes: &[NetworkNode],
    observer_indices: &[usize],
    channel_outpoint: &OutPoint,
    updated_by: Pubkey,
    expected_fee: u64,
    started: Instant,
    wait_timeout: Duration,
) -> Vec<Duration> {
    let mut samples: Vec<Option<Duration>> = vec![None; observer_indices.len()];
    loop {
        if started.elapsed() > wait_timeout {
            panic!(
                "gossip propagation timeout waiting all observers (remaining: {}) for fee {}",
                samples.iter().filter(|latency| latency.is_none()).count(),
                expected_fee
            );
        }

        let mut pending = 0usize;
        for (idx, observer_idx) in observer_indices.iter().enumerate() {
            if samples[idx].is_some() {
                continue;
            }
            if let Some(channel) = nodes[*observer_idx]
                .get_network_graph_channel(channel_outpoint)
                .await
            {
                if channel_update_fee(&channel, updated_by) == Some(expected_fee) {
                    samples[idx] = Some(started.elapsed());
                    continue;
                }
            }
            pending += 1;
        }

        if pending == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    samples
        .into_iter()
        .map(|sample| sample.expect("observer propagation sample should exist"))
        .collect()
}

async fn measure_message_propagation_percentiles(
    testbed: &mut MultiNodeChannelUpdateTestbed,
    rounds: usize,
    wait_timeout: Duration,
) -> Vec<f64> {
    let mut latency_samples_ms = Vec::new();

    for _ in 0..rounds {
        let workload_idx = testbed.next_workload_idx;
        testbed.next_workload_idx = (testbed.next_workload_idx + 1) % testbed.workloads.len();

        let (
            updater_idx,
            channel_id,
            channel_outpoint,
            updater_pubkey,
            observer_indices,
            expected_fee,
        ) = {
            let workload = &mut testbed.workloads[workload_idx];
            workload.next_fee = workload.next_fee.saturating_add(1);
            (
                workload.updater_idx,
                workload.channel_id,
                workload.channel_outpoint.clone(),
                workload.updater_pubkey,
                workload.observer_indices.clone(),
                workload.next_fee,
            )
        };

        let t0 = Instant::now();
        testbed.nodes[updater_idx]
            .update_channel_with_command(channel_id, make_update_command(expected_fee))
            .await;

        let samples = collect_observer_propagation_latencies(
            &testbed.nodes,
            &observer_indices,
            &channel_outpoint,
            updater_pubkey,
            expected_fee,
            t0,
            wait_timeout,
        )
        .await;
        latency_samples_ms.extend(samples.into_iter().map(format_ms));
    }

    latency_samples_ms
}

fn print_message_propagation_summary(node_count: usize, rounds: usize, mut samples_ms: Vec<f64>) {
    if samples_ms.is_empty() {
        println!(
            "message_propagation_summary node_count={} rounds={} samples=0",
            node_count, rounds
        );
        return;
    }

    samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("finite latency samples"));
    let min_ms = samples_ms[0];
    let max_ms = *samples_ms.last().expect("non-empty samples");
    let avg_ms = samples_ms.iter().sum::<f64>() / samples_ms.len() as f64;
    let p50_ms = percentile(&samples_ms, 0.50);
    let p90_ms = percentile(&samples_ms, 0.90);
    let p99_ms = percentile(&samples_ms, 0.99);

    println!(
        "message_propagation_summary node_count={} rounds={} samples={} min_ms={:.2} avg_ms={:.2} p50_ms={:.2} p90_ms={:.2} p99_ms={:.2} max_ms={:.2}",
        node_count,
        rounds,
        samples_ms.len(),
        min_ms,
        avg_ms,
        p50_ms,
        p90_ms,
        p99_ms,
        max_ms
    );
}

fn bench_gossip_channel_update_propagation(c: &mut Criterion) {
    init_tracing();
    let rt = Runtime::new().expect("create runtime");
    let mut group = c.benchmark_group("gossip_channel_update_propagation_multi_node");

    let node_counts = bench_node_counts();
    let max_observers = max_observers_per_edge();
    let (gossip_network_interval_ms, gossip_store_interval_ms) = bench_gossip_intervals_ms();
    let wait_timeout = bench_wait_timeout();

    info!(
        ?node_counts,
        max_observers,
        gossip_network_interval_ms,
        gossip_store_interval_ms,
        ?wait_timeout,
        "configured multi-node gossip benchmark"
    );

    for node_count in node_counts {
        let mut testbed = rt.block_on(prepare_multi_node_channel_update_testbed(
            node_count,
            max_observers,
            gossip_network_interval_ms,
            gossip_store_interval_ms,
            wait_timeout,
        ));
        let rounds = propagation_rounds(testbed.workloads.len());
        let samples_ms = rt.block_on(measure_message_propagation_percentiles(
            &mut testbed,
            rounds,
            wait_timeout,
        ));
        print_message_propagation_summary(node_count, rounds, samples_ms);

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("update_to_observers", node_count), |b| {
            b.iter_custom(|iters| {
                rt.block_on(bench_multi_node_channel_update_propagation_iters(
                    &mut testbed,
                    iters,
                    wait_timeout,
                ))
            });
        });
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(30))
        .sample_size(10);
    targets = bench_gossip_channel_update_propagation
}
criterion_main!(benches);
