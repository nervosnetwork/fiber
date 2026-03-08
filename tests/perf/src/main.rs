use std::env;
use std::time::Duration;

use fiber_e2e_tests::{
    compare_gossip_with_baseline, compare_with_baseline, load_benchmark_baseline,
    load_gossip_benchmark_baseline, run_benchmark_test, run_gossip_benchmark_test,
    run_integration_test, save_benchmark_baseline, save_gossip_benchmark_baseline, TestResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GossipLoadMode {
    Steady,
    Burst,
}

impl GossipLoadMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Burst => "burst",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Base,
    Compare,
}

impl RunMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Compare => "compare",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "base" => Ok(Self::Base),
            "compare" => Ok(Self::Compare),
            _ => Err(format!(
                "invalid --result-mode '{value}', expected 'base' or 'compare'"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GossipBenchmarkCliArgs {
    load_mode: GossipLoadMode,
    duration_secs: u64,
    run_mode: RunMode,
    interval_ms: u64,
    burst_updates_per_window: u64,
    burst_window_ms: u64,
}

fn parse_u64_flag(value: &str, flag: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid value '{value}' for {flag}, expected unsigned integer"))
}

fn print_gossip_benchmark_usage() {
    eprintln!("Usage:");
    eprintln!(
        "  cargo run --bin run-test gossip-benchmark steady [--duration-secs <secs>] [--interval-ms <ms>] [--result-mode base|compare]"
    );
    eprintln!(
        "  cargo run --bin run-test gossip-benchmark burst [--duration-secs <secs>] --burst-updates-per-window <n> [--burst-window-ms <ms>] [--result-mode base|compare]"
    );
    eprintln!("Examples:");
    eprintln!(
        "  cargo run --bin run-test gossip-benchmark steady --duration-secs 60 --interval-ms 50 --result-mode base"
    );
    eprintln!(
        "  cargo run --bin run-test gossip-benchmark burst --duration-secs 60 --burst-updates-per-window 200 --burst-window-ms 1000 --result-mode compare"
    );
}

fn parse_gossip_benchmark_args(args: &[String]) -> Result<GossipBenchmarkCliArgs, String> {
    if args.is_empty() {
        return Err("missing load mode, expected 'steady' or 'burst'".to_string());
    }

    let load_mode = match args[0].as_str() {
        "steady" => GossipLoadMode::Steady,
        "burst" => GossipLoadMode::Burst,
        other => {
            return Err(format!(
                "unknown load mode '{other}', expected 'steady' or 'burst'"
            ));
        }
    };

    let mut duration_secs = 60u64;
    let mut run_mode = RunMode::Base;
    let mut interval_ms = 1000u64;
    let mut burst_updates_per_window = 0u64;
    let mut burst_window_ms = 1000u64;

    let mut interval_flag_set = false;
    let mut burst_updates_flag_set = false;
    let mut burst_window_flag_set = false;

    let mut index = 1usize;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--help" || flag == "-h" {
            return Err("help requested".to_string());
        }
        let Some(value) = args.get(index + 1) else {
            return Err(format!("missing value for {flag}"));
        };
        match flag {
            "--duration-secs" => {
                duration_secs = parse_u64_flag(value, flag)?;
            }
            "--result-mode" => {
                run_mode = RunMode::parse(value)?;
            }
            "--interval-ms" => {
                interval_ms = parse_u64_flag(value, flag)?;
                interval_flag_set = true;
            }
            "--burst-updates-per-window" => {
                burst_updates_per_window = parse_u64_flag(value, flag)?;
                burst_updates_flag_set = true;
            }
            "--burst-window-ms" => {
                burst_window_ms = parse_u64_flag(value, flag)?;
                burst_window_flag_set = true;
            }
            _ => return Err(format!("unknown flag '{flag}'")),
        }
        index = index.saturating_add(2);
    }

    if duration_secs == 0 {
        return Err("--duration-secs must be > 0".to_string());
    }

    match load_mode {
        GossipLoadMode::Steady => {
            if interval_ms == 0 {
                return Err("--interval-ms must be > 0 in steady mode".to_string());
            }
            if burst_updates_flag_set || burst_window_flag_set {
                return Err(
                    "steady mode does not accept --burst-updates-per-window/--burst-window-ms"
                        .to_string(),
                );
            }
        }
        GossipLoadMode::Burst => {
            if interval_flag_set {
                return Err("burst mode does not accept --interval-ms".to_string());
            }
            if !burst_updates_flag_set {
                return Err("burst mode requires --burst-updates-per-window".to_string());
            }
            if burst_updates_per_window == 0 {
                return Err("--burst-updates-per-window must be > 0".to_string());
            }
            if burst_window_ms == 0 {
                return Err("--burst-window-ms must be > 0".to_string());
            }
            interval_ms = 0;
        }
    }

    Ok(GossipBenchmarkCliArgs {
        load_mode,
        duration_secs,
        run_mode,
        interval_ms,
        burst_updates_per_window,
        burst_window_ms,
    })
}

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
        println!(
            "  cargo run --bin run-test gossip-benchmark steady --duration-secs 60 --interval-ms 50 --result-mode base"
        );
        println!(
            "  cargo run --bin run-test gossip-benchmark burst --duration-secs 60 --burst-updates-per-window 200 --burst-window-ms 1000 --result-mode compare"
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
        "gossip-benchmark" => {
            let gossip_args = match parse_gossip_benchmark_args(&args[2..]) {
                Ok(parsed) => parsed,
                Err(error) => {
                    if error != "help requested" {
                        eprintln!("❌ {}", error);
                    }
                    print_gossip_benchmark_usage();
                    std::process::exit(1);
                }
            };

            println!("🎯 Starting gossip benchmark test...");
            println!(
                "📊 Parameters: load_mode={}, duration={}s, interval={}ms, burst={} updates/{}ms, result_mode={}",
                gossip_args.load_mode.as_str(),
                gossip_args.duration_secs,
                gossip_args.interval_ms,
                gossip_args.burst_updates_per_window,
                gossip_args.burst_window_ms,
                gossip_args.run_mode.as_str()
            );

            let benchmark_result = run_gossip_benchmark_test(
                Duration::from_secs(gossip_args.duration_secs),
                Duration::from_millis(gossip_args.interval_ms),
                gossip_args.burst_updates_per_window,
                Duration::from_millis(gossip_args.burst_window_ms),
            )
            .await?;

            match gossip_args.run_mode {
                RunMode::Base => {
                    save_gossip_benchmark_baseline(&benchmark_result, "gossip_baseline.json")?;
                    println!("💾 Gossip baseline benchmark results saved to gossip_baseline.json");
                }
                RunMode::Compare => {
                    save_gossip_benchmark_baseline(&benchmark_result, "gossip_current.json")?;
                    match load_gossip_benchmark_baseline("gossip_baseline.json") {
                        Ok(baseline) => {
                            match compare_gossip_with_baseline(
                                &benchmark_result,
                                &baseline,
                                "gossip_comparison_report.txt",
                            ) {
                                Ok(_) => println!("📊 Gossip comparison completed successfully"),
                                Err(e) => {
                                    eprintln!("❌ Failed to save gossip comparison report: {}", e)
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("❌ Failed to load gossip_baseline.json: {}", e);
                            eprintln!("💡 Run with 'base' mode first to create a gossip baseline");
                            std::process::exit(1);
                        }
                    }
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
            print_gossip_benchmark_usage();
            std::process::exit(1);
        }
    }
    Ok(())
}
