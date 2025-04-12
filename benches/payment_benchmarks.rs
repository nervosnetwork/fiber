#![cfg(feature = "bench")]

use ckb_types::packed::OutPoint;
use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use fnn::tests::{create_n_nodes_network, HUGE_CKB_AMOUNT};
use std::collections::{HashMap, HashSet};
use tokio::runtime::Runtime;

fn bench_payment_path_finding(c: &mut Criterion) {
    // Create a runtime for async functions
    let rt = Runtime::new().unwrap();

    // Create a benchmark group with minimal/no warmup
    let mut group = c.benchmark_group("payment_path_finding");

    // Configure the group to use minimal/no warmup
    group.warm_up_time(std::time::Duration::from_millis(1)); // Minimal warmup time
    group.sample_size(10); // Reduce number of samples
    group.sampling_mode(SamplingMode::Flat); // Use flat sampling mode

    for num_channels in [2, 4, 8, 16] {
        // Add throughput measurement based on the number of paths
        group.throughput(Throughput::Elements(num_channels as u64));

        group.bench_function(BenchmarkId::new("find_path", num_channels), |b| {
            b.iter_custom(|iters| {
                rt.block_on(async move {
                    // Setup the network with varying number of channels between node 1 and node 2
                    let mut channel_configs = Vec::new();

                    // Always add the first connection from node 0 to node 1
                    channel_configs.push(((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)));

                    // Add multiple channels between node 1 and node 2
                    for _ in 0..num_channels {
                        channel_configs.push(((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)));
                    }

                    // Add the final connection from node 2 to node 3
                    channel_configs.push(((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)));

                    // Create the network
                    let (nodes, channels) = create_n_nodes_network(&channel_configs, 4).await;

                    // Start timing
                    let start = std::time::Instant::now();

                    // Run the benchmark iterations
                    for _ in 0..iters {
                        let mut payments = HashSet::new();
                        let mut channel_stats_map = HashMap::new();

                        for _i in 0..5 {
                            let payment_amount = 10;
                            let res = nodes[0]
                                .send_payment_keysend(&nodes[3], payment_amount, false)
                                .await
                                .unwrap();

                            let payment_hash = res.payment_hash;
                            let second_hop_channel = res.router.nodes[1].channel_outpoint.clone();
                            channel_stats_map
                                .entry(second_hop_channel)
                                .and_modify(|e| *e += 1)
                                .or_insert(1);

                            payments.insert(payment_hash);
                        }

                        // Verify that path finding tried multiple channels
                        for channel in &channels[1..channels.len() - num_channels] {
                            let funding_tx = nodes[0].get_channel_funding_tx(channel).unwrap();
                            let channel_outpoint = OutPoint::new(funding_tx.into(), 0);

                            let _tried_count =
                                channel_stats_map.get(&channel_outpoint).unwrap_or(&0);
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

fn bench_payment_throughput(c: &mut Criterion) {
    // Create a runtime for async functions
    let rt = Runtime::new().unwrap();

    // Create a benchmark group with minimal/no warmup
    let mut group = c.benchmark_group("payment_throughput");

    // Configure the group to use minimal/no warmup
    group.warm_up_time(std::time::Duration::from_millis(100)); // Minimal warmup time
    group.sample_size(10); // Reduce number of samples
    group.sampling_mode(SamplingMode::Flat); // Use flat sampling mode

    for concurrent_payments in [1, 5] {
        // Add throughput measurement based on number of concurrent payments
        group.throughput(Throughput::Elements(concurrent_payments));

        group.bench_function(
            BenchmarkId::new("concurrent_payments", concurrent_payments),
            |b| {
                b.iter_custom(|iters| {
                    rt.block_on(async move {
                        // Set up a simple network with a direct path
                        let channel_configs = vec![
                            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
                            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
                        ];

                        let (nodes, _) = create_n_nodes_network(&channel_configs, 3).await;

                        // Start timing
                        let start = std::time::Instant::now();

                        // Run the benchmark iterations
                        for _ in 0..iters {
                            let mut payments = vec![];

                            // Send payments concurrently
                            for _ in 0..concurrent_payments {
                                let payment_result = nodes[0]
                                    .send_payment_keysend(&nodes[2], 1000, false)
                                    .await
                                    .unwrap();

                                payments.push(payment_result.payment_hash);
                            }

                            // Wait for all payments to complete
                            for payment_hash in payments {
                                nodes[0].wait_until_success(payment_hash).await;
                            }
                        }

                        // Return elapsed time
                        start.elapsed()
                    })
                });
            },
        );
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_millis(100)) // Global minimal warmup
        .sample_size(10) // Reduce sample size globally
        .without_plots() // Skip plot generation to speed things up
        .noise_threshold(0.05); // Lower precision for faster results
    targets = bench_payment_path_finding, bench_payment_throughput
}
criterion_main!(benches);
