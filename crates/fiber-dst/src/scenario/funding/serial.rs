use anyhow::{ensure, Context, Result};
use fnn::fiber::Hash256;

use crate::host::{FrameLoss, NAMES};

use super::{Attempt, Driver, Edge, FundingOrder, OpeningFault, FUNDING_AMOUNT};
use crate::scenario::faults::Fault;

impl Driver {
    pub(in crate::scenario) fn open_channel(&mut self) -> Result<Hash256> {
        Ok(self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel)
    }

    pub(in crate::scenario) fn open_between(
        &mut self,
        a: usize,
        b: usize,
        amount: u128,
        fee: u128,
        public: bool,
        fault: OpeningFault,
    ) -> Result<Edge> {
        const MAX_ATTEMPTS: usize = 2;

        for retry in 0..MAX_ATTEMPTS {
            let fault = match fault {
                OpeningFault::DropAcceptorFrame if retry > 0 => OpeningFault::None,
                fault => fault,
            };
            let order = FundingOrder {
                opener: a,
                acceptor: b,
                amount,
                gap_ticks: 0,
            };
            let (channel, edge) = self.open_between_attempt(order, fee, public, fault)?;
            if let Some(edge) = edge {
                return Ok(edge);
            }
            ensure!(
                retry + 1 < MAX_ATTEMPTS,
                "channel {channel} funding aborted on both endpoints"
            );
            self.runtime.note(format_args!(
                "retry fully aborted funding {}-{} channel={channel}",
                NAMES[a], NAMES[b]
            ));
        }
        unreachable!("the last aborted attempt returns an error")
    }

    fn open_between_attempt(
        &mut self,
        order: FundingOrder,
        fee: u128,
        public: bool,
        fault: OpeningFault,
    ) -> Result<(Hash256, Option<Edge>)> {
        let FundingOrder {
            opener: a,
            acceptor: b,
            amount,
            ..
        } = order;
        self.protect_open(a, b)?;
        let temporary = self.begin_open(a, b, amount, fee, public)?;
        let outage = matches!(fault, OpeningFault::CkbOutage)
            .then(|| self.begin_ckb_funding_fault(b))
            .transpose()?;
        let dropped_funding = matches!(fault, OpeningFault::DropAcceptorFrame)
            .then(|| {
                Fault::DropPeerFrame {
                    from: b,
                    to: a,
                    rule: FrameLoss::After(1),
                }
                .inject(&mut self.runtime)
            })
            .transpose()?;
        let reply = self.accept_open(b, temporary, amount)?;
        let response = self.reply("accept channel", reply)?;
        ensure!(
            response.old_channel_id == temporary,
            "accept reply changed temporary channel ID"
        );
        let channel = response.new_channel_id;
        let attempt = Attempt::new(&self.runtime, order, channel)?;
        if let Some(dropped_funding) = dropped_funding {
            self.until("funding frame is dropped", |s| {
                Ok(dropped_funding.hit(&s.runtime).then_some(()))
            })?;
            dropped_funding.heal(&mut self.runtime);
            self.runtime.note(format_args!(
                "dropped one funding frame {}->{} channel={channel}",
                NAMES[b], NAMES[a]
            ));
        }
        self.allow_open_faults(a, b, channel)?;
        if let Some(outage) = outage {
            self.restore_ckb_funding(b, &channel, outage)?;
        }
        let outcome = self
            .until("channel online or funding aborted", |s| {
                Ok(attempt.outcome(&s.runtime))
            })
            .with_context(|| {
                let endpoints = [a, b].map(|node| {
                    self.runtime
                        .channel(node, &channel)
                        .map(|state| (state.state, state.connectivity_state))
                });
                format!("channel {channel} persisted endpoints {endpoints:?}")
            })?;
        self.finish_open_faults();
        let edge = attempt.verify(&self.runtime, outcome, fee)?;
        if edge.is_some() {
            self.runtime.note(format_args!(
                "opened V2 channel {}-{} {channel}",
                NAMES[a], NAMES[b]
            ));
        } else {
            self.runtime.note(format_args!(
                "funding fully aborted {}-{} channel={channel} with no chain residue",
                NAMES[a], NAMES[b]
            ));
        }
        Ok((channel, edge))
    }

    pub(in crate::scenario) fn funding_abort_recovery(&mut self) -> Result<()> {
        let channel = self
            .open_between(
                0,
                1,
                FUNDING_AMOUNT,
                0,
                false,
                OpeningFault::DropAcceptorFrame,
            )?
            .channel;
        ensure!(
            self.runtime
                .nodes
                .borrow()
                .iter()
                .take(2)
                .all(|slot| !slot.aborted_channels.is_empty()),
            "the injected funding loss did not produce a two-sided abort"
        );
        self.runtime
            .note(format_args!("funding abort retry recovered {channel}"));
        self.probe(&channel)
    }
}
