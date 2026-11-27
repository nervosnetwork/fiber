use anyhow::{ensure, Context, Result};
use fnn::fiber::{ChannelState, Hash256};
use fnn::fiber_types::schema::CHANNEL_IMMUTABLE_STATE_PREFIX;
use fnn::fiber_types::{AwaitingChannelReadyFlags, AwaitingTxSignaturesFlags};
use fnn::simulation::{BatchTarget, DiskFault};

use crate::host::{FrameLoss, NAMES};

use super::{
    audited_channel_state, mutable_key, Attempt, Driver, Edge, FundingOrder, OpeningFault, Outcome,
    FUNDING_AMOUNT,
};
use crate::scenario::faults::{Fault, ProtocolPoint};

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
        let attempt = Attempt::new(&self.runtime, order, temporary, channel)?;
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

    pub(in crate::scenario) fn channel_id_rewrite(&mut self, after_commit: bool) -> Result<()> {
        let order = FundingOrder {
            opener: 0,
            acceptor: 1,
            amount: FUNDING_AMOUNT,
            gap_ticks: 0,
        };
        self.protect_open(0, 1)?;
        let temporary = self.begin_open(0, 1, order.amount, 0, false)?;
        ensure!(
            audited_channel_state(&self.runtime.store(0)?, temporary, &self.runtime.peers[1])?
                .is_some(),
            "opener has no temporary channel {temporary}"
        );
        let injection = if after_commit {
            Fault::ProtocolPause {
                node: 0,
                channel: temporary,
                point: ProtocolPoint::ChannelIdRewritten,
                preceding_hits: 0,
            }
        } else {
            Fault::Sqlite {
                node: 0,
                fault: DiskFault::SyncIo,
                batch: Some(BatchTarget {
                    put_prefix: Some(vec![CHANNEL_IMMUTABLE_STATE_PREFIX]),
                    delete_prefix: Some(mutable_key(temporary)),
                }),
            }
        }
        .inject(&mut self.runtime)?;
        let reply = self.accept_open(1, temporary, order.amount)?;
        let response = self.reply("accept channel for ID rewrite", reply)?;
        let channel = response.new_channel_id;
        ensure!(response.old_channel_id == temporary && temporary != channel);
        let attempt = Attempt::new(&self.runtime, order, temporary, channel)?;
        self.until("channel ID rewrite interruption", |s| {
            Ok(injection.hit(&s.runtime).then_some(()))
        })?;
        if after_commit {
            self.runtime.crash(0);
            injection.heal(&mut self.runtime);
        } else {
            injection.stop_after_sqlite_fault(&mut self.runtime);
        }
        {
            let store = self.runtime.reopen_offline_store(0)?;
            let old = audited_channel_state(&store, temporary, &self.runtime.peers[1])?.is_some();
            let new = audited_channel_state(&store, channel, &self.runtime.peers[1])?.is_some();
            ensure!(old != new, "ID rewrite kept old={old} and new={new}");
            ensure!(
                !after_commit || new,
                "successful ID rewrite lost its final record"
            );
            self.runtime.note(format_args!(
                "ID rewrite after_commit={after_commit} old={old} final={new}"
            ));
        }
        self.runtime.restart(0);
        let outcome = self
            .until("interrupted channel ID rewrite reconciles", |s| {
                if s.runtime.channel_online(0, 1, &channel) {
                    return Ok(Some(Outcome::Online));
                }
                // The opener may still have used the temporary id when its write failed.
                // Check durable absence under both IDs, even when its abort receipt used the old ID.
                let both_absent = s.runtime.link(0, 1)
                    && s.runtime.funding_record_absent(0, temporary)?
                    && s.runtime.funding_record_absent(0, channel)?
                    && s.runtime.funding_record_absent(1, channel)?;
                Ok(both_absent.then_some(Outcome::Aborted))
            })
            .with_context(|| format!("opening {temporary} -> {channel}"))?;
        ensure!(
            audited_channel_state(&self.runtime.store(0)?, temporary, &self.runtime.peers[1])?
                .is_none(),
            "temporary channel survived recovery"
        );
        let ready = match attempt.verify(&self.runtime, outcome, 0)? {
            Some(edge) => edge.channel,
            None => self.open_channel()?,
        };
        self.probe(&ready)?;
        self.runtime
            .note(format_args!("ID rewrite recovered {ready}"));
        Ok(())
    }

    pub(in crate::scenario) fn funding_signature_replay(&mut self) -> Result<()> {
        self.protect_open(0, 1)?;
        let temporary = self.begin_open(0, 1, FUNDING_AMOUNT, 0, false)?;
        // A funds less, so it sends its signatures first and B sends the final response.
        let reply = self.accept_open(1, temporary, FUNDING_AMOUNT + 100_000_000)?;
        let response = self.reply("accept channel for signature replay", reply)?;
        ensure!(response.old_channel_id == temporary);
        let channel = response.new_channel_id;
        let loss = Fault::DropPeerFrame {
            from: 1,
            to: 0,
            rule: FrameLoss::FundingSignatures(channel),
        }
        .inject(&mut self.runtime)?;
        let outpoint = self.until(
            "A is missing funding signatures and B has confirmed funding",
            |s| {
                if !loss.hit(&s.runtime) {
                    return Ok(None);
                }
                let Some(a) = s.runtime.channel(0, &channel) else {
                    return Ok(None);
                };
                let Some(b) = s.runtime.channel(1, &channel) else {
                    return Ok(None);
                };
                Ok(match (a.state, b.state) {
                    (
                        ChannelState::AwaitingTxSignatures(flags),
                        ChannelState::AwaitingChannelReady(ready),
                    ) if flags == AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT
                        && ready == AwaitingChannelReadyFlags::OUR_CHANNEL_READY =>
                    {
                        b.get_funding_transaction_outpoint()
                    }
                    _ => None,
                })
            },
        )?;
        self.runtime.note(format_args!(
            "restart A missing B funding signatures channel={channel} outpoint={outpoint:?}"
        ));
        let crash = Fault::Crash(0).inject(&mut self.runtime)?;
        loss.heal(&mut self.runtime);
        crash.heal(&mut self.runtime);

        self.wait_online(0, 1, &channel)?;
        for (node, name) in NAMES[..2].iter().enumerate() {
            let state = self
                .runtime
                .channel(node, &channel)
                .context("missing recovered channel")?;
            ensure!(
                state.get_funding_transaction_outpoint().as_ref() == Some(&outpoint),
                "{name} changed the funding outpoint during recovery"
            );
        }
        self.probe(&channel)?;
        self.runtime.note(format_args!(
            "funding signature replay recovered channel={channel} outpoint={outpoint:?}"
        ));
        Ok(())
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
