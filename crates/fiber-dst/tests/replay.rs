#![cfg(not(target_arch = "wasm32"))]

use std::process::{Command, Output};

use fiber_dst::Scenario;

fn check_output(output: &Output, case: &str, seed: &str) {
    assert!(
        output.status.success(),
        "{case} seed {seed} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

// The test executable is also a child process for fixed scenarios, so we can
// compare traces across process boundaries without exposing fixtures in the CLI.
#[test]
#[ignore = "subprocess worker for the replay tests"]
fn emit_fixture_trace() {
    use std::{env, fs};

    let Ok(case) = env::var("FIBER_DST_FIXTURE") else {
        return;
    };
    let seed = env::var("FIBER_DST_SEED")
        .expect("fixture seed")
        .parse()
        .expect("numeric seed");
    let path = env::var_os("FIBER_DST_TRACE").expect("trace path");
    let scenario = match case.as_str() {
        "watchtower-settlement" => Scenario::WatchtowerSettlement,
        "crash-before-ack" => Scenario::CrashBeforeAck,
        "two-node-faults" => Scenario::TwoNodeFaults,
        "swarm" => Scenario::Swarm,
        "faults" => Scenario::Faults,
        "multihop" => Scenario::Multihop,
        "mpp" => Scenario::Mpp,
        "funding-abort-recovery" => Scenario::FundingAbortRecovery,
        "parallel-funding" => Scenario::ParallelFunding { mixed: false },
        "parallel-funding-abort" => Scenario::ParallelFunding { mixed: true },
        _ => panic!("unknown replay fixture {case}"),
    };
    let trace = fiber_dst::run(scenario, seed).expect("run replay fixture");
    fs::write(path, trace.join("\n")).expect("write fixture trace");
}

fn run_fixture(case: &str, seed: &str) -> String {
    let directory = tempfile::tempdir().expect("fixture trace directory");
    let path = directory.path().join("trace");
    let output = Command::new(std::env::current_exe().expect("locate replay test executable"))
        .args(["--ignored", "--exact", "emit_fixture_trace"])
        .env("FIBER_DST_FIXTURE", case)
        .env("FIBER_DST_SEED", seed)
        .env("FIBER_DST_TRACE", &path)
        .env_remove("RUST_LOG")
        .output()
        .expect("start replay fixture");
    check_output(&output, case, seed);
    std::fs::read_to_string(path).expect("read fixture trace")
}

fn run_random(seed: &str, extra: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_fiber-dst"))
        .args(["random", seed])
        .args(extra)
        .env_remove("RUST_LOG")
        .output()
        .expect("start Fiber DST");
    check_output(&output, "random", seed);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).lines().next(),
        Some(format!("fiber-dst random {seed} {}", extra.join(" ")).as_str())
    );
    String::from_utf8(output.stdout).expect("UTF-8 trace")
}

#[test]
fn continuous_chaos_replays_funding_and_relay_crashes_peer_faults_and_force_close() {
    let options = ["--payments", "32", "--phases", "5", "--chaos"];
    let trace = run_random("6", &options);
    for marker in [
        "chaos startup starts",
        "chaos hit FundingPersisted",
        "chaos hit CommitmentAck",
        "relay=true",
        "dropped peer frame",
        "chaos end drop-frame hit=true",
        "continuous workload drained",
        "chaos force close confirmed both sides",
        "watchtower mailbox stalled",
        "Watchtower get_transactions Offline",
        "Watchtower send_transaction Timeout",
        "watchtowers completed settlement",
        "watchtower settled in-flight TLC",
        "watchtower mode BuiltIn",
        "random reconciled workload=32",
        "recovery_probes=4",
    ] {
        assert!(trace.contains(marker), "missing chaos milestone {marker}");
    }
    let (_, after_disconnect) = trace
        .split_once("chaos disconnect B->H")
        .expect("disconnect B and H during the payment workload");
    for (local, remote) in [("B", "H"), ("H", "B")] {
        let (_, disconnected) = after_disconnect
            .split_once(&format!("{local} disconnected from {remote}"))
            .expect("peer observes the disconnect");
        assert!(
            disconnected.contains(&format!("{local} connected to {remote}")),
            "{local} did not reconnect to {remote}"
        );
    }
    assert_eq!(trace, run_random("6", &options));
}

#[test]
fn random_replays_startup_funding_and_repeated_workload() {
    let options = ["--payments", "32", "--phases", "5"];
    let trace = run_random("1", &options);
    for marker in [
        "random topology",
        "random startup hit TCP Dial",
        "random startup recovered",
        "random funding core starts",
        "random MPP baseline drained",
        "random multihop baseline drained",
        "random funding expansion starts",
        "random wave 5/5 orders=6",
        "watchtower mode External",
        "external watchtower settled while",
        "random reconciled workload=32",
        "recovery_probes=4",
    ] {
        assert!(trace.contains(marker), "missing random milestone {marker}");
    }
    assert_eq!(trace.matches("ckb_fault=true").count(), 2);
    assert_eq!(trace.matches("swarm audit").count(), 40);
    assert!(trace.contains("faults_during_payment=true"));
    let replay = run_random("1", &options);
    assert_eq!(trace, replay);
}

#[test]
fn seeded_random_wave_replays_a_long_peer_delay() {
    let options = ["--payments", "12", "--phases", "3"];
    let trace = run_random("2", &options);
    assert!(
        trace
            .lines()
            .any(|line| line.contains("delaying peer frame to A AddTlc ")
                && line.ends_with(" 5700ms"))
    );
    assert!(trace.contains("swarm end delay-frame hit=true"));
    assert!(trace.contains("random reconciled workload=12 successful=12"));
    assert_eq!(trace, run_random("2", &options));
}

#[test]
fn crash_before_commitment_ack_replays_across_processes() {
    let trace = run_fixture("crash-before-ack", "42");
    assert!(trace.contains("before ACK"));
    assert!(trace.contains("B start 2"));
    assert_eq!(trace.matches(" Success ").count(), 2);
    assert_eq!(trace, run_fixture("crash-before-ack", "42"));
}

#[test]
fn two_node_faults_replay_across_processes() {
    let seed = "1729";
    let trace = run_fixture("two-node-faults", seed);
    assert!(trace.contains(" SQLite ") || trace.contains(" TCP ") || trace.contains(" CKB "));
    assert_eq!(trace.matches(" channel mailbox backlog=4 ").count(), 2);
    assert_eq!(trace.matches(" settled ").count(), 9);
    assert_eq!(trace, run_fixture("two-node-faults", seed));
}

#[test]
fn swarm_reconciles_bidirectional_mpp_and_multihop_across_processes() {
    let seed = "42";
    let trace = run_fixture("swarm", seed);
    for direction in ["A->D", "D->A"] {
        assert!(trace.lines().any(|line| {
            line.contains(&format!("swarm audit Mpp {direction}"))
                && line.contains("status=Some(Success) shards=2")
        }));
    }
    for direction in ["B->C", "C->B"] {
        assert!(trace.lines().any(|line| {
            line.contains(&format!("swarm audit Multihop {direction}"))
                && line.contains("status=Some(Success) shards=1")
        }));
    }
    assert!(trace.contains("faults_during_payment=true"));
    assert!(trace.contains("swarm repair"));
    assert!(trace
        .lines()
        .any(|line| { line.contains("swarm heal mailbox") && !line.ends_with("hits=0") }));
    assert!(trace.contains("B start 2"));
    assert_eq!(trace.matches("swarm audit").count(), 18);
    assert!(trace.contains("swarm reconciled all payments"));
    let replay = run_fixture("swarm", seed);
    assert_eq!(trace, replay);
}

#[test]
fn each_fault_hits_and_recovers() {
    let trace = run_fixture("faults", "42");
    for marker in [
        "CKB Actor",
        "CKB Client get_block_timestamp Slow",
        "CKB Client get_shutdown_tx Timeout",
        "SQLite fault WriteIo stopped Fiber",
        "SQLite fault DiskFull stopped Fiber",
        "SQLite fault SyncIo stopped Fiber",
        "hit TCP Dial",
        "hit TCP Read",
        "hit TCP Write",
        "bounded TCP writer queue full",
        "A delaying peer frame to B AddTlc ",
        "long peer frame delay kept payment",
        "B channel mailbox backlog=4 pending replies; local network and A channel responded",
        "B channel mailbox recovered",
    ] {
        assert!(trace.contains(marker), "missing fault receipt {marker}");
    }
    assert!(
        trace
            .lines()
            .any(|line| line.contains("delaying peer frame to B AddTlc ")
                && line.ends_with(" 7000ms"))
    );
    assert_eq!(trace, run_fixture("faults", "42"));
}

#[test]
fn multihop_replays_durable_relay_fulfillment() {
    let trace = run_fixture("multihop", "42");
    assert!(trace.contains("B persisted downstream fulfillment before relay"));
    assert!(trace.contains("relay_fee=100"));
    assert_eq!(trace, run_fixture("multihop", "42"));
}

#[test]
fn mpp_replays_across_two_paths_after_crash() {
    let trace = run_fixture("mpp", "42");
    assert!(trace.contains("C paused one MPP path"));
    assert!(trace.contains("MPP settled"));
    assert_eq!(trace, run_fixture("mpp", "42"));
}

#[test]
fn fully_aborted_funding_is_verified_and_retried() {
    let trace = run_fixture("funding-abort-recovery", "42");
    for marker in [
        "dropped one funding frame",
        "funding fully aborted",
        "retry fully aborted funding",
        "funding abort retry recovered",
    ] {
        assert!(
            trace.contains(marker),
            "missing funding abort marker {marker}"
        );
    }
    assert_eq!(trace.matches(" funding aborted ").count(), 2);
    assert_eq!(trace, run_fixture("funding-abort-recovery", "42"));
}

#[test]
fn parallel_funding_opens_six_channels_and_probes_each() {
    let trace = run_fixture("parallel-funding", "42");
    assert!(trace.contains("parallel funding overlap=6"));
    assert!(trace.contains("parallel funding reconciled attempts=6 online=6 aborted=0 probes=6"));
    assert_eq!(trace, run_fixture("parallel-funding", "42"));
}

#[test]
fn parallel_funding_abort_is_isolated_from_healthy_siblings() {
    let trace = run_fixture("parallel-funding-abort", "42");
    assert!(trace.contains("waiting before sibling offers"));
    assert!(trace.contains("parallel funding overlap=4"));
    assert!(trace.contains("pending with online siblings"));
    assert!(trace.contains("parallel funding one-sided abort"));
    assert!(trace.contains("parallel funding reconciled attempts=4 online=3 aborted=1 probes=3"));
    assert_eq!(trace, run_fixture("parallel-funding-abort", "42"));
}

#[test]
fn random_chaos_replays_parallel_funding_under_ckb_delay() {
    let options = ["--payments", "12", "--phases", "3", "--chaos"];
    let trace = run_random("8", &options);
    for marker in [
        "parallel funding G-A starts attempts=3 fault=slow-ckb",
        "parallel funding overlap=3",
        "CKB Actor Sign Slow",
        "parallel funding reconciled attempts=3 online=3 aborted=0 probes=3",
        "random reconciled workload=12",
    ] {
        assert!(
            trace.contains(marker),
            "missing parallel funding milestone {marker}"
        );
    }
    assert_eq!(trace, run_random("8", &options));
}

#[test]
fn watchtower_restarts_and_settles_a_committed_payment_on_chain() {
    let seed = "42";
    let trace = run_fixture("watchtower-settlement", seed);
    for marker in [
        "in-flight TLC committed",
        "B watchtower mailbox stalled",
        "B CKB Watchtower get_transactions Offline",
        "B watchtower stopped",
        "B CKB Watchtower send_transaction Timeout",
        "watchtower settled in-flight TLC",
    ] {
        assert!(
            trace.contains(marker),
            "missing watchtower milestone {marker}"
        );
    }
    assert_eq!(trace, run_fixture("watchtower-settlement", seed));
}

#[test]
fn external_watchtower_recovers_its_link_and_settles_while_fiber_is_offline() {
    let seed = "43";
    let trace = run_fixture("watchtower-settlement", seed);
    let mut remaining = trace.as_str();
    for marker in [
        "watchtower mode External",
        "partition B<->W-B external watchtower RPC",
        "B external watchtower RPC create_preimage failed during partition",
        "repair B<->W-B external watchtower RPC",
        "W-B external watchtower RPC recovered with preimage",
        "W-B watchtower mailbox stalled",
        "W-B CKB Watchtower get_transactions Offline",
        "W-B watchtower stopped",
        "W-B CKB Watchtower send_transaction Timeout",
        "crash B",
        "W-B external watchtower settled while B offline",
        "restart B",
        "watchtower settled in-flight TLC",
    ] {
        (_, remaining) = remaining
            .split_once(marker)
            .unwrap_or_else(|| panic!("missing or out-of-order milestone {marker}"));
    }
    assert_eq!(trace, run_fixture("watchtower-settlement", seed));
}
