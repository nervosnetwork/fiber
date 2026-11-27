use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use fnn::fiber::config::DEFAULT_HOLD_TLC_TIMEOUT;
use fnn::fiber::graph::NetworkGraphStateStore;
use fnn::fiber::network::SendPaymentResponse;
use fnn::fiber::payment::{PaymentSessionExt, SendPaymentCommand};
use fnn::fiber::{FiberActorCommand, Hash256, PaymentSession, PaymentStatus, Privkey};
use fnn::invoice::{Currency, InvoiceBuilder, InvoiceStore};
use fnn::simulation;
use rand::{rngs::StdRng, Rng, SeedableRng};
use secp256k1::SECP256K1;
use tokio::sync::oneshot;

use crate::host::NAMES;

use super::{
    faults::{schedule_fault_windows, FaultWindow},
    funding::Edge,
    Driver, FUNDING_AMOUNT, PHASE_TICKS, TICK,
};

mod ledger;
pub(super) mod random;
use ledger::Ledger;

const RELAY_FEE_PPM: u128 = 1_000;
const MPP_AMOUNT: u128 = FUNDING_AMOUNT * 6 / 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Direct,
    Multihop,
    Mpp,
}

struct Order {
    tick: usize,
    from: usize,
    to: usize,
    kind: Kind,
    amount: u128,
}

struct Payment {
    order: Order,
    hash: Hash256,
    invoice: String,
    admission: Admission,
}

enum Admission {
    Scheduled,
    Submitted(oneshot::Receiver<Result<SendPaymentResponse, String>>),
    Accepted,
    Rejected,
    ReplyLost,
}

struct WaveResult {
    peak: usize,
    fault_overlapped_payment: bool,
}

impl Driver {
    pub(super) fn swarm(&mut self, seed: u64, links: &[(usize, usize)]) -> Result<()> {
        let edges = links
            .iter()
            .map(|&(from, to)| self.public_edge(from, to, FUNDING_AMOUNT, RELAY_FEE_PPM))
            .collect::<Result<Vec<_>>>()?;
        self.step()?;
        simulation::observe_tlc_settlements();
        let mut ledger = Ledger::new(&edges);
        let mut workload = StdRng::seed_from_u64(seed ^ 0x0073_776f_726b);
        let mut faults = StdRng::seed_from_u64(seed ^ 0x0073_6661_756c);
        let mut invoices = StdRng::seed_from_u64(seed ^ 0x0073_696e_766f);

        let baseline = [
            order(0, 0, Kind::Mpp, MPP_AMOUNT),
            order(0, 3, Kind::Mpp, MPP_AMOUNT),
            order(0, 1, Kind::Multihop, 100_000),
            order(0, 2, Kind::Multihop, 100_000),
        ];
        let mut payments = self.prepare_payments(baseline, &mut invoices, &mut ledger)?;
        let baseline = self.drive_payment_wave("baseline", &mut payments, &mut [], &mut ledger)?;
        ensure!(
            baseline.peak >= 2,
            "baseline never had concurrent payments in flight"
        );
        ledger.check_payments(self, &payments, true)?;

        let mut stress = vec![
            order(0, 0, Kind::Mpp, MPP_AMOUNT),
            order(0, 3, Kind::Mpp, MPP_AMOUNT),
            order(0, 1, Kind::Multihop, 100_000),
            order(0, 2, Kind::Multihop, 100_000),
        ];
        for tick in 1..=6 {
            let from = workload.random_range(0..self.runtime.peers.len());
            let kind = [Kind::Direct, Kind::Multihop, Kind::Mpp][workload.random_range(0..3)];
            let amount = if kind == Kind::Mpp {
                MPP_AMOUNT
            } else {
                workload.random_range(100_000..1_000_000)
            };
            stress.push(order(
                workload.random_range(tick * 2..=tick * 2 + 1),
                from,
                kind,
                amount,
            ));
        }
        let mut stress = self.prepare_payments(stress, &mut invoices, &mut ledger)?;
        let mut windows = schedule_fault_windows(&mut faults, links, self.runtime.peers.len(), 100);
        let wave =
            self.drive_payment_wave("fault workload", &mut stress, &mut windows, &mut ledger)?;
        ensure!(
            wave.fault_overlapped_payment,
            "no scheduled fault overlapped an in-flight payment"
        );
        ledger.check_payments(self, &stress, false)?;

        let probes = [
            order(0, 0, Kind::Direct, 10_000),
            order(0, 3, Kind::Direct, 10_000),
            order(0, 1, Kind::Multihop, 10_000),
            order(0, 2, Kind::Multihop, 10_000),
        ];
        let mut probes = self.prepare_payments(probes, &mut invoices, &mut ledger)?;
        self.drive_payment_wave("recovery probes", &mut probes, &mut [], &mut ledger)?;
        ledger.check_payments(self, &probes, true)?;
        ledger.check_consumed()?;
        self.runtime
            .note("swarm reconciled all payments, TLC fulfillments and channels");
        Ok(())
    }

    fn prepare_payments(
        &self,
        orders: impl IntoIterator<Item = Order>,
        invoices: &mut StdRng,
        ledger: &mut Ledger<'_>,
    ) -> Result<Vec<Payment>> {
        orders
            .into_iter()
            .map(|order| {
                let preimage = Hash256::from(invoices.random::<[u8; 32]>());
                let key = Privkey::from([(order.to + 1) as u8; 32]);
                let invoice = InvoiceBuilder::new(Currency::Fibd)
                    .amount(Some(order.amount))
                    .payment_preimage(preimage)
                    .payment_secret(Hash256::from(invoices.random::<[u8; 32]>()))
                    .payee_pub_key(self.runtime.peers[order.to].into())
                    .allow_mpp(order.kind == Kind::Mpp)
                    .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &key.0))?;
                let hash = *invoice.payment_hash();
                self.runtime.nodes.borrow()[order.to]
                    .node
                    .as_ref()
                    .context("payee offline while preparing workload")?
                    .store
                    .insert_invoice(invoice.clone(), Some(preimage))?;
                ledger.register(hash, order.to, order.amount)?;
                Ok(Payment {
                    order,
                    hash,
                    invoice: invoice.to_string(),
                    admission: Admission::Scheduled,
                })
            })
            .collect()
    }

    fn drive_payment_wave(
        &mut self,
        phase: &str,
        payments: &mut [Payment],
        faults: &mut [FaultWindow],
        ledger: &mut Ledger<'_>,
    ) -> Result<WaveResult> {
        self.runtime.note(format_args!("swarm {phase} starts"));
        let last_fault = faults
            .iter()
            .map(|window| window.end)
            .max()
            .unwrap_or_default();
        // A partial MPP may legitimately hold funds until the protocol timer expires.
        let budget = TICK * PHASE_TICKS as u32
            + if faults.is_empty() && self.chaos.is_none() {
                Duration::ZERO
            } else {
                Duration::from_millis(DEFAULT_HOLD_TLC_TIMEOUT)
            };
        let last_order = payments
            .iter()
            .map(|p| p.order.tick)
            .max()
            .unwrap_or_default();
        let mut max_ticks = last_order + (budget.as_millis() / TICK.as_millis()) as usize;
        let mut peak = 0;
        let mut faults_during_payment = false;
        let mut completed = vec![false; payments.len()];
        let mut tick = 0;
        while tick < max_ticks {
            for window in faults.iter_mut() {
                window.advance(tick, &mut self.runtime)?;
            }
            for payment in payments.iter_mut() {
                let order = &payment.order;
                if matches!(payment.admission, Admission::Scheduled)
                    && tick >= order.tick
                    && self.runtime.node_available(order.from)
                {
                    let reply = self.runtime.request(order.from, |reply| {
                        FiberActorCommand::SendPayment(
                            SendPaymentCommand {
                                invoice: Some(payment.invoice.clone()),
                                max_parts: Some(if order.kind == Kind::Mpp { 2 } else { 1 }),
                                max_fee_rate: Some(5),
                                ..Default::default()
                            },
                            reply,
                        )
                    })?;
                    payment.admission = Admission::Submitted(reply);
                    self.runtime.note(format_args!(
                        "swarm submit {:?} {}->{} amount={} hash={}",
                        order.kind, NAMES[order.from], NAMES[order.to], order.amount, payment.hash
                    ));
                }
            }
            self.step()?;
            if self.runtime.watchtowers.is_some()
                && self.runtime.elapsed().as_millis().is_multiple_of(250)
            {
                self.scan_onchain(ledger.onchain_endpoints(self))?;
            }
            ledger.observe(self)?;

            let mut in_flight = 0;
            let mut pending = 0;
            let mut relays = BTreeSet::new();
            for (payment, completed) in payments.iter_mut().zip(&mut completed) {
                if *completed {
                    continue;
                }
                if let Admission::Submitted(reply) = &mut payment.admission {
                    match reply.try_recv() {
                        Ok(Ok(result)) => {
                            ensure!(
                                result.payment_hash == payment.hash,
                                "payment admission changed hash"
                            );
                            payment.admission = Admission::Accepted;
                        }
                        Ok(Err(_)) => payment.admission = Admission::Rejected,
                        Err(oneshot::error::TryRecvError::Closed) => {
                            payment.admission = Admission::ReplyLost
                        }
                        Err(oneshot::error::TryRecvError::Empty) => {}
                    }
                }
                let session = self.swarm_session(payment);
                let active = session
                    .as_ref()
                    .is_some_and(|session| session.attempts().any(|attempt| attempt.is_inflight()));
                if let Some(session) = session.as_ref().filter(|_| self.chaos.is_some()) {
                    for attempt in session.attempts().filter(|attempt| attempt.is_inflight()) {
                        let hops = &attempt.route.nodes;
                        for hop in hops.iter().skip(1).take(hops.len().saturating_sub(2)) {
                            if let Some(node) = self
                                .runtime
                                .peers
                                .iter()
                                .position(|peer| *peer == hop.pubkey)
                            {
                                relays.insert(node);
                            }
                        }
                    }
                }
                in_flight += usize::from(active);
                let reply_pending = matches!(
                    payment.admission,
                    Admission::Scheduled | Admission::Submitted(_)
                );
                let status_pending = match session {
                    Some(session) => !self.swarm_status(payment, &session)?.is_final(),
                    None => matches!(payment.admission, Admission::Accepted),
                };
                let unfinished = reply_pending || status_pending;
                pending += usize::from(unfinished);
                *completed =
                    !unfinished && !active && self.runtime.node_available(payment.order.from);
            }
            peak = peak.max(in_flight);
            let (chaos_active, chaos_drained) =
                self.chaos.as_mut().map_or((false, true), |chaos| {
                    chaos.set_relays(relays);
                    (
                        chaos.fault_active(),
                        chaos.quiescent(self.runtime.elapsed()),
                    )
                });
            if in_flight > 0 && (chaos_active || faults.iter().any(FaultWindow::is_active)) {
                faults_during_payment = true;
            }
            if tick >= last_fault
                && chaos_drained
                && pending == 0
                && in_flight == 0
                && ledger.channels_drained(self)
            {
                for node in 0..self.runtime.peers.len() {
                    self.wait_payment_actor(node)?;
                }
                ledger.observe(self)?;
                self.runtime.note(format_args!(
                    "swarm {phase} drained peak_inflight={peak} faults_during_payment={faults_during_payment}"
                ));
                return Ok(WaveResult {
                    peak,
                    fault_overlapped_payment: faults_during_payment,
                });
            }
            tick += 1;
            if tick == max_ticks
                && tick >= last_fault
                && chaos_drained
                && self.runtime.watchtowers.is_some()
            {
                if let Some(expiry) = ledger.next_onchain_expiry(self) {
                    // The fault schedule uses Turmoil elapsed time. Only the Unix clock
                    // shared by Fiber and the synthetic CKB advances between faults.
                    self.advance_clock_past(expiry);
                    max_ticks += PHASE_TICKS;
                }
            }
        }
        let states: Vec<_> = payments
            .iter()
            .map(|payment| {
                let session = self.swarm_session(payment);
                let status = session.as_ref().map(|session| session.status);
                let unfinished_attempts =
                    session
                        .filter(|session| !session.status.is_final())
                        .map(|session| {
                            session
                                .attempts()
                                .map(|attempt| {
                                    (attempt.id, attempt.status, attempt.last_error.clone())
                                })
                                .collect::<Vec<_>>()
                        });
                (payment.hash, status, unfinished_attempts)
            })
            .collect();
        bail!("swarm {phase} failed to drain after {} virtual ms\npayments (hash, status, attempts for non-final sessions): {states:?}\nchannels: {:#?}",
            TICK.as_millis() * (max_ticks - last_order) as u128, ledger.channel_diagnostics(self))
    }

    fn swarm_session(&self, payment: &Payment) -> Option<PaymentSession> {
        if !self.runtime.node_available(payment.order.from) {
            return None;
        }
        self.runtime.nodes.borrow()[payment.order.from]
            .node
            .as_ref()?
            .store
            .get_payment_session(payment.hash)
    }

    fn swarm_status(&self, payment: &Payment, session: &PaymentSession) -> Result<PaymentStatus> {
        let nodes = self.runtime.nodes.borrow();
        let payer = nodes[payment.order.from]
            .node
            .as_ref()
            .context("payer offline")?;
        Ok(session.effective_status(&payer.store))
    }
}

fn order(tick: usize, from: usize, kind: Kind, amount: u128) -> Order {
    let to = if kind == Kind::Direct {
        from ^ 1
    } else {
        from ^ 3
    };
    Order {
        tick,
        from,
        to,
        kind,
        amount,
    }
}
