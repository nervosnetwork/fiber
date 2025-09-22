use std::env;
use std::time::Duration;

use fiber_e2e_tests::{run_benchmark_test, run_integration_test, TestResult};

#[tokio::main]
async fn main() -> TestResult<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("🚀 Fiber High-Load Performance Testing Tool");
        println!("Examples:");
        println!("  cargo run --bin run-test benchmark");
        println!("  cargo run --bin run-test integration");
        return Ok(());
    }

    let test_type = &args[1];

    match test_type.as_str() {
        "integration" => {
            println!("🎯 Starting integration tests...");
            match run_integration_test().await {
                Ok(_) => println!("All integration tests passed!"),
                Err(e) => {
                    eprintln!("Integration test failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "benchmark" => {
            let duration = args[2].parse::<u64>().unwrap_or(60);
            let workers = args[3].parse::<usize>().unwrap_or(10);

            println!("🎯 Starting custom performance test...");
            println!("📊 Parameters: {} seconds, {} threads", duration, workers);
            run_benchmark_test(Duration::from_secs(duration), workers)?;
        }

        _ => {
            println!("❌ Unknown test type: {}", test_type);
            println!("Available types: integration, benchmark");
        }
    }
    Ok(())
}
