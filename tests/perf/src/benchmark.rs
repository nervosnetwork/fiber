use fiber_e2e_tests::{run_benchmark_test, run_integration_test, TestResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> TestResult<()> {
    let args: Vec<String> = std::env::args().collect();

    let (duration, workers) = if args.len() >= 3 {
        let duration_secs: u64 = args[1].parse().unwrap_or(60);
        let worker_count: usize = args[2].parse().unwrap_or(6);
        (Duration::from_secs(duration_secs), worker_count)
    } else {
        println!("Usage: {} <duration_seconds> <worker_count>", args[0]);
        println!("Using defaults: 60 seconds (1 minutes), 6 workers");
        (Duration::from_secs(60), 6)
    };

    println!("🔥 Starting Fiber Payment Benchmark");
    println!("Duration: {} seconds", duration.as_secs());
    println!("Workers: {}", workers);
    println!(
        "Target: {} workers running for {} minutes",
        workers,
        duration.as_secs() / 60
    );

    match run_integration_test().await {
        Ok(_) => println!("All integration tests passed!"),
        Err(e) => {
            eprintln!("Integration test failed: {}", e);
            std::process::exit(1);
        }
    }

    run_benchmark_test(duration, workers).await
}
