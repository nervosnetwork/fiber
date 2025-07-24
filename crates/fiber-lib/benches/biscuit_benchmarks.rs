#![cfg(feature = "bench")]
use biscuit_auth::{macros::biscuit, KeyPair};
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, BenchmarkId, Criterion,
    Throughput,
};
use fnn::rpc::biscuit::BiscuitAuth;
use ractor::concurrency::Duration;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn bench_group(group: &mut BenchmarkGroup<'_, WallTime>, auth_ok: bool) {
    for requests in [100, 500, 1000, 2000] {
        // Add throughput measurement based on requests
        group.throughput(Throughput::Elements(requests as u64));

        group.bench_function(BenchmarkId::new("auth", requests), |b| {
            b.iter_custom(|iters| {
                let root = KeyPair::new();
                let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

                // sign a biscuit with timeout
                let token = {
                    let biscuit = biscuit!(
                        r#"
                write("payments");
                write("channels");
                read("channels");
                read("payments");
                check if time($time), $time <= 2022-03-30T20:00:00Z;
    "#
                    )
                    .build(&root)
                    .unwrap();

                    biscuit.to_base64().unwrap()
                };

                // Start timing
                let start = std::time::Instant::now();

                // Run the benchmark iterations
                for _ in 0..iters {
                    for _ in 0..requests {
                        // check permission

                        // get time is a part of server logic, so we calculate time per
                        // request
                        let time = if auth_ok {
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64
                        } else {
                            Duration::from_millis(10).as_millis() as u64
                        };
                        assert_eq!(
                            auth.check_permission_with_time(
                                "send_payment",
                                json!({}),
                                &token,
                                time
                            )
                            .is_err(),
                            auth_ok
                        );
                    }
                }

                // Return elapsed time
                start.elapsed()
            });
        });
    }
}

fn bench_biscuit_auth(c: &mut Criterion) {
    // Create a benchmark group with minimal/no warmup
    let mut group = c.benchmark_group("biscuit-auth pass");
    bench_group(&mut group, true);
    group.finish();

    let mut group = c.benchmark_group("biscuit-auth token timeout");
    bench_group(&mut group, false);
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_millis(500))
        .measurement_time(std::time::Duration::from_secs(40)) // Increased from default 5s to 25s
        .sample_size(10);
    targets = bench_biscuit_auth
}
criterion_main!(benches);
