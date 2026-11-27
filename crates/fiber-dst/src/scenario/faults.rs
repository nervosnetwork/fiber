use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use fnn::fiber::channel::{ChannelActorMessage, ChannelCommand};
use fnn::fiber::{ChannelState, FiberActorCommand, Hash256, PaymentStatus};
use fnn::simulation::{ChainEndpoint, ChainFault, DiskFault};
use ractor::ActorRef;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::sync::oneshot;

use crate::host::{FrameDelay, TcpFault, NAMES};

use super::{funding::OpeningFault, receive, Driver, FUNDING_AMOUNT, TICK};

mod chaos;
mod injection;
mod window;

pub(super) use chaos::Chaos;
pub(super) use injection::{Fault, Injection, MailboxTarget, ProtocolPoint};
pub(super) use window::{schedule_fault_windows, FaultWindow};

pub(super) fn long_delay_rng(seed: u64) -> Option<StdRng> {
    let mut rng = StdRng::seed_from_u64(seed ^ 0x6465_6c61_792d_6473);
    rng.random_ratio(1, 8).then_some(rng)
}

pub(super) fn sample_long_delay(rng: &mut StdRng, after: usize) -> FrameDelay {
    FrameDelay {
        after,
        duration: TICK * rng.random_range(1_000..=2_000),
    }
}

impl Driver {
    pub(super) fn faults(&mut self) -> Result<()> {
        let channel = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::CkbOutage)?
            .channel;
        self.ckb_fault_payment(
            &channel,
            0,
            1,
            100,
            ChainFault::Timeout(Duration::from_millis(30)),
        )?;
        for fault in [DiskFault::WriteIo, DiskFault::DiskFull, DiskFault::SyncIo] {
            self.disk_fault_payment(&channel, 0, 100, fault)?;
        }
        for fault in [TcpFault::Dial, TcpFault::Read, TcpFault::Write] {
            self.tcp_fault_payment(&channel, 0, 100, fault)?;
        }
        self.queue_fault_payment(&channel, 0, 100)?;
        self.mailbox_fault_payment(&channel, 0, 1, 100, Duration::from_millis(50))?;
        self.long_frame_delay_payment(&channel)?;
        self.probe(&channel)
    }

    fn long_frame_delay_payment(&mut self, channel: &Hash256) -> Result<()> {
        let delay = Duration::from_secs(7);
        let before = self.balances(channel)?;
        let started = self.runtime.elapsed();
        let injection = Fault::DelayPeerFrame {
            from: 0,
            to: 1,
            rule: FrameDelay {
                after: 0,
                duration: delay,
            },
        }
        .inject(&mut self.runtime)?;
        let payment = self.start_payment(0, 100)?;
        self.until(
            "the payment is still pending before the delayed frame can arrive",
            |s| {
                ensure!(s.runtime.link(0, 1), "frame delay disconnected the peers");
                ensure!(
                    s.runtime
                        .payment_status(0, payment)
                        .is_some_and(|status| !status.is_final()),
                    "payment terminated before the delayed frame could arrive"
                );
                s.check_safety(channel, before, 0, 100)?;
                Ok((injection.hit(&s.runtime)
                    && s.runtime.elapsed() - started >= delay - Duration::from_millis(10))
                .then_some(()))
            },
        )?;
        self.runtime.note(format_args!(
            "long peer frame delay kept payment {payment} pending"
        ));
        injection.heal(&mut self.runtime);
        let status = self.finish_payment(channel, 0, payment, 100, before)?;
        ensure!(
            status == PaymentStatus::Success && self.runtime.elapsed() - started >= delay,
            "payment did not settle after the long peer frame delay"
        );
        Ok(())
    }

    pub(super) fn begin_ckb_funding_fault(&mut self, node: usize) -> Result<Injection> {
        self.runtime
            .note(format_args!("{} CKB offline during funding", NAMES[node]));
        Fault::Chain {
            node,
            request: None,
            fault: ChainFault::Offline,
        }
        .inject(&mut self.runtime)
    }

    pub(super) fn restore_ckb_funding(
        &mut self,
        node: usize,
        channel: &Hash256,
        outage: Injection,
    ) -> Result<()> {
        self.until("CKB outage blocks a funding command", |s| {
            Ok(outage
                .chain_hit(&s.runtime, Some(ChainEndpoint::Actor))
                .then_some(()))
        })?;
        ensure!(
            self.runtime
                .channel(node, channel)
                .is_none_or(|state| state.state != ChannelState::ChannelReady),
            "funding completed while CKB was unavailable"
        );
        outage.heal(&mut self.runtime);
        let slowdown = Fault::Chain {
            node,
            request: Some((ChainEndpoint::Client, "get_block_timestamp")),
            fault: ChainFault::Slow(Duration::from_millis(30)),
        }
        .inject(&mut self.runtime)?;
        self.until("funding reaches the delayed block timestamp query", |s| {
            Ok(slowdown.hit(&s.runtime).then_some(()))
        })?;
        slowdown.heal(&mut self.runtime);
        Ok(())
    }

    pub(super) fn ckb_fault_payment(
        &mut self,
        channel: &Hash256,
        from: usize,
        target: usize,
        amount: u128,
        fault: ChainFault,
    ) -> Result<()> {
        let before = self.balances(channel)?;
        let injection = Fault::Chain {
            node: target,
            request: Some((ChainEndpoint::Client, "get_shutdown_tx")),
            fault,
        }
        .inject(&mut self.runtime)?;
        let payment = self.start_payment(from, amount)?;
        let check = self.runtime.request(target, |reply| {
            FiberActorCommand::CheckChannelShutdown(*channel, reply)
        })?;
        self.reply("request CKB shutdown query", check)?;
        self.until(
            "active channel's chain query hits the scheduled fault",
            |s| {
                s.check_safety(channel, before, from, amount)?;
                Ok(injection.hit(&s.runtime).then_some(()))
            },
        )?;
        injection.heal(&mut self.runtime);
        self.finish_payment(channel, from, payment, amount, before)?;
        Ok(())
    }

    pub(super) fn disk_fault_payment(
        &mut self,
        channel: &Hash256,
        from: usize,
        amount: u128,
        fault: DiskFault,
    ) -> Result<()> {
        let target = 1 - from;
        let before = self.balances(channel)?;
        let injection = Fault::Sqlite {
            node: target,
            fault,
            batch: None,
        }
        .inject(&mut self.runtime)?;
        self.runtime
            .note(format_args!("{} arm SQLite {fault:?}", NAMES[target]));
        let payment = self.start_payment(from, amount)?;
        self.until("SQLite error stops the target node", |s| {
            s.check_safety(channel, before, from, amount)?;
            Ok(s.runtime.nodes.borrow()[target]
                .storage_failed
                .then_some(()))
        })?;
        ensure!(injection.hit(&self.runtime), "SQLite hit a different fault");
        ensure!(
            self.runtime.payment_status(from, payment) != Some(PaymentStatus::Success),
            "payer observed success before the failed write recovered"
        );
        injection.heal(&mut self.runtime);
        self.finish_payment(channel, from, payment, amount, before)?;
        Ok(())
    }

    pub(super) fn tcp_fault_payment(
        &mut self,
        channel: &Hash256,
        from: usize,
        amount: u128,
        fault: TcpFault,
    ) -> Result<()> {
        let target = match fault {
            TcpFault::Dial => 0,
            TcpFault::Read => 1 - from,
            TcpFault::Write => from,
        };
        let injection = Fault::Tcp {
            from: target,
            to: 1 - target,
            fault,
            reconnect: fault == TcpFault::Dial,
        }
        .inject(&mut self.runtime)?;
        self.runtime
            .note(format_args!("{} arm TCP {fault:?}", NAMES[target]));
        if fault == TcpFault::Dial {
            self.until("TCP dial fault", |s| {
                Ok(injection.hit(&s.runtime).then_some(()))
            })?;
            self.wait_online(0, 1, channel)?;
        }
        let before = self.balances(channel)?;
        let payment = self.start_payment(from, amount)?;
        self.until("TCP operation hits the scheduled fault", |s| {
            s.check_safety(channel, before, from, amount)?;
            Ok(injection.hit(&s.runtime).then_some(()))
        })?;
        self.runtime
            .note(format_args!("{} hit TCP {fault:?}", NAMES[target]));
        injection.heal(&mut self.runtime);
        self.finish_payment(channel, from, payment, amount, before)?;
        Ok(())
    }

    pub(super) fn queue_fault_payment(
        &mut self,
        channel: &Hash256,
        from: usize,
        amount: u128,
    ) -> Result<()> {
        let before = self.balances(channel)?;
        let injection = Fault::Writer { from, to: 1 - from }.inject(&mut self.runtime)?;
        let payment = self.start_payment(from, amount)?;
        self.until("bounded TCP writer queue applies backpressure", |s| {
            s.check_safety(channel, before, from, amount)?;
            Ok(injection.hit(&s.runtime).then_some(()))
        })?;
        self.runtime.note(format_args!(
            "{} bounded TCP writer queue full",
            NAMES[from]
        ));
        injection.heal(&mut self.runtime);
        self.finish_payment(channel, from, payment, amount, before)?;
        Ok(())
    }

    fn channel_actor(
        &mut self,
        node: usize,
        channel: Hash256,
    ) -> Result<ActorRef<ChannelActorMessage>> {
        let mut receiver = self.runtime.request(node, |reply| {
            FiberActorCommand::GetChannelActor(channel, reply)
        })?;
        self.until("live channel actor", |_| receive(&mut receiver))?
            .context("channel actor is missing")
    }

    pub(super) fn mailbox_fault_payment(
        &mut self,
        channel: &Hash256,
        from: usize,
        target: usize,
        amount: u128,
        delay: Duration,
    ) -> Result<()> {
        const BURST: usize = 4;
        let slow = self.channel_actor(target, *channel)?;
        let healthy = self.channel_actor(1 - target, *channel)?;
        let before = self.balances(channel)?;
        let started = self.runtime.elapsed();
        let fault = Fault::Mailbox {
            target: MailboxTarget::Actor(slow.get_cell()),
            delay,
        }
        .inject(&mut self.runtime)?;
        self.runtime.note(format_args!(
            "{} slow channel mailbox {channel} by {delay:?} per message",
            NAMES[target]
        ));

        let request_status = |actor: &ActorRef<ChannelActorMessage>| {
            let (reply, receiver) = oneshot::channel();
            actor
                .send_message(ChannelActorMessage::Command(ChannelCommand::TestBarrier(
                    reply.into(),
                )))
                .map_err(|error| anyhow!(error.to_string()))?;
            Ok::<_, anyhow::Error>(receiver)
        };
        let mut backlog = (0..BURST)
            .map(|_| request_status(&slow))
            .collect::<Result<Vec<_>>>()?;
        let mut control = request_status(&healthy)?;
        let network_control = self
            .runtime
            .request(target, FiberActorCommand::GetInflightPaymentCount)?;
        self.reply("unaffected network on the same node", network_control)?;
        self.until("unaffected channel handles its request", |_| {
            receive(&mut control)
        })?;
        ensure!(
            self.runtime.elapsed() - started < delay,
            "mailbox delay stalled an untargeted actor"
        );
        ensure!(
            backlog
                .iter_mut()
                .all(|reply| matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty))),
            "target mailbox answered before the first delay elapsed"
        );
        self.runtime.note(format_args!(
            "{} channel mailbox backlog={BURST} pending replies; local network and {} channel responded",
            NAMES[target],
            NAMES[1 - target]
        ));
        for (index, reply) in backlog.iter_mut().enumerate() {
            self.until("slow channel handles its queued request", |_| {
                receive(reply)
            })?;
            ensure!(
                self.runtime.elapsed() - started >= delay * (index as u32 + 1),
                "mailbox backlog drained faster than one message per {delay:?}"
            );
        }
        let backlog_hits = fault.mailbox_hits();
        ensure!(
            backlog_hits >= BURST as u64,
            "mailbox delay missed the burst"
        );

        let payment = self.start_payment(from, amount)?;
        ensure!(
            self.finish_payment(channel, from, payment, amount, before)? == PaymentStatus::Success,
            "payment did not settle through the slow actor"
        );
        let payment_hits = fault.mailbox_hits() - backlog_hits;
        ensure!(
            payment_hits > 0,
            "payment did not exercise the slow mailbox"
        );
        fault.heal(&mut self.runtime);
        self.runtime.note(format_args!(
            "{} channel mailbox recovered after {payment_hits} further delayed messages",
            NAMES[target]
        ));
        Ok(())
    }
}
