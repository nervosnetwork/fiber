#![cfg(feature = "bench")]
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fnn::{tests::create_n_nodes_network, MIN_RESERVED_CKB};
use std::collections::HashSet;
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

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_millis(1500))
        .measurement_time(std::time::Duration::from_secs(100))
        .sample_size(10);
    targets = bench_payment_path_finding
}
criterion_main!(benches);
