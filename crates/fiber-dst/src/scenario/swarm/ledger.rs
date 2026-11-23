use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use fnn::fiber::channel::ChannelActorStateStore;
use fnn::fiber::{
    AppliedFlags, Attempt, ChannelConnectivityState, ChannelState, Hash256, PaymentStatus, Pubkey,
};
use fnn::invoice::{CkbInvoiceStatus, InvoiceStore};
use fnn::simulation::{self, TlcSettlement};

use crate::host::NAMES;

use super::{Admission, Driver, Edge, Kind, Payment};

/// A channel's TLC counter is scoped to the offering endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TlcKey {
    channel: Hash256,
    from: usize,
    id: u64,
}

#[derive(Default)]
struct Fulfillment {
    offered: Option<TlcSettlement>,
    received: Option<TlcSettlement>,
}

struct InvoiceIntent {
    recipient: usize,
    amount: u128,
    delivered: u128,
    started_shards: BTreeSet<u64>,
}

pub(super) struct Ledger<'a> {
    edges: &'a [Edge],
    balances: Vec<[u128; 2]>,
    payments: BTreeMap<Hash256, InvoiceIntent>,
    tlcs: BTreeMap<TlcKey, Fulfillment>,
    consumed: BTreeSet<TlcKey>,
}

impl<'a> Ledger<'a> {
    pub(super) fn new(edges: &'a [Edge]) -> Self {
        Self {
            edges,
            balances: edges.iter().map(|edge| edge.before).collect(),
            payments: BTreeMap::new(),
            tlcs: BTreeMap::new(),
            consumed: BTreeSet::new(),
        }
    }

    pub(super) fn register(
        &mut self,
        payment: Hash256,
        recipient: usize,
        amount: u128,
    ) -> Result<()> {
        let intent = InvoiceIntent {
            recipient,
            amount,
            delivered: 0,
            started_shards: BTreeSet::new(),
        };
        ensure!(
            self.payments.insert(payment, intent).is_none(),
            "duplicate invoice hash {payment}"
        );
        Ok(())
    }

    pub(super) fn observe(&mut self, driver: &Driver) -> Result<()> {
        for receipt in simulation::take_tlc_settlements() {
            let (key, node) = self.record(&driver.runtime.peers, &receipt)?;
            driver.runtime.note(format_args!(
                "swarm fulfilled {} on {} from {} TLC {} at {} amount={}",
                receipt.payment, key.channel, NAMES[key.from], key.id, NAMES[node], receipt.amount
            ));
        }
        for (edge, balances) in self.edges.iter().zip(&self.balances) {
            let capacity: u128 = edge.before.iter().sum();
            for (side, node) in [edge.sender, edge.receiver].into_iter().enumerate() {
                if let Some(state) = driver.runtime.channel(node, &edge.channel) {
                    let local = state.get_local_balance();
                    let remote = state.get_remote_balance();
                    ensure!(
                        local == balances[side] && remote == capacity - balances[side],
                        "channel {} node {} balance mismatch: expected {}/{}, got {local}/{remote}",
                        edge.channel,
                        NAMES[node],
                        balances[side],
                        capacity - balances[side]
                    );
                }
            }
        }
        Ok(())
    }

    fn record(&mut self, peers: &[Pubkey], receipt: &TlcSettlement) -> Result<(TlcKey, usize)> {
        let intent = self
            .payments
            .get_mut(&receipt.payment)
            .with_context(|| format!("fulfillment for unissued payment: {receipt:?}"))?;
        let index = self
            .edges
            .iter()
            .position(|edge| edge.channel == receipt.channel)
            .with_context(|| format!("fulfillment on unknown channel: {receipt:?}"))?;
        let edge = &self.edges[index];
        let nodes = [edge.sender, edge.receiver];
        let side = nodes
            .iter()
            .position(|&node| peers[node] == receipt.node)
            .with_context(|| format!("fulfillment at unknown endpoint: {receipt:?}"))?;
        let key = TlcKey {
            channel: receipt.channel,
            from: nodes[if receipt.offered { side } else { 1 - side }],
            id: receipt.id,
        };
        let fulfillment = self.tlcs.entry(key).or_default();
        let (own, peer) = if receipt.offered {
            (&mut fulfillment.offered, &fulfillment.received)
        } else {
            (&mut fulfillment.received, &fulfillment.offered)
        };
        ensure!(
            own.is_none(),
            "duplicate durable fulfillment: {key:?} {receipt:?}"
        );
        if let Some(peer) = peer {
            ensure!(
                (receipt.payment, receipt.amount) == (peer.payment, peer.amount),
                "channel endpoints disagree on fulfilled TLC {key:?}: {receipt:?} vs {peer:?}"
            );
        }
        if let Some(attempt) = receipt
            .attempt
            .filter(|_| receipt.offered && receipt.previous.is_none())
        {
            ensure!(
                intent.started_shards.insert(attempt),
                "payment {} fulfilled attempt {attempt} more than once",
                receipt.payment
            );
        }
        if !receipt.offered && nodes[side] == intent.recipient {
            intent.delivered = intent
                .delivered
                .checked_add(receipt.amount)
                .context("invoice receipts overflowed")?;
            ensure!(
                intent.delivered <= intent.amount,
                "overpaid invoice {}: delivered {}, expected {}",
                receipt.payment,
                intent.delivered,
                intent.amount
            );
        }
        let balance = &mut self.balances[index][side];
        *balance = if receipt.offered {
            balance.checked_sub(receipt.amount)
        } else {
            balance.checked_add(receipt.amount)
        }
        .with_context(|| format!("invalid balance change: {receipt:?}"))?;
        ensure!(
            *balance <= edge.before.iter().sum(),
            "channel capacity exceeded: {receipt:?}"
        );
        *own = Some(receipt.clone());
        Ok((key, nodes[side]))
    }

    pub(super) fn channels_drained(&self, driver: &Driver) -> bool {
        self.edges.iter().all(|edge| {
            driver.runtime.link(edge.sender, edge.receiver)
                && [edge.sender, edge.receiver].into_iter().all(|node| {
                    driver
                        .runtime
                        .channel(node, &edge.channel)
                        .is_some_and(|state| {
                            state.state == ChannelState::ChannelReady
                            && state.connectivity_state == ChannelConnectivityState::Online
                            && !state.tlc_state.waiting_ack
                            // Applied failures are retained until the next commitment.
                            && state.tlc_state.all_tlcs().all(|tlc| {
                                tlc.is_fail_remove_confirmed()
                                    && tlc.removed_confirmed_at.is_some()
                                    && tlc.applied_flags.contains(AppliedFlags::REMOVE)
                            })
                        })
                })
        })
    }

    pub(super) fn channel_diagnostics(&self, driver: &Driver) -> Vec<String> {
        self.edges
            .iter()
            .flat_map(|edge| {
                [edge.sender, edge.receiver].map(|node| {
                    let state = driver.runtime.channel(node, &edge.channel);
                    let summary = state.as_ref().map(|state| {
                        let tlcs = state
                            .tlc_state
                            .all_tlcs()
                            .map(|tlc| {
                                (
                                    tlc.payment_hash,
                                    tlc.tlc_id,
                                    &tlc.status,
                                    &tlc.applied_flags,
                                )
                            })
                            .collect::<Vec<_>>();
                        (
                            state.state,
                            state.connectivity_state,
                            state.tlc_state.waiting_ack,
                            tlcs,
                        )
                    });
                    format!("{} {} {summary:?}", NAMES[node], edge.channel)
                })
            })
            .collect()
    }

    pub(super) fn check_payments(
        &mut self,
        driver: &Driver,
        payments: &[Payment],
        must_succeed: bool,
    ) -> Result<usize> {
        let mut succeeded = 0;
        for payment in payments {
            let success = self
                .check_payment(driver, payment, must_succeed)
                .with_context(|| {
                    format!(
                        "payment {} {:?} {}->{} amount={}",
                        payment.hash,
                        payment.order.kind,
                        NAMES[payment.order.from],
                        NAMES[payment.order.to],
                        payment.order.amount
                    )
                })?;
            succeeded += usize::from(success);
        }
        for edge in self.edges {
            let a = driver
                .runtime
                .channel(edge.sender, &edge.channel)
                .context("missing channel sender")?;
            let b = driver
                .runtime
                .channel(edge.receiver, &edge.channel)
                .context("missing channel receiver")?;
            let a_round = a.get_current_commitment_numbers();
            let b_round = b.get_current_commitment_numbers();
            ensure!(
                a.get_local_balance() == b.get_remote_balance()
                    && a.get_remote_balance() == b.get_local_balance()
                    && a_round.local == b_round.remote
                    && a_round.remote == b_round.local
                    && a.tlc_state.offered_tlcs.next_tlc_id
                        == b.tlc_state.received_tlcs.next_tlc_id
                    && a.tlc_state.received_tlcs.next_tlc_id
                        == b.tlc_state.offered_tlcs.next_tlc_id,
                "channel {} endpoints did not converge",
                edge.channel
            );
        }
        Ok(succeeded)
    }

    fn check_payment(
        &mut self,
        driver: &Driver,
        payment: &Payment,
        must_succeed: bool,
    ) -> Result<bool> {
        let session = driver.swarm_session(payment);
        let status = session
            .as_ref()
            .map(|session| driver.swarm_status(payment, session))
            .transpose()?;
        ensure!(
            status.is_none_or(|status| status.is_final()),
            "payment is still pending"
        );
        ensure!(
            status.is_some() || !matches!(payment.admission, Admission::Accepted),
            "admitted payment vanished"
        );
        let success = status == Some(PaymentStatus::Success);
        ensure!(
            !must_succeed || success,
            "healthy payment failed: {session:?}"
        );

        let nodes = driver.runtime.nodes.borrow();
        let payee = &nodes[payment.order.to]
            .node
            .as_ref()
            .context("payee offline")?
            .store;
        let invoice = payee
            .get_invoice_status(&payment.hash)
            .context("invoice missing")?;
        ensure!(
            invoice != CkbInvoiceStatus::Received,
            "invoice is still settling"
        );
        ensure!(
            (invoice == CkbInvoiceStatus::Paid) == success,
            "payer and payee disagree: payment={status:?} invoice={invoice:?}"
        );
        ensure!(
            payee.get_payment_hold_tlcs(payment.hash).is_empty(),
            "invoice still holds TLCs"
        );
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut amount = 0;
        let mut fee = 0;
        if let Some(session) = session {
            for attempt in session.attempts().filter(|attempt| attempt.is_success()) {
                ensure!(success, "failed payment has a successful attempt");
                ensure!(
                    ids.insert(attempt.id),
                    "duplicate successful attempt {}",
                    attempt.id
                );
                let (delivered, route_fee) = self.check_attempt(driver, payment, attempt)?;
                amount += delivered;
                fee += route_fee;
                paths.insert(Hash256::from(
                    attempt.route.nodes[0].channel_outpoint.tx_hash(),
                ));
            }
        }
        if success {
            ensure!(
                amount == payment.order.amount,
                "received {amount}, expected {}",
                payment.order.amount
            );
            ensure!(
                match payment.order.kind {
                    Kind::Mpp if must_succeed => ids.len() == 2,
                    Kind::Mpp => (1..=2).contains(&ids.len()),
                    _ => ids.len() == 1,
                },
                "unexpected number of successful shards: {}",
                ids.len()
            );
            if must_succeed && payment.order.kind == Kind::Mpp {
                ensure!(paths.len() == 2, "healthy MPP did not use both paths");
            }
        }
        for (key, tlc) in &self.tlcs {
            if tlc
                .offered
                .as_ref()
                .or(tlc.received.as_ref())
                .is_some_and(|receipt| receipt.payment == payment.hash)
            {
                ensure!(
                    self.consumed.contains(key),
                    "fulfilled TLC {key:?} has no successful attempt"
                );
            }
        }
        driver.runtime.note(format_args!(
            "swarm audit {:?} {}->{} hash={} status={status:?} shards={} delivered={amount} fee={fee}",
            payment.order.kind, NAMES[payment.order.from], NAMES[payment.order.to], payment.hash, ids.len()
        ));
        Ok(success)
    }

    fn check_attempt(
        &mut self,
        driver: &Driver,
        payment: &Payment,
        attempt: &Attempt,
    ) -> Result<(u128, u128)> {
        let route = &attempt.route.nodes;
        ensure!(
            route.len() >= 2
                && route
                    .first()
                    .is_some_and(|hop| hop.pubkey == driver.runtime.peers[payment.order.from])
                && route
                    .last()
                    .is_some_and(|hop| hop.pubkey == driver.runtime.peers[payment.order.to]),
            "wrong route endpoints: {route:?}"
        );
        ensure!(
            payment.order.kind == Kind::Direct || route.len() >= 3,
            "multihop payment did not use a relay"
        );
        let mut previous: Option<(TlcKey, u128)> = None;
        for pair in route.windows(2) {
            let [from, to] = pair else { unreachable!() };
            let edge = self
                .edges
                .iter()
                .find(|edge| Hash256::from(from.channel_outpoint.tx_hash()) == edge.funding_tx)
                .context("route uses unknown funding outpoint")?;
            let sender = if driver.runtime.peers[edge.sender] == from.pubkey
                && driver.runtime.peers[edge.receiver] == to.pubkey
            {
                edge.sender
            } else {
                ensure!(
                    driver.runtime.peers[edge.receiver] == from.pubkey
                        && driver.runtime.peers[edge.sender] == to.pubkey,
                    "route uses channel {} with wrong endpoints",
                    edge.channel
                );
                edge.receiver
            };
            let mut matches = self.tlcs.iter().filter(|(key, tlc)| {
                key.channel == edge.channel
                    && key.from == sender
                    && tlc.offered.as_ref().is_some_and(|receipt| {
                        receipt.payment == payment.hash
                            && match previous {
                                Some((upstream, _)) => {
                                    receipt.previous == Some((upstream.channel, upstream.id))
                                }
                                None => {
                                    receipt.attempt == Some(attempt.id)
                                        && receipt.previous.is_none()
                                }
                            }
                    })
            });
            let (&key, tlc) = matches.next().with_context(|| {
                format!(
                    "no fulfilled TLC for attempt {} on {} from {} after {previous:?}",
                    attempt.id, edge.channel, NAMES[sender]
                )
            })?;
            ensure!(
                matches.next().is_none(),
                "multiple fulfilled TLCs for one attempt hop on {}",
                edge.channel
            );
            let sent = tlc.offered.as_ref().expect("matched offered receipt");
            ensure!(
                tlc.received.is_some(),
                "recipient did not durably apply TLC {key:?}"
            );
            ensure!(
                sent.amount == from.amount && sent.amount > 0,
                "wrong amount for TLC {key:?}: {sent:?}"
            );
            if let Some((_, incoming)) = previous {
                let fee_ppm = if sender == edge.sender { edge.fee } else { 0 };
                let fee = (sent.amount * fee_ppm).div_ceil(1_000_000);
                ensure!(
                    incoming == sent.amount + fee,
                    "wrong relay fee for TLC {key:?}: {incoming} -> {} expected fee {fee}",
                    sent.amount
                );
            }
            ensure!(
                self.consumed.insert(key),
                "TLC {key:?} credited to more than one shard"
            );
            previous = Some((key, sent.amount));
        }
        let delivered = previous.context("empty payment route")?.1;
        ensure!(
            delivered == attempt.route.receiver_amount(),
            "last hop disagrees with receiver amount"
        );
        Ok((delivered, route[0].amount - delivered))
    }

    pub(super) fn check_consumed(&self) -> Result<()> {
        ensure!(
            self.tlcs.len() == self.consumed.len(),
            "unaccounted durable fulfillments remain"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use fnn::fiber::{Hash256, Privkey};
    use fnn::simulation::TlcSettlement;

    use super::{Edge, Ledger};

    #[test]
    fn rejects_replayed_fulfillments_and_overpaid_invoices() {
        let payment = Hash256::from([1; 32]);
        let channel = Hash256::from([2; 32]);
        let peers = [
            Privkey::from([1; 32]).pubkey(),
            Privkey::from([2; 32]).pubkey(),
        ];
        let edges = [Edge {
            sender: 0,
            receiver: 1,
            channel,
            before: [1000; 2],
            funding_tx: Hash256::default(),
            fee: 0,
        }];
        let mut ledger = Ledger::new(&edges);
        ledger.register(payment, 1, 100).unwrap();
        let mut receipt = TlcSettlement {
            node: peers[0],
            channel,
            id: 0,
            offered: true,
            payment,
            amount: 100,
            attempt: Some(1),
            previous: None,
        };
        ledger.record(&peers, &receipt).unwrap();
        assert!(ledger
            .record(&peers, &receipt)
            .unwrap_err()
            .to_string()
            .contains("duplicate durable fulfillment"));

        receipt.id = 1;
        assert!(ledger
            .record(&peers, &receipt)
            .unwrap_err()
            .to_string()
            .contains("fulfilled attempt 1 more than once"));

        receipt.node = peers[1];
        receipt.offered = false;
        receipt.attempt = None;
        receipt.id = 0;
        receipt.amount = 99;
        assert!(ledger
            .record(&peers, &receipt)
            .unwrap_err()
            .to_string()
            .contains("channel endpoints disagree"));
        receipt.amount = 100;
        ledger.record(&peers, &receipt).unwrap();
        receipt.id = 1;
        assert!(ledger
            .record(&peers, &receipt)
            .unwrap_err()
            .to_string()
            .contains("overpaid invoice"));
    }
}
