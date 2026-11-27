use std::cell::Cell;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{FiberActorCommand, Hash256};
use fnn::simulation;
use tokio::sync::oneshot;

use crate::harness::{Harness, HarnessOptions, EPOCH, TICK};
use crate::host::Trace;
use crate::tower::TowerMode;

mod faults;
mod funding;
mod onchain;
mod routes;
mod swarm;
mod two_node;

const PHASE_TICKS: usize = 6_000;

thread_local! {
    static CLOCK_OFFSET_MS: Cell<u64> = const { Cell::new(0) };
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
}

/// Exercise real Fiber actors and return the logical trace for cross-process replay.
/// Failures include the seed and the events preceding the failed assertion.
pub fn run(scenario: Scenario, seed: u64) -> Result<Vec<String>> {
    let trace = Trace::default();
    let result = simulation::run(seed ^ 0x0066_6962_6572, virtual_time, || {
        struct ResetClock;
        impl Drop for ResetClock {
            fn drop(&mut self) {
                CLOCK_OFFSET_MS.set(0);
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
            Scenario::Multihop => &[(0, 1), (1, 2)],
            Scenario::Mpp | Scenario::Swarm => &[(0, 1), (0, 2), (1, 3), (2, 3)],
            Scenario::Random { .. } => &topology.as_ref().expect("random topology").links,
            _ => &[(0, 1)],
        };
        // Random exploration has a watchdog per stage, including the protocol's MPP hold timer.
        let duration = if topology.is_some() {
            Duration::MAX
        } else {
            Duration::from_secs(900)
        };
        let continuous = matches!(scenario, Scenario::Random { fault_policy, .. }
            if fault_policy != RandomFaultPolicy::Windows);
        let tower_mode = (topology.is_some() || matches!(scenario, Scenario::WatchtowerSettlement))
            .then_some(if seed.is_multiple_of(2) {
                TowerMode::BuiltIn
            } else {
                TowerMode::External
            });
        let runtime = Harness::new(
            seed,
            directory.path(),
            trace.clone(),
            edges,
            HarnessOptions {
                duration,
                // Continuous reconnects need headroom in Turmoil's shared SYN and data queue.
                tcp_capacity: continuous.then_some(512),
                tower_mode,
                // A peer that misses the terminal abort frame follows its funding timer.
                funding_timeout_seconds: matches!(
                    scenario,
                    Scenario::ChannelIdRewrite { .. } | Scenario::ParallelFundingCleanup
                )
                .then_some(1),
            },
        )?;
        let mut driver = Driver {
            runtime,
            chaos: None,
        };
        driver.runtime.note(format_args!(
            "fiber={} seed={seed} scenario={scenario:?}",
            fnn::get_git_version()
        ));
        match scenario {
            Scenario::WatchtowerSettlement => driver.watchtower_settlement(),
            Scenario::Faults => driver.faults(),
            Scenario::Multihop => driver.multihop(),
            Scenario::Mpp => driver.mpp(seed),
            Scenario::Swarm => driver.swarm(seed, edges),
            Scenario::Random {
                payments,
                phases,
                fault_policy,
            } => driver.random(
                seed,
                topology.as_ref().expect("random topology"),
                payments,
                phases,
                fault_policy,
            ),
            Scenario::FundingAbortRecovery => driver.funding_abort_recovery(),
            Scenario::FundingSignatureReplay => driver.funding_signature_replay(),
            Scenario::ChannelIdRewrite { after_commit } => driver.channel_id_rewrite(after_commit),
            Scenario::ParallelFundingCleanup => {
                driver.parallel_funding_fixture(funding::BatchFault::AbortCleanup(1))
            }
            Scenario::ParallelFunding { mixed } => driver.parallel_funding_fixture(if mixed {
                funding::BatchFault::DropUpdate(1)
            } else {
                funding::BatchFault::None
            }),
            Scenario::CrashBeforeAck | Scenario::TwoNodeFaults => {
                let channel = driver.open_channel()?;
                match scenario {
                    Scenario::CrashBeforeAck => driver.crash_before_ack(&channel),
                    _ => driver.two_node_faults(&channel, seed),
                }
                .with_context(|| format!("channel state: {:?}", driver.channel_summary(&channel)))
            }
        }
    });
    result.with_context(|| trace.borrow().join("\n"))?;
    Ok(trace.take())
}

fn virtual_time() -> Option<u64> {
    turmoil::since_epoch().map(|time| time.as_millis() as u64 + CLOCK_OFFSET_MS.get())
}

struct Driver {
    runtime: Harness,
    chaos: Option<faults::Chaos>,
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

    fn step(&mut self) -> Result<()> {
        if let Some(mut chaos) = self.chaos.take() {
            let result = chaos.advance(&mut self.runtime);
            self.chaos = Some(chaos);
            result?;
        }
        self.runtime.step()
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
            let count = self
                .runtime
                .request(from, FiberActorCommand::GetInflightPaymentCount)?;
            if self.reply("payment actor cleanup", count)? == 0 {
                break;
            }
            ensure!(
                self.runtime.elapsed() < cleanup_deadline,
                "payment actor still active"
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
