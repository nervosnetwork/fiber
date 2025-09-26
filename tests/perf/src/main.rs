use std::env;
use std::time::Duration;

use fiber_e2e_tests::{
    compare_with_baseline, load_benchmark_baseline, run_benchmark_test, run_integration_test,
    save_benchmark_baseline, TestResult,
};

#[tokio::main]
async fn main() -> TestResult<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("🚀 Fiber High-Load Performance Testing Tool");
        println!("Examples:");
        println!("  cargo run --bin run-test benchmark 60 10 base     # Run 60s with 10 threads and save baseline");
        println!(
            "  cargo run --bin run-test benchmark 60 10 compare  # Run and compare with baseline"
        );
        println!("  cargo run --bin run-test integration              # Run integration tests");
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
            let duration = args
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            let workers = args
                .get(3)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(10);
            let run_mode = args.get(4).cloned().unwrap_or_else(|| "base".to_string());

            println!("🎯 Starting custom performance test...");
            println!("📊 Parameters: {} seconds, {} threads", duration, workers);

            let benchmark_result = run_benchmark_test(Duration::from_secs(duration), workers)?;

            match run_mode.as_str() {
                "base" => {
                    // Save baseline
                    save_benchmark_baseline(&benchmark_result, "baseline.json")?;
                    println!("💾 Baseline benchmark results saved to baseline.json");
                }
                "compare" => {
                    save_benchmark_baseline(&benchmark_result, "current.json")?;
                    // Load baseline and compare
                    match load_benchmark_baseline("baseline.json") {
                        Ok(baseline) => {
                            match compare_with_baseline(
                                &benchmark_result,
                                &baseline,
                                "comparison_report.txt",
                            ) {
                                Ok(_) => println!("📊 Comparison completed successfully"),
                                Err(e) => eprintln!("❌ Failed to save comparison report: {}", e),
                            }
                        }
                        Err(e) => {
                            eprintln!("❌ Failed to load baseline.json: {}", e);
                            eprintln!("💡 Run with 'base' mode first to create a baseline");
                            std::process::exit(1);
                        }
                    }
                }
                _ => {
                    eprintln!("❌ Invalid run mode: {}", run_mode);
                    eprintln!("Available modes: base, compare");
                    std::process::exit(1);
                }
            }
        }

        _ => {
            println!("❌ Unknown test type: {}", test_type);
            println!("Available types:");
            println!("  integration - Run integration tests");
            println!("  benchmark <duration> <workers> <mode> - Run performance benchmark");
            println!("    - duration: test duration in seconds (default: 60)");
            println!("    - workers: number of worker threads (default: 10)");
            println!("    - mode: 'base' to save baseline, 'compare' to compare with baseline (default: base)");
            std::process::exit(1);
        }
    }
    Ok(())
}
