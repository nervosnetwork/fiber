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
        "funding-signature-replay" => Scenario::FundingSignatureReplay,
        "parallel-funding" => Scenario::ParallelFunding { mixed: false },
        "channel-id-rewrite-sync" => Scenario::ChannelIdRewrite {
            after_commit: false,
        },
        "channel-id-rewrite-crash" => Scenario::ChannelIdRewrite { after_commit: true },
        "parallel-funding-cleanup" => Scenario::ParallelFundingCleanup,
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
    assert!(trace.contains("cache and disk agree after ACK recovery"));
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
fn funding_reconnect_replays_saved_signatures_before_ready() {
    let case = "funding-signature-replay";
    let trace = run_fixture(case, "42");
    let dropped = trace
        .lines()
        .find(|line| line.contains("A dropped peer frame from B TxSignatures "))
        .expect("the final signatures were lost before A processed them");
    let (_, after_restart) = trace.split_once("A start 2").expect("A restarted");
    let (before_ready, _) = after_restart
        .split_once("A receive from B ChannelReadyV2 ")
        .expect("B replayed ChannelReady");
    let replayed = before_ready
        .lines()
        .find(|line| line.contains("A receive from B TxSignatures "))
        .expect("B replayed its signatures before ChannelReady");
    assert_eq!(
        dropped.split_whitespace().last(),
        replayed.split_whitespace().last()
    );
    assert!(trace.contains("funding signature replay recovered"));
    assert!(trace.contains(" Success "));
    assert_eq!(trace, run_fixture(case, "42"));
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
fn tx_abort_while_syncing_cleans_up_funding_and_allows_retry() {
    let trace = run_random("9", &["--payments", "12", "--phases", "3", "--chaos"]);
    let channel = "Hash256(0x361dcb3d5c0a965b8239b64850b16d8db5bbcb14348290d9fe2d705bf6a2061a)";
    let (before_abort, after_abort) = trace
        .split_once("C receive from D TxAbort ")
        .expect("C receives D's abort while reestablishing");
    assert!(before_abort.contains(&format!("D funding aborted {channel}")));
    assert!(after_abort.contains(&format!("C funding aborted {channel}")));
    assert!(after_abort.contains(&format!(
        "funding fully aborted C-D channel={channel} with no chain residue"
    )));
    let (_, retry) = after_abort
        .split_once(&format!(
            "retry fully aborted funding C-D channel={channel}"
        ))
        .expect("retry opening after both endpoints clean up");
    assert!(retry
        .lines()
        .any(|line| { line.contains("opened V2 channel C-D ") && !line.contains(channel) }));
    assert!(retry.contains("random reconciled workload=12"));
}

#[test]
fn random_workload_reconciles_a_force_close_and_times_out_the_onchain_tlc() {
    let options = ["--payments", "32", "--phases", "5", "--chaos"];
    let trace = run_random("14", &options);
    let channel = "Hash256(0xaf6235d802873849bd6e022432f4db58110a6d03e6365626f4904d827bf8fc05)";
    let payment = "0x28301a784c490cd9657113a93bb5b6b6501752c9df817dc126fabe20bbcc604e";
    assert!(trace.contains("advance virtual Unix clock"));
    for node in ["G", "A"] {
        let timeout = format!("swarm audited TLC timeout node={node}");
        assert!(trace.lines().any(|line| {
            line.contains(&timeout) && line.contains(channel) && line.contains(payment)
        }));
    }
    assert!(trace.contains(&format!("swarm audited close channel={channel}")));
    let audit = trace
        .lines()
        .find(|line| line.contains("swarm audit Direct A->G") && line.contains(payment))
        .expect("audit the payment whose TLC timed out on chain");
    assert!(
        audit.contains("status=Some(Failed) shards=0 delivered=0"),
        "{audit}"
    );
    assert!(trace.contains("random reconciled workload=32"));
    assert!(trace.contains("recovery_probes=4"));
    assert_eq!(trace, run_random("14", &options));
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
fn interrupted_channel_id_rewrite_is_atomic_and_recovers() {
    for case in ["channel-id-rewrite-sync", "channel-id-rewrite-crash"] {
        let trace = run_fixture(case, "42");
        assert!(trace.contains("ID rewrite after_commit="));
        assert!(trace.contains("ID rewrite recovered"));
        assert_eq!(trace, run_fixture(case, "42"));
    }
}

#[test]
fn interrupted_abort_cleanup_preserves_online_siblings() {
    let trace = run_fixture("parallel-funding-cleanup", "42");
    assert!(trace.contains("funding cleanup SQLite error"));
    assert!(trace.contains("parallel funding reconciled attempts=4 online=3 aborted=1 probes=3"));
    assert_eq!(trace, run_fixture("parallel-funding-cleanup", "42"));
}

#[test]
fn mpp_first_hop_rejection_leaves_no_undispatched_attempt() {
    let options = ["--payments", "12", "--phases", "3", "--chaos-payments"];
    let trace = run_random("3", &options);
    let payment = "0x2ff7b23acb22470a20fba6888cfc3e8c605030a7d2857d1c9ab410e32d854fbe";
    let audit = trace
        .lines()
        .find(|line| line.contains("swarm audit Mpp C->E") && line.contains(payment))
        .expect("audit the MPP whose first hop rejected its first shard");
    assert!(
        audit.contains("status=Some(Failed) shards=0 delivered=0 fee=0 attempts=1"),
        "{audit}"
    );
    assert!(trace.contains("random reconciled workload=12"));
    assert_eq!(trace, run_random("3", &options));
}

#[test]
fn receiver_restart_expires_an_incomplete_mpp_hold() {
    let trace = run_random(
        "13661109889927125842",
        &["--payments", "128", "--phases", "8"],
    );
    let payment = "0x47a8fc7c101e867aed09efdddce18fd880d492e1291a260fbae8d5765632c002";
    let shard_received = trace
        .lines()
        .find(|line| line.contains("D receive from F AddTlc") && line.contains(payment))
        .expect("receiver gets the MPP shard before crashing");
    let (_, after_receive) = trace
        .split_once(shard_received)
        .expect("trace after receipt");
    let (_, after_crash) = after_receive
        .split_once(" crash D\n")
        .expect("receiver crashes");
    let (_, after_restart) = after_crash
        .split_once(" restart D\n")
        .expect("receiver restarts");
    let audit = after_restart
        .lines()
        .find(|line| line.contains("swarm audit Mpp G->D") && line.contains(payment))
        .expect("audit the interrupted MPP after both hops drain");
    assert!(
        audit.contains("status=Some(Failed) shards=0 delivered=0"),
        "{audit}"
    );
    assert!(trace.contains("random reconciled workload=128"));
}

#[test]
fn random_payment_chaos_replays_a_targeted_sqlite_write_error() {
    let options = ["--payments", "12", "--phases", "3", "--chaos-payments"];
    let trace = run_random("9", &options);
    let mut remaining = trace.as_str();
    for marker in [
        "chaos arm sqlite-batch F WriteIo",
        "F SQLite fault WriteIo stopped Fiber",
        "chaos end sqlite-batch hit=true",
        "F start 2",
        "random reconciled workload=12",
    ] {
        (_, remaining) = remaining
            .split_once(marker)
            .unwrap_or_else(|| panic!("missing or out-of-order SQLite milestone {marker}"));
    }
    assert_eq!(trace, run_random("9", &options));
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
