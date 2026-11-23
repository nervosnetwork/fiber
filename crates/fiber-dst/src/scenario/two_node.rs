use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{ChannelConnectivityState, ChannelState, Hash256, PaymentStatus};
use fnn::simulation::{ChainFault, DiskFault};
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::host::{TcpFault, NAMES};

use super::{faults, Driver};

const TWO_NODE_PAYMENTS: usize = 8;

impl Driver {
    pub(super) fn channel_summary(&self, channel: &Hash256) -> [Option<String>; 2] {
        [0, 1].map(|node| {
            self.runtime.channel(node, channel).map(|state| {
                format!(
                    "{:?}, {:?}, commitment={:?}, balance={}, TLCs={}, waiting_ack={}",
                    state.state,
                    state.connectivity_state,
                    state.get_current_commitment_numbers(),
                    state.get_local_balance(),
                    state.tlc_state.all_tlcs().count(),
                    state.tlc_state.waiting_ack,
                )
            })
        })
    }

    pub(super) fn balances(&self, channel: &Hash256) -> Result<[u128; 2]> {
        let a = self
            .runtime
            .channel(0, channel)
            .context("channel state missing on A")?;
        let b = self
            .runtime
            .channel(1, channel)
            .context("channel state missing on B")?;
        Ok([a.get_local_balance(), b.get_local_balance()])
    }

    pub(super) fn start_payment(&mut self, from: usize, amount: u128) -> Result<Hash256> {
        let hash = self.pay(
            from,
            SendPaymentCommand {
                target_pubkey: Some(self.runtime.peers[1 - from]),
                amount: Some(amount),
                keysend: Some(true),
                max_parts: Some(1),
                ..Default::default()
            },
        )?;
        self.runtime.note(format_args!(
            "pay {} amount={amount} hash={hash}",
            NAMES[from]
        ));
        Ok(hash)
    }

    pub(super) fn check_safety(
        &self,
        channel: &Hash256,
        before: [u128; 2],
        from: usize,
        amount: u128,
    ) -> Result<()> {
        // Only one payment is in flight. Each endpoint may apply it at a different tick.
        let mut after = before;
        after[from] -= amount;
        after[1 - from] += amount;
        for (node, name) in NAMES[..2].iter().enumerate() {
            if let Some(state) = self.runtime.channel(node, channel) {
                let balance = state.get_local_balance();
                ensure!(
                    balance == before[node] || balance == after[node],
                    "{} applied an unexpected balance change to {balance}",
                    name
                );
                ensure!(
                    balance + state.get_remote_balance() == before.iter().sum::<u128>(),
                    "{} changed the funded channel capacity",
                    name
                );
            }
        }
        Ok(())
    }

    pub(super) fn finish_payment(
        &mut self,
        channel: &Hash256,
        from: usize,
        hash: Hash256,
        amount: u128,
        before: [u128; 2],
    ) -> Result<PaymentStatus> {
        let status = self.until("terminal payment and empty TLC queues", |s| {
            s.check_safety(channel, before, from, amount)?;
            let status = s.runtime.payment_status(from, hash);
            let drained = s.runtime.link(0, 1)
                && (0..2).all(|node| {
                    s.runtime.channel(node, channel).is_some_and(|state| {
                        state.state == ChannelState::ChannelReady
                            && state.connectivity_state == ChannelConnectivityState::Online
                            && !state.tlc_state.waiting_ack
                            && state.tlc_state.all_tlcs().next().is_none()
                    })
                });
            Ok(status.filter(|status| status.is_final() && drained))
        })?;
        let mut expected = before;
        if status == PaymentStatus::Success {
            expected[from] -= amount;
            expected[1 - from] += amount;
        }
        let actual = self.balances(channel)?;
        ensure!(
            actual == expected,
            "payment {hash} {status:?} expected {expected:?}, got {actual:?}"
        );
        let a = self
            .runtime
            .channel(0, channel)
            .context("missing A channel")?;
        let b = self
            .runtime
            .channel(1, channel)
            .context("missing B channel")?;
        ensure!(
            a.get_remote_balance() == actual[1] && b.get_remote_balance() == actual[0],
            "channel balances disagree"
        );
        let a_round = a.get_current_commitment_numbers();
        let b_round = b.get_current_commitment_numbers();
        ensure!(
            a_round.local == b_round.remote && a_round.remote == b_round.local,
            "commitments disagree: {a_round:?} {b_round:?}"
        );
        ensure!(
            a.tlc_state.offered_tlcs.next_tlc_id == b.tlc_state.received_tlcs.next_tlc_id
                && a.tlc_state.received_tlcs.next_tlc_id == b.tlc_state.offered_tlcs.next_tlc_id,
            "settled TLC cursors disagree"
        );
        self.wait_payment_actor(from)?;
        self.runtime.note(format_args!(
            "settled {hash} {status:?} balances={actual:?} round={a_round:?}"
        ));
        Ok(status)
    }

    pub(super) fn probe(&mut self, channel: &Hash256) -> Result<()> {
        let before = self.balances(channel)?;
        let payment = self.start_payment(0, 1)?;
        ensure!(
            self.finish_payment(channel, 0, payment, 1, before)? == PaymentStatus::Success,
            "recovery probe did not succeed"
        );
        Ok(())
    }

    pub(super) fn crash_before_ack(&mut self, channel: &Hash256) -> Result<()> {
        let before = self.balances(channel)?;
        let b_round = self
            .runtime
            .channel(1, channel)
            .context("missing B channel")?
            .get_current_commitment_numbers();
        let cut = faults::Fault::ProtocolPause {
            node: 1,
            channel: *channel,
            point: faults::ProtocolPoint::CommitmentAck,
            preceding_hits: 0,
        }
        .inject(&mut self.runtime)?;
        let payment = self.start_payment(0, 100)?;
        self.until("B persisted commitment before ACK", |s| {
            s.check_safety(channel, before, 0, 100)?;
            Ok(cut.hit(&s.runtime).then_some(()))
        })?;
        let persisted = self
            .runtime
            .channel(1, channel)
            .context("missing B channel")?
            .get_current_commitment_numbers();
        ensure!(
            persisted != b_round,
            "pause did not follow a durable B commitment"
        );
        self.runtime
            .note(format_args!("B committed {persisted:?} before ACK"));
        let crash = faults::Fault::Crash(1).inject(&mut self.runtime)?;
        cut.heal(&mut self.runtime);
        self.until("A notices B disconnect", |s| {
            Ok((!s.runtime.nodes.borrow()[0].connected.contains(&1)).then_some(()))
        })?;
        ensure!(
            self.runtime.nodes.borrow()[0].generation == 1,
            "A restarted while B was down"
        );
        crash.heal(&mut self.runtime);
        ensure!(
            self.finish_payment(channel, 0, payment, 100, before)? == PaymentStatus::Success,
            "crash-window payment did not succeed"
        );
        self.probe(channel)
    }

    pub(super) fn two_node_faults(&mut self, channel: &Hash256, seed: u64) -> Result<()> {
        let mut workload = StdRng::seed_from_u64(seed ^ 0x776f_726b);
        let mut faults = StdRng::seed_from_u64(seed ^ 0x0066_6175_6c74);
        for _ in 0..TWO_NODE_PAYMENTS {
            let from = workload.random_range(0..2);
            let target = faults.random_range(0..2);
            let amount = workload.random_range(10..1_000);
            let fault = faults.random_range(0..9);
            match fault {
                3 => {
                    let fault = match faults.random_range(0..3) {
                        0 => ChainFault::Offline,
                        1 => ChainFault::Slow(Duration::from_millis(30)),
                        _ => ChainFault::Timeout(Duration::from_millis(30)),
                    };
                    self.ckb_fault_payment(channel, from, target, amount, fault)?;
                    continue;
                }
                4 => {
                    let fault = [DiskFault::WriteIo, DiskFault::DiskFull, DiskFault::SyncIo]
                        [faults.random_range(0..3)];
                    self.disk_fault_payment(channel, from, amount, fault)?;
                    continue;
                }
                5 => {
                    let fault = [TcpFault::Dial, TcpFault::Read, TcpFault::Write]
                        [faults.random_range(0..3)];
                    self.tcp_fault_payment(channel, from, amount, fault)?;
                    continue;
                }
                6 => {
                    self.queue_fault_payment(channel, from, amount)?;
                    continue;
                }
                7 => {
                    // Control workload without a fault provides a baseline in the same run.
                    let before = self.balances(channel)?;
                    let payment = self.start_payment(from, amount)?;
                    self.finish_payment(channel, from, payment, amount, before)?;
                    continue;
                }
                8 => {
                    let delay = Duration::from_millis(faults.random_range(25..=80));
                    self.mailbox_fault_payment(channel, from, target, amount, delay)?;
                    continue;
                }
                _ => {}
            }
            let before = self.balances(channel)?;
            let payment = self.start_payment(from, amount)?;
            for _ in 0..faults.random_range(0..4) {
                self.step()?;
                self.check_safety(channel, before, from, amount)?;
            }
            let injection = match fault {
                0 => faults::Fault::Crash(target),
                1 => {
                    self.runtime.note("partition A <-> B");
                    faults::Fault::Partition {
                        from: 0,
                        to: 1,
                        one_way: false,
                    }
                }
                _ => {
                    self.runtime.note(format_args!(
                        "partition {} -> {}",
                        NAMES[target],
                        NAMES[1 - target]
                    ));
                    faults::Fault::Partition {
                        from: target,
                        to: 1 - target,
                        one_way: true,
                    }
                }
            }
            .inject(&mut self.runtime)?;
            for _ in 0..faults.random_range(8..20) {
                self.step()?;
                self.check_safety(channel, before, from, amount)?;
            }
            if fault != 0 {
                // Reopen the original TCP session after the partition drops data.
                self.runtime.note("heal A <-> B and reconnect");
            }
            injection.heal(&mut self.runtime);
            if fault != 0 {
                self.until("both peers close the partitioned connection", |s| {
                    Ok(s.runtime
                        .nodes
                        .borrow()
                        .iter()
                        .all(|node| node.connected.is_empty())
                        .then_some(()))
                })?;
            }
            self.finish_payment(channel, from, payment, amount, before)?;
        }
        self.probe(channel)
    }
}
