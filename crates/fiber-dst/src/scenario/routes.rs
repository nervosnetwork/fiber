use std::collections::BTreeSet;

use anyhow::{ensure, Context, Result};
use fnn::fiber::graph::NetworkGraphStateStore;
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{ChannelConnectivityState, ChannelState, Hash256, PaymentSession, PaymentStatus};
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use rand::{rngs::StdRng, Rng, SeedableRng};
use secp256k1::SECP256K1;

use crate::host::NAMES;

use super::{
    faults::{Fault, ProtocolPoint},
    funding::{Edge, OpeningFault},
    Driver, FUNDING_AMOUNT,
};

impl Driver {
    pub(super) fn multihop(&mut self) -> Result<()> {
        const FEE_PPM: u128 = 10_000;
        const AMOUNT: u128 = 10_000;
        const FEE: u128 = AMOUNT * FEE_PPM / 1_000_000;
        let ab = self.public_edge(0, 1, FUNDING_AMOUNT, 0)?;
        let bc = self.public_edge(1, 2, FUNDING_AMOUNT, FEE_PPM)?;
        let edges = [ab, bc];
        self.step()?;
        let cut = Fault::ProtocolPause {
            node: 1,
            channel: edges[1].channel,
            point: ProtocolPoint::ForwardFulfill,
            preceding_hits: 0,
        }
        .inject(&mut self.runtime)?;
        let payment = self.pay(
            0,
            SendPaymentCommand {
                target_pubkey: Some(self.runtime.peers[2]),
                amount: Some(AMOUNT),
                max_fee_amount: Some(FEE * 2),
                max_fee_rate: Some(20),
                keysend: Some(true),
                ..Default::default()
            },
        )?;
        self.until("relay has persisted its downstream fulfillment", |s| {
            s.route_safety(&edges)?;
            Ok(cut.hit(&s.runtime).then_some(()))
        })?;
        ensure!(
            self.runtime.payment_status(0, payment) != Some(PaymentStatus::Success),
            "payer received fulfillment before the forwarding fault"
        );
        self.runtime.note(format_args!(
            "B persisted downstream fulfillment before relay {payment}"
        ));
        let crash = Fault::Crash(1).inject(&mut self.runtime)?;
        cut.heal(&mut self.runtime);
        self.wait_links_down(1, &[0, 2])?;
        crash.heal(&mut self.runtime);

        let session = self.finish_route(payment, &edges)?;
        ensure!(
            session
                .attempts()
                .filter(|attempt| attempt.is_success())
                .count()
                == 1,
            "expected one successful attempt"
        );
        self.check_edge(&edges[0], AMOUNT + FEE)?;
        self.check_edge(&edges[1], AMOUNT)?;
        self.runtime.note(format_args!(
            "multihop settled {payment} amount={AMOUNT} relay_fee={FEE}"
        ));
        Ok(())
    }

    pub(super) fn mpp(&mut self, seed: u64) -> Result<()> {
        let ab = self.public_edge(0, 1, FUNDING_AMOUNT, 0)?;
        let ac = self.public_edge(0, 2, FUNDING_AMOUNT * 4 / 5, 0)?;
        let bd = self.public_edge(1, 3, FUNDING_AMOUNT, 0)?;
        let cd = self.public_edge(2, 3, FUNDING_AMOUNT * 4 / 5, 0)?;
        let edges = [ab, ac, bd, cd];
        self.step()?;

        let amount = FUNDING_AMOUNT * 7 / 5;
        ensure!(
            amount > edges[0].before[0].min(edges[2].before[0])
                && amount > edges[1].before[0].min(edges[3].before[0]),
            "invoice can fit in a single path"
        );
        let mut random = StdRng::seed_from_u64(seed ^ 0x0069_6e76_6f69);
        let preimage = Hash256::from(random.random::<[u8; 32]>());
        let payee_key = fnn::fiber::Privkey::from([4; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_preimage(preimage)
            .payment_secret(Hash256::from(random.random::<[u8; 32]>()))
            .payee_pub_key(self.runtime.peers[3].into())
            .allow_mpp(true)
            .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0))?;
        self.runtime.nodes.borrow()[3]
            .node
            .as_ref()
            .context("payee offline")?
            .store
            .insert_invoice(invoice.clone(), Some(preimage))?;

        let cut = Fault::ProtocolPause {
            node: 2,
            channel: edges[1].channel,
            point: ProtocolPoint::CommitmentAck,
            preceding_hits: 0,
        }
        .inject(&mut self.runtime)?;
        let payment = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                max_parts: Some(2),
                ..Default::default()
            },
        )?;
        self.until(
            "C holds the commitment and both MPP parts are in flight",
            |s| {
                s.route_safety(&edges)?;
                let both_inflight = s.payer_session(payment).is_some_and(|session| {
                    [1, 2].into_iter().all(|relay| {
                        session.attempts().any(|attempt| {
                            attempt.is_inflight()
                                && attempt
                                    .route
                                    .nodes
                                    .get(1)
                                    .is_some_and(|hop| hop.pubkey == s.runtime.peers[relay])
                        })
                    })
                });
                Ok((cut.hit(&s.runtime) && both_inflight).then_some(()))
            },
        )?;
        self.runtime.note(format_args!(
            "C paused one MPP path with two parts in flight {payment}"
        ));
        let crash = Fault::Crash(2).inject(&mut self.runtime)?;
        cut.heal(&mut self.runtime);
        self.wait_links_down(2, &[0, 3])?;
        crash.heal(&mut self.runtime);

        let session = self.finish_route(payment, &edges)?;
        let mut shard_ids = BTreeSet::new();
        let mut totals = [None, None];
        for attempt in session.attempts().filter(|attempt| attempt.is_success()) {
            ensure!(
                shard_ids.insert(attempt.id),
                "duplicate successful shard {}",
                attempt.id
            );
            let nodes = &attempt.route.nodes;
            ensure!(
                nodes.len() == 3
                    && nodes[0].pubkey == self.runtime.peers[0]
                    && nodes[2].pubkey == self.runtime.peers[3],
                "unexpected MPP route {nodes:?}"
            );
            let path = if nodes[1].pubkey == self.runtime.peers[1] {
                0
            } else {
                ensure!(nodes[1].pubkey == self.runtime.peers[2], "unexpected relay");
                1
            };
            ensure!(
                totals[path]
                    .replace(attempt.route.receiver_amount())
                    .is_none(),
                "more than one successful shard on a path"
            );
        }
        let left = totals[0].context("no successful MPP shard through B")?;
        let right = totals[1].context("no successful MPP shard through C")?;
        ensure!(
            left > 0 && right > 0 && left + right == amount,
            "MPP received {left} + {right}, expected {amount}"
        );
        for (edge, expected) in edges.iter().zip([left, right, left, right]) {
            self.check_edge(edge, expected)?;
        }
        let nodes = self.runtime.nodes.borrow();
        let payee = nodes[3].node.as_ref().context("payee offline")?;
        ensure!(
            payee.store.get_invoice_status(&payment) == Some(CkbInvoiceStatus::Paid),
            "MPP invoice was not paid"
        );
        self.runtime.note(format_args!(
            "MPP settled {payment} parts=[{left}, {right}] amount={amount}"
        ));
        Ok(())
    }

    pub(super) fn public_edge(
        &mut self,
        sender: usize,
        receiver: usize,
        capacity: u128,
        fee: u128,
    ) -> Result<Edge> {
        self.open_between(sender, receiver, capacity, fee, true, OpeningFault::None)
    }

    fn payer_session(&self, hash: Hash256) -> Option<PaymentSession> {
        self.runtime.nodes.borrow()[0]
            .node
            .as_ref()?
            .store
            .get_payment_session(hash)
    }

    fn wait_links_down(&mut self, node: usize, neighbors: &[usize]) -> Result<()> {
        self.until("surviving peers observe the relay crash", |s| {
            let nodes = s.runtime.nodes.borrow();
            Ok(neighbors
                .iter()
                .all(|peer| !nodes[*peer].connected.contains(&node))
                .then_some(()))
        })
    }

    fn route_safety(&self, edges: &[Edge]) -> Result<()> {
        for edge in edges {
            for node in [edge.sender, edge.receiver] {
                if let Some(state) = self.runtime.channel(node, &edge.channel) {
                    ensure!(
                        state.get_local_balance() + state.get_remote_balance()
                            == edge.before.iter().sum::<u128>(),
                        "{} changed the capacity of {}",
                        NAMES[node],
                        edge.channel
                    );
                }
            }
        }
        Ok(())
    }

    fn finish_route(&mut self, hash: Hash256, edges: &[Edge]) -> Result<PaymentSession> {
        self.until("route settled and every channel reestablished", |s| {
            s.route_safety(edges)?;
            let settled = edges.iter().all(|edge| {
                s.runtime.link(edge.sender, edge.receiver)
                    && [edge.sender, edge.receiver].into_iter().all(|node| {
                        s.runtime.channel(node, &edge.channel).is_some_and(|state| {
                            state.state == ChannelState::ChannelReady
                                && state.connectivity_state == ChannelConnectivityState::Online
                                && !state.tlc_state.waiting_ack
                                && state.tlc_state.all_tlcs().next().is_none()
                        })
                    })
            });
            Ok((settled
                && s.runtime
                    .payment_status(0, hash)
                    .is_some_and(|status| status.is_final()))
            .then_some(()))
        })
        .with_context(|| format!("payment {hash}: {:?}", self.payer_session(hash)))?;
        self.wait_payment_actor(0)?;
        ensure!(
            self.runtime.payment_status(0, hash) == Some(PaymentStatus::Success),
            "route payment failed: {:?}",
            self.payer_session(hash)
        );
        self.payer_session(hash).context("no settled payer session")
    }

    fn check_edge(&self, edge: &Edge, amount: u128) -> Result<()> {
        let a = self
            .runtime
            .channel(edge.sender, &edge.channel)
            .context("missing sender state")?;
        let b = self
            .runtime
            .channel(edge.receiver, &edge.channel)
            .context("missing receiver state")?;
        let expected = edge.expected(amount)?;
        ensure!(
            [a.get_local_balance(), a.get_remote_balance()] == expected
                && [b.get_remote_balance(), b.get_local_balance()] == expected,
            "channel {} did not settle {amount} exactly once",
            edge.channel
        );
        let a_round = a.get_current_commitment_numbers();
        let b_round = b.get_current_commitment_numbers();
        ensure!(
            a_round.local == b_round.remote
                && a_round.remote == b_round.local
                && a.tlc_state.offered_tlcs.next_tlc_id == b.tlc_state.received_tlcs.next_tlc_id
                && a.tlc_state.received_tlcs.next_tlc_id == b.tlc_state.offered_tlcs.next_tlc_id,
            "channel {} did not converge",
            edge.channel
        );
        Ok(())
    }
}
