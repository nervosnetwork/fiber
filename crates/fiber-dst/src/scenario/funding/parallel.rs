use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use fnn::fiber::config::DEFAULT_FINAL_TLC_EXPIRY_DELTA;
use fnn::fiber::payment::SendPaymentWithRouterCommand;
use fnn::fiber::{ChannelState, FiberActorCommand, Hash256, PaymentStatus};
use fnn::fiber_types::{CloseFlags, RouterHop};
use fnn::simulation::{BatchTarget, ChainEndpoint, ChainFault, DiskFault};

use crate::host::{FrameLoss, NAMES};
use crate::scenario::{faults::Fault, TICK};

use super::{
    audited_channel_state, mutable_key, Attempt, BatchFault, Driver, Edge, FundingOrder, Outcome,
    FUNDING_AMOUNT,
};

impl Driver {
    pub(in crate::scenario) fn parallel_funding_fixture(
        &mut self,
        fault: BatchFault,
    ) -> Result<()> {
        let expected_aborts = usize::from(matches!(
            fault,
            BatchFault::DropUpdate(_) | BatchFault::AbortCleanup(_)
        ));
        let count = if expected_aborts == 0 { 6 } else { 4 };
        let orders = (0..count)
            .map(|index| FundingOrder {
                opener: index % 2,
                acceptor: 1 - index % 2,
                amount: FUNDING_AMOUNT - index as u128 * 100_000_000,
                gap_ticks: index % 2,
            })
            .collect();
        let edges = self.parallel_funding(orders, fault, 0, &mut BTreeSet::new())?;
        ensure!(edges.len() == count - expected_aborts);
        Ok(())
    }

    pub(in crate::scenario) fn parallel_funding(
        &mut self,
        mut orders: Vec<FundingOrder>,
        fault: BatchFault,
        fee: u128,
        funding_txs: &mut BTreeSet<Hash256>,
    ) -> Result<Vec<Edge>> {
        let pair = [orders[0].opener, orders[0].acceptor];
        let [a, b] = pair;
        let policy = match fault {
            BatchFault::None => "none",
            BatchFault::DropUpdate(_) => "drop-update",
            BatchFault::AbortCleanup(_) => "abort-cleanup",
            BatchFault::Chaos(_) => "continuous",
            BatchFault::SlowChain { .. } => "slow-ckb",
        };
        self.runtime.note(format_args!(
            "parallel funding {}-{} starts attempts={} fault={policy}",
            NAMES[a],
            NAMES[b],
            orders.len()
        ));
        self.protect_open(a, b)?;
        let drop_victim = match fault {
            BatchFault::DropUpdate(index) | BatchFault::AbortCleanup(index) => {
                // Start the faulty attempt before submitting its healthy siblings.
                let victim = orders.remove(index);
                orders.insert(0, victim);
                Some(0)
            }
            _ => None,
        };
        let mut accepted = BTreeMap::new();
        let mut dropped = None;
        let mut offers = Vec::with_capacity(orders.len());
        for (index, order) in orders.into_iter().enumerate() {
            for _ in 0..order.gap_ticks {
                self.step()?;
            }
            let temporary =
                self.begin_open(order.opener, order.acceptor, order.amount, fee, true)?;
            self.runtime.note(format_args!(
                "parallel funding offer {}->{} temp={temporary} amount={}",
                NAMES[order.opener], NAMES[order.acceptor], order.amount
            ));
            if Some(index) == drop_victim {
                let reply = self.accept_open(order.acceptor, temporary, order.amount)?;
                let response = self.reply("accept parallel funding victim", reply)?;
                ensure!(response.old_channel_id == temporary);
                let channel = response.new_channel_id;
                let drop = Fault::DropPeerFrame {
                    from: order.acceptor,
                    to: order.opener,
                    rule: FrameLoss::FundingUpdate(channel),
                }
                .inject(&mut self.runtime)?;
                self.until("targeted TxUpdate is lost before sibling offers", |s| {
                    Ok(drop.hit(&s.runtime).then_some(()))
                })?;
                ensure!(
                    pair.iter()
                        .all(|&node| !self.runtime.funding_aborted(node, channel)),
                    "parallel funding victim {channel} aborted before sibling offers"
                );
                self.runtime.note(format_args!(
                    "parallel funding victim={channel} waiting before sibling offers"
                ));
                accepted.insert(index, channel);
                dropped = Some(drop);
            }
            offers.push((order, temporary));
        }
        // Queue the healthy accept commands together while all of their offers are pending.
        let mut replies = Vec::new();
        for (index, (order, temporary)) in offers.iter().enumerate() {
            if Some(index) != drop_victim {
                replies.push((
                    index,
                    self.accept_open(order.acceptor, *temporary, order.amount)?,
                ));
            }
        }
        for (index, reply) in replies {
            let response = self.reply("accept parallel funding", reply)?;
            ensure!(response.old_channel_id == offers[index].1);
            accepted.insert(index, response.new_channel_id);
        }
        let attempts = offers
            .into_iter()
            .enumerate()
            .map(|(index, (order, _))| Attempt::new(&self.runtime, order, accepted[&index]))
            .collect::<Result<Vec<_>>>()?;
        let ids: BTreeSet<_> = attempts.iter().map(|attempt| attempt.channel).collect();
        ensure!(
            ids.len() == attempts.len(),
            "parallel attempts reused a channel ID"
        );
        let in_funding = attempts
            .iter()
            .filter(|attempt| {
                attempt.initial.state != ChannelState::ChannelReady
                    && !attempt.initial.is_closed()
                    && !self
                        .runtime
                        .funding_aborted(attempt.order.opener, attempt.channel)
            })
            .count();
        ensure!(
            in_funding >= 2,
            "parallel opening never had two active funding attempts"
        );
        self.runtime.note(format_args!(
            "parallel funding overlap={in_funding} channels={ids:?}"
        ));

        if let Some(drop) = dropped {
            let victim = attempts[drop_victim.expect("armed victim")].channel;
            self.until(
                "targeted funding frame lost and sibling channels online",
                |s| {
                    ensure!(
                        pair.iter()
                            .all(|&node| !s.runtime.funding_aborted(node, victim)),
                        "parallel funding victim {victim} aborted before the scripted reconnect"
                    );
                    Ok((drop.hit(&s.runtime)
                        && !s.runtime.channel_online(a, b, &victim)
                        && attempts
                            .iter()
                            .filter(|attempt| attempt.channel != victim)
                            .all(|attempt| s.runtime.channel_online(a, b, &attempt.channel)))
                    .then_some(()))
                },
            )
            .with_context(|| self.parallel_funding_status(pair, &attempts))?;
            self.runtime.note(format_args!(
                "parallel funding victim={victim} pending with online siblings"
            ));
            let cleanup = if matches!(fault, BatchFault::AbortCleanup(_)) {
                let order = &attempts[drop_victim.expect("armed victim")].order;
                let node = order.acceptor;
                ensure!(
                    audited_channel_state(
                        &self.runtime.store(node)?,
                        victim,
                        &self.runtime.peers[order.opener]
                    )?
                    .is_some(),
                    "abort victim {victim} was already absent"
                );
                let injection = Fault::Sqlite {
                    node,
                    fault: DiskFault::WriteIo,
                    batch: Some(BatchTarget {
                        put_prefix: None,
                        delete_prefix: Some(mutable_key(victim)),
                    }),
                }
                .inject(&mut self.runtime)?;
                Some((node, injection))
            } else {
                None
            };
            // The dropped application frame stays lost. Reconnect makes Fiber reconcile it.
            drop.heal(&mut self.runtime);
            if let Some((node, injection)) = cleanup {
                self.until("victim funding cleanup hits SQLite error", |s| {
                    Ok(injection.hit(&s.runtime).then_some(()))
                })?;
                injection.stop_after_sqlite_fault(&mut self.runtime);
                {
                    let store = self.runtime.reopen_offline_store(node)?;
                    let peer = if node == a { b } else { a };
                    ensure!(
                        audited_channel_state(&store, victim, &self.runtime.peers[peer])?
                            == Some(ChannelState::Closed(CloseFlags::FUNDING_ABORTED)),
                        "failed delete did not preserve the terminal state of channel {victim}"
                    );
                    for attempt in attempts.iter().filter(|attempt| attempt.channel != victim) {
                        ensure!(
                            audited_channel_state(
                                &store,
                                attempt.channel,
                                &self.runtime.peers[peer]
                            )? == Some(ChannelState::ChannelReady),
                            "abort of {victim} affected sibling {}",
                            attempt.channel
                        );
                    }
                    self.runtime.note(format_args!(
                        "funding cleanup SQLite error rolled back channel={victim}"
                    ));
                }
                self.runtime.restart(node);
            }
        } else if let BatchFault::Chaos(index) = fault {
            self.allow_open_faults(a, b, attempts[index].channel)?;
        } else if let BatchFault::SlowChain { index, ticks } = fault {
            let node = attempts[index].order.opener;
            let slow = Fault::Chain {
                node,
                request: Some((ChainEndpoint::Actor, "Sign")),
                fault: ChainFault::Slow(TICK * ticks),
            }
            .inject(&mut self.runtime)?;
            self.until("parallel funding hits CKB signing delay", |s| {
                Ok(slow.hit(&s.runtime).then_some(()))
            })
            .with_context(|| self.parallel_funding_status(pair, &attempts))?;
            // The request that hit the fault completes its delay even after healing.
            slow.heal(&mut self.runtime);
        }

        let mut asymmetric = BTreeSet::new();
        let outcomes = self
            .until("parallel funding outcomes after recovery", |s| {
                for attempt in &attempts {
                    let channel = attempt.channel;
                    let aborts = pair.map(|node| s.runtime.funding_aborted(node, channel));
                    if aborts[0] != aborts[1] && asymmetric.insert(channel) {
                        let node = if aborts[0] { a } else { b };
                        s.runtime.note(format_args!(
                            "parallel funding one-sided abort {} channel={channel}",
                            NAMES[node]
                        ));
                    }
                }
                if !s.runtime.link(a, b)
                    || s.chaos
                        .as_ref()
                        .is_some_and(|chaos| !chaos.open_ready(a, b))
                {
                    return Ok(None);
                }
                Ok(attempts
                    .iter()
                    .map(|attempt| attempt.outcome(&s.runtime))
                    .collect::<Option<Vec<_>>>())
            })
            .with_context(|| self.parallel_funding_status(pair, &attempts))?;
        self.finish_open_faults();
        self.protect_open(a, b)?;

        let mut edges = Vec::new();
        for (index, (attempt, outcome)) in attempts.iter().zip(outcomes).enumerate() {
            let channel = attempt.channel;
            match outcome {
                Outcome::Aborted => ensure!(
                    drop_victim == Some(index) || matches!(fault, BatchFault::Chaos(_)),
                    "parallel funding unexpectedly aborted {channel}"
                ),
                Outcome::Online => ensure!(
                    drop_victim != Some(index),
                    "parallel funding {channel} unexpectedly online after losing its funding frame"
                ),
            }
            if let Some(edge) = attempt.verify(&self.runtime, outcome, fee)? {
                edges.push(edge);
            } else {
                self.runtime.note(format_args!(
                    "parallel funding aborted channel={channel} without chain residue"
                ));
            }
        }
        ensure!(
            !edges.is_empty(),
            "parallel funding left no usable channel for {}-{}",
            NAMES[a],
            NAMES[b]
        );
        self.check_funded_pair(a, b, &edges, funding_txs)?;
        for edge in &mut edges {
            self.probe_funded_edge(edge)?;
            *edge = self.opened_edge(edge.sender, edge.receiver, edge.channel, edge.fee)?;
        }
        if let Some(chaos) = &mut self.chaos {
            chaos.unprotect_open();
        }
        self.runtime.note(format_args!(
            "parallel funding reconciled attempts={} online={} aborted={} probes={}",
            attempts.len(),
            edges.len(),
            attempts.len() - edges.len(),
            edges.len()
        ));
        Ok(edges)
    }

    fn parallel_funding_status(&self, pair: [usize; 2], attempts: &[Attempt]) -> String {
        let mut outcomes = Vec::new();
        for attempt in attempts {
            let channel = attempt.channel;
            let endpoints = pair.map(|node| {
                (
                    NAMES[node],
                    self.runtime.funding_aborted(node, channel),
                    self.runtime
                        .channel(node, &channel)
                        .map(|state| (state.state, state.connectivity_state)),
                )
            });
            outcomes.push(format!("{channel}: {endpoints:?}"));
        }
        format!(
            "parallel funding endpoints (node, abort receipt, persisted state): {}",
            outcomes.join("; ")
        )
    }

    fn probe_funded_edge(&mut self, edge: &Edge) -> Result<()> {
        const AMOUNT: u128 = 10_000;
        let channel = edge.channel;
        let outpoint = self
            .runtime
            .channel(edge.sender, &channel)
            .and_then(|state| state.get_funding_transaction_outpoint())
            .context("probe channel has no funding outpoint")?;
        let reply = self.runtime.request(edge.sender, |reply| {
            FiberActorCommand::SendPaymentWithRouter(
                SendPaymentWithRouterCommand {
                    keysend: Some(true),
                    router: vec![RouterHop {
                        target: self.runtime.peers[edge.receiver],
                        channel_outpoint: outpoint.clone(),
                        amount_received: AMOUNT,
                        incoming_tlc_expiry: DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                    }],
                    ..Default::default()
                },
                reply,
            )
        })?;
        let hash = self
            .reply("submit parallel funding probe", reply)
            .with_context(|| format!("submit probe for {channel}"))?
            .payment_hash;
        self.until("parallel funding probe settles on its exact channel", |s| {
            let status = s.runtime.payment_status(edge.sender, hash);
            ensure!(
                status != Some(PaymentStatus::Failed),
                "parallel funding probe {hash} failed on {channel}"
            );
            let [a, b] = [edge.sender, edge.receiver].map(|node| s.runtime.channel(node, &channel));
            Ok((status == Some(PaymentStatus::Success)
                && a.zip(b).is_some_and(|(a, b)| {
                    !a.any_tlc_pending()
                        && !b.any_tlc_pending()
                        && a.get_local_balance() == edge.before[0] - AMOUNT
                        && b.get_local_balance() == edge.before[1] + AMOUNT
                        && a.get_remote_balance() == b.get_local_balance()
                        && b.get_remote_balance() == a.get_local_balance()
                }))
            .then_some(()))
        })
        .with_context(|| format!("parallel funding probe {hash} channel={channel}"))?;
        self.wait_payment_actor(edge.sender)?;
        self.runtime.note(format_args!(
            "parallel funding probe settled channel={channel} payment={hash}"
        ));
        Ok(())
    }
}
