use std::cell::Cell;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{FiberActorCommand, Hash256};
use fnn::simulation;
use tokio::sync::oneshot;

use crate::harness::{Harness, HarnessOptions, LspSetup, EPOCH, TICK};
use crate::host::Trace;
use crate::tower::TowerMode;

mod adversarial;
mod byzantine;
mod dos;
mod exhaustion;
mod faults;
mod funding;
mod fuzz;
mod liveness;
mod lsp;
mod onchain;
mod routes;
mod swarm;
mod two_node;
mod udt;

const PHASE_TICKS: usize = 6_000;

thread_local! {
    static CLOCK_OFFSET_MS: Cell<u64> = const { Cell::new(0) };
    static DRIVER_ELAPSED_MS: Cell<u64> = const { Cell::new(0) };
}
const FUNDING_AMOUNT: u128 = 100_000 * 100_000_000;

/// Fault scheduling policy for random exploration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandomFaultPolicy {
    /// Use the original payment-wave windows.
    Windows,
    /// Use one continuous schedule through boot, funding, and payments.
    Continuous,
    /// Fund the topology first, then run the continuous payment schedule.
    Payments,
}

/// The scenario whose execution the seed controls.
#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    /// Force close a CKB channel and execute settlement through the real watchtower.
    WatchtowerSettlement,
    /// Supersede a round's CommitmentSignedV2 variant, force close, and settle via watchtower.
    WatchtowerSuperseded,
    /// Crash the receiver after it commits an incoming V2 commitment, before it sends the ACK.
    CrashBeforeAck,
    /// Exercise faults and recovery during serial payments across a two-node channel.
    TwoNodeFaults,
    /// Run concurrent bidirectional payments under independently scheduled faults.
    Swarm,
    /// Exercise seeded startup, funding, repeated payment waves, and recovery.
    Random {
        /// Number of payment orders in the workload, excluding probes.
        payments: usize,
        /// Number of workload waves across which to spread the payment orders.
        phases: usize,
        /// Policy controlling when a run can inject faults.
        fault_policy: RandomFaultPolicy,
        /// Whether to co-host an LSP service and external `fiber-lsp-sdk` wallet hosts in the mesh.
        lsp: bool,
    },
    /// Exercise every fault adapter against real nodes.
    Faults,
    /// Crash a forwarding node with a persisted downstream fulfillment.
    Multihop,
    /// Recover a two-path payment after one of its relay nodes crashes.
    Mpp,
    /// Drop one funding frame, verify a clean two-sided abort, and retry.
    FundingAbortRecovery,
    /// Reconnect after one endpoint misses the final funding signatures.
    FundingSignatureReplay,
    /// Interrupt an opening during or immediately after its durable channel ID rewrite.
    ChannelIdRewrite {
        /// Crash after commit before the notification, or inject an SQLite sync error.
        after_commit: bool,
    },
    /// Interrupt deletion of one aborted funding attempt while sibling channels are active.
    ParallelFundingCleanup,
    /// Open multiple channels on one peer link and verify each funding outcome.
    ParallelFunding {
        /// Drop a funding update for one attempt and check abort isolation.
        mixed: bool,
    },
    /// Buffer hosted LSP payments while an external SDK wallet is offline, restart LSP,
    /// and verify both recovery and long-offline buffer expiry.
    LspOfflineDelivery,
    /// Inject abrupt crashes, dropped responses, duplicate/conflicting signatures, and
    /// long-offline force-close self-funded watchtower settlement against an external SDK wallet.
    LspSignerFaults,
    /// Race two full-value payers on distinct channels against a single-path immediate invoice.
    SinglePathDoublePay,
    /// Exercise SigningPolicy::Manual confirmation dismissal, Byzantine commitment tampering,
    /// and mixed batch fulfill+refund commitments against an external SDK wallet.
    LspSignerAdversarial,
    /// Exercise force-close micro-boundaries: PeerFailRecordedBeforeAck and RemoveConfirmedBeforeRelay.
    ForceCloseAdversarial,
    /// Exercise multi-hop UDT channels, relay crash at ForwardFulfill, and UDT hold-invoice cancellation.
    UdtMultihop,
    /// Exercise peer blackholes, malicious delays, and autonomous timeout/expiry force-closes.
    LivenessHangAndAutoForceClose,
    /// Exercise permanent SDK refusal during open/TLC updates, TxAbort while awaiting signature, and auto force-close.
    SdkPermanentRefusalAndEscape,
    /// Exercise hung cooperative close escalation to force-close (both P2P and SDK refusal) and close races.
    ShutdownEscalationAndRace,
    /// Exercise P2P frame/DoS floods (oversized, malformed, duplicate, OpenChannelV2, TLC/gossip) with sibling non-starvation.
    PeerProtocolDosAndFlood,
    /// Exercise LSP JSON-RPC floods (malformed, oversized, revoked Biscuit, replayed) and offline buffer DoS with non-starvation.
    LspRpcAndBufferDos,
    /// Exercise revoked stale commitment broadcast (watchtower penalty sweep) and full Byzantine LSP/watchtower tampering.
    ByzantineStaleAndTamper,
    /// Exercise aggressive exhaustion & race probes (coop-vs-force close race, reconnect timeout reset, DB write amplification, global pending open exhaustion).
    AdversarialExhaustionAndRaces,
    /// Exercise LSP exhaustion & tamper probes (opening/closing/watchtower-extra-input tampers, slow signer deadline race, SendClosingSigned refusal eviction starvation).
    LspAdversarialExhaustion,
    /// Exercise seeded stateful malicious-peer protocol fuzzer across all 5 channel lifecycle states.
    ProtocolStateMachineFuzz,
}

/// Exercise real Fiber actors and return the logical trace for cross-process replay.
/// Failures include the seed and the events preceding the failed assertion.
pub fn run(scenario: Scenario, seed: u64) -> Result<Vec<String>> {
    let trace = Trace::default();
    let result: Result<()> = crate::oracle::with_error_oracle(|error_oracle| {
        let disk_faults_armed = matches!(
            scenario,
            Scenario::Faults
                | Scenario::Random { .. }
                | Scenario::ChannelIdRewrite {
                    after_commit: false
                }
                | Scenario::ParallelFundingCleanup
                | Scenario::LspSignerFaults
        );
        let run_result = simulation::run(seed ^ 0x0066_6962_6572, virtual_time, || {
            fiber_lsp_sdk::with_simulation_rng(simulation::fill_random, || {
                struct ResetClock;
                impl Drop for ResetClock {
                    fn drop(&mut self) {
                        CLOCK_OFFSET_MS.set(0);
                        DRIVER_ELAPSED_MS.set(0);
                    }
                }
                let _clock = ResetClock;
                let directory = tempfile::tempdir()?;
                let topology = if let Scenario::Random {
                    payments, phases, ..
                } = scenario
                {
                    ensure!(
                        phases > 0 && phases <= payments,
                        "random requires 1 <= phases <= payments"
                    );
                    Some(swarm::random::Topology::generate(seed))
                } else {
                    None
                };
                let edges: &[(usize, usize)] = match scenario {
                    Scenario::Multihop
                    | Scenario::UdtMultihop
                    | Scenario::LivenessHangAndAutoForceClose
                    | Scenario::PeerProtocolDosAndFlood
                    | Scenario::ProtocolStateMachineFuzz => &[(0, 1), (1, 2)],
                    Scenario::AdversarialExhaustionAndRaces => {
                        &[(0, 1), (2, 1), (3, 1), (4, 1), (5, 1), (6, 1)]
                    }
                    Scenario::SinglePathDoublePay => &[(0, 2), (1, 2)],
                    Scenario::ByzantineStaleAndTamper => &[(0, 1), (0, 2)],
                    Scenario::Mpp | Scenario::Swarm | Scenario::ForceCloseAdversarial => {
                        &[(0, 1), (0, 2), (1, 3), (2, 3)]
                    }
                    Scenario::Random { .. } => &topology.as_ref().expect("random topology").links,
                    _ => &[(0, 1)],
                };
                let is_lsp_fixture = matches!(
                    scenario,
                    Scenario::LspOfflineDelivery
                        | Scenario::LspSignerFaults
                        | Scenario::LspSignerAdversarial
                        | Scenario::SdkPermanentRefusalAndEscape
                        | Scenario::ShutdownEscalationAndRace
                        | Scenario::LspRpcAndBufferDos
                        | Scenario::ByzantineStaleAndTamper
                        | Scenario::LspAdversarialExhaustion
                );
                let random_has_lsp = matches!(scenario, Scenario::Random { lsp: true, .. });
                let has_lsp = is_lsp_fixture || random_has_lsp;
                // Random exploration has a watchdog per stage, including the protocol's MPP hold timer.
                let duration = if topology.is_some() {
                    Duration::MAX
                } else {
                    Duration::from_secs(900)
                };
                let continuous = matches!(scenario, Scenario::Random { fault_policy, .. }
                if fault_policy != RandomFaultPolicy::Windows);
                let tower_mode = if is_lsp_fixture
                    || matches!(
                        scenario,
                        Scenario::ForceCloseAdversarial
                            | Scenario::LivenessHangAndAutoForceClose
                            | Scenario::PeerProtocolDosAndFlood
                            | Scenario::AdversarialExhaustionAndRaces
                            | Scenario::ProtocolStateMachineFuzz
                    ) {
                    Some(TowerMode::BuiltIn)
                } else {
                    (topology.is_some()
                        || matches!(
                            scenario,
                            Scenario::WatchtowerSettlement | Scenario::WatchtowerSuperseded
                        ))
                    .then_some(if seed.is_multiple_of(2) {
                        TowerMode::BuiltIn
                    } else {
                        TowerMode::External
                    })
                };
                let lsp_multi_wallet = random_has_lsp
                    || matches!(
                        scenario,
                        Scenario::LspRpcAndBufferDos | Scenario::LspAdversarialExhaustion
                    );
                let lsp = has_lsp.then(|| {
                    let lsp_node = if let Some(topo) = topology.as_ref() {
                        topo.lsp_node(seed)
                    } else {
                        1
                    };
                    let mut config = Driver::lsp_config_for_scenario();
                    if lsp_multi_wallet {
                        // 3 wallets competing for 2 active tenant slots exercises capacity-bound
                        // eviction and cold/hot runtime churn.
                        config.max_active_tenants = 2;
                    }
                    LspSetup {
                        lsp_node,
                        wallet_count: if lsp_multi_wallet { 3 } else { 1 },
                        tenant_liquidity: FUNDING_AMOUNT,
                        config,
                    }
                });
                let runtime = Harness::new(
                    seed,
                    directory.path(),
                    trace.clone(),
                    edges,
                    HarnessOptions {
                        duration,
                        // Continuous reconnects and DoS floods need headroom in Turmoil's TCP queue.
                        tcp_capacity: if matches!(
                            scenario,
                            Scenario::PeerProtocolDosAndFlood
                                | Scenario::LspRpcAndBufferDos
                                | Scenario::AdversarialExhaustionAndRaces
                                | Scenario::LspAdversarialExhaustion
                                | Scenario::ProtocolStateMachineFuzz
                        ) {
                            Some(1024)
                        } else {
                            (continuous || has_lsp).then_some(512)
                        },
                        tower_mode,
                        // A peer that misses the terminal abort frame follows its funding timer.
                        funding_timeout_seconds: match scenario {
                            Scenario::ChannelIdRewrite { .. }
                            | Scenario::ParallelFundingCleanup => Some(1),
                            Scenario::Random {
                                fault_policy: RandomFaultPolicy::Continuous,
                                ..
                            } => Some(10),
                            _ => None,
                        },
                        lsp,
                    },
                )?;
                let mut driver = Driver {
                    runtime,
                    chaos: None,
                    coverage: crate::oracle::CoverageTracker::default(),
                };
                driver.runtime.note(format_args!(
                    "fiber={} seed={seed} scenario={scenario:?}",
                    fnn::get_git_version()
                ));
                match scenario {
                    Scenario::WatchtowerSettlement => driver.watchtower_settlement(),
                    Scenario::WatchtowerSuperseded => driver.watchtower_superseded(),
                    Scenario::Faults => driver.faults(),
                    Scenario::Multihop => driver.multihop(),
                    Scenario::Mpp => driver.mpp(seed),
                    Scenario::Swarm => driver.swarm(seed, edges),
                    Scenario::Random {
                        payments,
                        phases,
                        fault_policy,
                        lsp,
                    } => driver.random(
                        seed,
                        topology.as_ref().expect("random topology"),
                        payments,
                        phases,
                        fault_policy,
                        lsp,
                    ),
                    Scenario::FundingAbortRecovery => driver.funding_abort_recovery(),
                    Scenario::FundingSignatureReplay => driver.funding_signature_replay(),
                    Scenario::ChannelIdRewrite { after_commit } => {
                        driver.channel_id_rewrite(after_commit)
                    }
                    Scenario::ParallelFundingCleanup => {
                        driver.parallel_funding_fixture(funding::BatchFault::AbortCleanup(1))
                    }
                    Scenario::ParallelFunding { mixed } => {
                        driver.parallel_funding_fixture(if mixed {
                            funding::BatchFault::DropUpdate(1)
                        } else {
                            funding::BatchFault::None
                        })
                    }
                    Scenario::LspOfflineDelivery => driver.lsp_offline_delivery(),
                    Scenario::LspSignerFaults => driver.lsp_signer_faults(),
                    Scenario::SinglePathDoublePay => driver.single_path_double_pay(),
                    Scenario::LspSignerAdversarial => driver.lsp_signer_adversarial(),
                    Scenario::ForceCloseAdversarial => driver.force_close_adversarial(),
                    Scenario::UdtMultihop => driver.udt_multihop(seed),
                    Scenario::LivenessHangAndAutoForceClose => {
                        driver.liveness_hang_and_auto_force_close()
                    }
                    Scenario::SdkPermanentRefusalAndEscape => {
                        driver.sdk_permanent_refusal_and_escape()
                    }
                    Scenario::ShutdownEscalationAndRace => driver.shutdown_escalation_and_race(),
                    Scenario::PeerProtocolDosAndFlood => driver.peer_protocol_dos_and_flood(),
                    Scenario::LspRpcAndBufferDos => driver.lsp_rpc_and_buffer_dos(),
                    Scenario::ByzantineStaleAndTamper => driver.byzantine_stale_and_tamper(),
                    Scenario::AdversarialExhaustionAndRaces => {
                        driver.adversarial_exhaustion_and_races()
                    }
                    Scenario::LspAdversarialExhaustion => driver.lsp_adversarial_exhaustion(),
                    Scenario::ProtocolStateMachineFuzz => driver.protocol_state_machine_fuzz(seed),
                    Scenario::CrashBeforeAck | Scenario::TwoNodeFaults => {
                        let channel = driver.open_channel()?;
                        match scenario {
                            Scenario::CrashBeforeAck => driver.crash_before_ack(&channel),
                            _ => driver.two_node_faults(&channel, seed),
                        }
                        .with_context(|| {
                            format!("channel state: {:?}", driver.channel_summary(&channel))
                        })
                    }
                }
            })
        });
        run_result?;
        error_oracle.check(disk_faults_armed)?;
        Ok(())
    });
    result.with_context(|| trace.borrow().join("\n"))?;
    Ok(trace.take())
}

fn virtual_time() -> Option<u64> {
    let base_ms = turmoil::since_epoch()
        .map(|time| time.as_millis() as u64)
        .unwrap_or_else(|| EPOCH.as_millis() as u64 + DRIVER_ELAPSED_MS.get());
    Some(base_ms + CLOCK_OFFSET_MS.get())
}

struct Driver {
    runtime: Harness,
    chaos: Option<faults::Chaos>,
    coverage: crate::oracle::CoverageTracker,
}

impl Driver {
    fn clock_ms(&self) -> u64 {
        (EPOCH + self.runtime.elapsed()).as_millis() as u64 + CLOCK_OFFSET_MS.get()
    }

    fn advance_clock_past(&self, deadline: u64) {
        let delta = deadline
            .saturating_add(1_000)
            .saturating_sub(self.clock_ms());
        CLOCK_OFFSET_MS.set(CLOCK_OFFSET_MS.get() + delta);
        self.runtime.note(format_args!(
            "advance virtual Unix clock by {delta}ms for on-chain TLC expiry={deadline}"
        ));
    }

    #[allow(dead_code)]
    fn advance_clock_by(&self, delta_ms: u64) {
        CLOCK_OFFSET_MS.set(CLOCK_OFFSET_MS.get().saturating_add(delta_ms));
        self.runtime.note(format_args!(
            "advance virtual Unix clock by {delta_ms}ms (now={})",
            self.clock_ms()
        ));
    }

    #[allow(dead_code)]
    fn trigger_channel_maintenance(&self, nodes: impl IntoIterator<Item = usize>) -> Result<()> {
        let slots = self.runtime.nodes.borrow();
        for node in nodes {
            if let Some(live) = slots.get(node).and_then(|s| s.node.as_ref()) {
                crate::host::send(
                    &live.actor,
                    fnn::fiber::NetworkActorMessage::new_command(FiberActorCommand::CheckChannels),
                )?;
            }
        }
        Ok(())
    }

    fn step(&mut self) -> Result<()> {
        if let Some(mut chaos) = self.chaos.take() {
            let result = chaos.advance(&mut self.runtime);
            self.chaos = Some(chaos);
            result?;
        }
        let res = self.runtime.step();
        DRIVER_ELAPSED_MS.set(self.runtime.elapsed().as_millis() as u64);
        res
    }

    fn until<T>(
        &mut self,
        description: &str,
        mut observe: impl FnMut(&Self) -> Result<Option<T>>,
    ) -> Result<T> {
        for _ in 0..PHASE_TICKS {
            if let Some(value) = observe(self)? {
                return Ok(value);
            }
            self.step()?;
        }
        bail!("timed out waiting for {description}");
    }

    fn reply<T>(
        &mut self,
        description: &str,
        mut receiver: oneshot::Receiver<Result<T, String>>,
    ) -> Result<T> {
        self.until(description, |_| {
            receive(&mut receiver)?
                .transpose()
                .map_err(|error| anyhow!(error))
        })
        .with_context(|| format!("{description} failed"))
    }

    fn wait_online(&mut self, a: usize, b: usize, channel: &Hash256) -> Result<()> {
        self.until("both channel endpoints online", |s| {
            Ok(s.runtime.channel_online(a, b, channel).then_some(()))
        })
    }

    fn pay(&mut self, from: usize, command: SendPaymentCommand) -> Result<Hash256> {
        let reply = self
            .runtime
            .request(from, |reply| FiberActorCommand::SendPayment(command, reply))?;
        Ok(self.reply("send payment", reply)?.payment_hash)
    }

    fn wait_payment_actor(&mut self, from: usize) -> Result<()> {
        let cleanup_deadline = self.runtime.elapsed() + TICK * PHASE_TICKS as u32;
        loop {
            let reply = self
                .runtime
                .request(from, FiberActorCommand::GetInflightPaymentCount)?;
            let count = self.reply("payment actor cleanup", reply)?;
            if count == 0 {
                break;
            }
            ensure!(
                self.runtime.elapsed() < cleanup_deadline,
                "payment actor still active on {} (count={count})",
                crate::host::NAMES[from]
            );
        }
        Ok(())
    }
}

fn receive<T>(reply: &mut oneshot::Receiver<T>) -> Result<Option<T>> {
    match reply.try_recv() {
        Ok(message) => Ok(Some(message)),
        Err(oneshot::error::TryRecvError::Empty) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
