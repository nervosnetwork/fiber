use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fnn::fiber::channel::{ChannelCommand, ChannelCommandWithId, ShutdownCommand};
use fnn::fiber::config::DEFAULT_FINAL_TLC_EXPIRY_DELTA;
use fnn::fiber::payment::SendPaymentWithRouterCommand;
use fnn::fiber::{ChannelState, FiberActorCommand, Hash256, NetworkActorMessage, PaymentStatus};
use fnn::fiber_types::{CloseFlags, HashAlgorithm, NodeId, RouterHop};
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use fnn::simulation::slow_mailbox;

use crate::chain_fault::{ChainEndpoint, ChainFault};
use fnn::watchtower::{WatchtowerMessage, WatchtowerStore};

use super::faults::Fault;
use super::{receive, Driver};
use crate::host::{FrameDelay, FrameLoss, NAMES};
use crate::tower::{Tower, TowerMode};

impl Driver {
    pub(super) fn confirm_force_close(
        &mut self,
        channel: Hash256,
        closer: usize,
        peer: usize,
    ) -> Result<Hash256> {
        self.runtime.note(format_args!(
            "chaos force close {}->{} channel={channel}",
            NAMES[closer], NAMES[peer]
        ));
        let reply = self.runtime.request(closer, |reply| {
            FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channel,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: None,
                        fee_rate: None,
                        force: true,
                    },
                    reply,
                ),
            })
        })?;
        self.reply("broadcast force close", reply)?;
        let tx_hash = self.until("force close confirmed locally", |s| {
            Ok(s.runtime.channel(closer, &channel).and_then(|state| {
                if matches!(state.state, ChannelState::Closed(flags)
                    if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL))
                {
                    state.shutdown_transaction_hash.clone()
                } else {
                    None
                }
            }))
        })?;
        let check = self.runtime.request(peer, |reply| {
            FiberActorCommand::CheckChannelShutdown(channel, reply)
        })?;
        self.reply("peer queries force close on chain", check)?;
        self.until("peer observes the same confirmed force close", |s| {
            Ok(s.runtime
                .channel(peer, &channel)
                .filter(|state| {
                    matches!(state.state, ChannelState::Closed(flags)
                    if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE))
                })
                .filter(|state| state.shutdown_transaction_hash.as_ref() == Some(&tx_hash))
                .map(|_| ()))
        })?;
        let tx_hash = Hash256::from(tx_hash);
        self.runtime.note(format_args!(
            "chaos force close confirmed both sides tx={tx_hash}"
        ));
        Ok(tx_hash)
    }

    pub(super) fn watchtower_settlement(&mut self) -> Result<()> {
        self.start_watchtowers([0, 1])?;
        let channel = self.open_channel()?;
        self.settle_held_payment(channel, [0, 1], Hash256::from([17; 32]))
    }

    pub(super) fn watchtower_superseded(&mut self) -> Result<()> {
        self.start_watchtowers([0, 1])?;
        let channel = self.open_channel()?;
        let from = self
            .runtime
            .channel(0, &channel)
            .context("missing opener channel state")?;
        let funding = from
            .get_funding_transaction_outpoint()
            .context("funding output")?;

        let add_invoice = |driver: &mut Self,
                           payee: usize,
                           preimage: Hash256,
                           known_preimage: bool,
                           amount: u128|
         -> Result<(Hash256, String)> {
            let payment: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            let payee_key = fnn::fiber::Privkey::from([(payee + 1) as u8; 32]);
            let invoice = InvoiceBuilder::new(Currency::Fibd)
                .amount(Some(amount))
                .payment_hash(payment)
                .payee_pub_key(driver.runtime.peers[payee].into())
                .build_with_sign(|message| {
                    secp256k1::SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0)
                })?;
            let invoice_string = invoice.to_string();
            let mut reply = driver.runtime.request(payee, |reply| {
                FiberActorCommand::AddInvoice(invoice, known_preimage.then_some(preimage), reply)
            })?;
            driver.until("invoice recorded", |_| {
                receive(&mut reply)?.transpose().map_err(Into::into)
            })?;
            Ok((payment, invoice_string))
        };

        let send_direct = |driver: &mut Self,
                           payer: usize,
                           payee: usize,
                           invoice_string: String,
                           amount: u128,
                           payment: Hash256|
         -> Result<()> {
            let reply = driver.runtime.request(payer, |reply| {
                FiberActorCommand::SendPaymentWithRouter(
                    SendPaymentWithRouterCommand {
                        invoice: Some(invoice_string),
                        router: vec![RouterHop {
                            target: driver.runtime.peers[payee],
                            channel_outpoint: funding.clone(),
                            amount_received: amount,
                            incoming_tlc_expiry: DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                        }],
                        ..Default::default()
                    },
                    reply,
                )
            })?;
            ensure!(driver.reply("submit superseded probe", reply)?.payment_hash == payment);
            Ok(())
        };

        let pre0 = Hash256::from([20; 32]);
        let pre1 = Hash256::from([21; 32]);
        let pre2 = Hash256::from([22; 32]);
        let amount0 = 400 * 100_000_000;
        let amount1 = 1_000 * 100_000_000;
        let amount2 = 500 * 100_000_000;
        let (p0, inv0) = add_invoice(self, 0, pre0, false, amount0)?;
        let (p1, inv1) = add_invoice(self, 1, pre1, false, amount1)?;
        let (p2, inv2) = add_invoice(self, 0, pre2, false, amount2)?;

        // First commit hold invoice p0 (B->A) cleanly so A can reveal pre0 after force close.
        send_direct(self, 1, 0, inv0, amount0, p0)?;
        self.until("baseline hold invoice p0 committed on both sides", |s| {
            let received = s.runtime.nodes.borrow()[0]
                .node
                .as_ref()
                .and_then(|node| node.store.get_invoice_status(&p0))
                == Some(CkbInvoiceStatus::Received);
            let [a, b] = [0, 1].map(|n| s.runtime.channel(n, &channel));
            Ok((received
                && a.zip(b)
                    .is_some_and(|(a, b)| !a.tlc_state.waiting_ack && !b.tlc_state.waiting_ack))
            .then_some(()))
        })?;

        // Delay A's CommitmentSignedV2 for the next round (frame after AddTlc) so the link stays
        // up while A is waiting_ack on variant 1 ([p0, p1]) and B concurrently commits p2 (B->A).
        let delay_cs = Fault::DelayPeerFrame {
            from: 0,
            to: 1,
            rule: FrameDelay {
                after: 1,
                duration: Duration::from_secs(5),
            },
        }
        .inject(&mut self.runtime)?;
        send_direct(self, 0, 1, inv1, amount1, p1)?;
        self.until(
            "A signs variant 1 while CommitmentSignedV2 is delayed in transit",
            |s| {
                let a_waiting = s.runtime.channel(0, &channel).is_some_and(|state| {
                    state.tlc_state.waiting_ack
                        && state.tlc_state.all_tlcs().any(|tlc| tlc.payment_hash == p1)
                });
                Ok((delay_cs.hit(&s.runtime) && a_waiting).then_some(()))
            },
        )?;

        // Send p2 in the reverse direction (B->A) while A's variant-1 CS is still delayed.
        // Once A accepts B's CommitmentSignedV2 for p2, p2 enters A's remote commitment
        // state, changing A's pending round settlement for B from [p0, p1] to [p0, p1, p2].
        send_direct(self, 1, 0, inv2, amount2, p2)?;
        self.until(
            "A commits incoming p2 while variant-1 CS is still delayed",
            |s| {
                Ok(s.runtime
                    .channel(0, &channel)
                    .is_some_and(|state| {
                        state.tlc_state.all_tlcs().any(|tlc| tlc.payment_hash == p2)
                    })
                    .then_some(()))
            },
        )?;
        delay_cs.heal(&mut self.runtime);

        let drop_ack = Fault::DropPeerFrame {
            from: 1,
            to: 0,
            rule: FrameLoss::RevokeAndAck(channel),
        }
        .inject(&mut self.runtime)?;
        self.runtime.reset_link(0, 1);

        self.until(
            "A resigns the round with variant 2 and B's RevokeAndAckV2 is dropped",
            |s| {
                let b_committed = s.runtime.channel(1, &channel).is_some_and(|state| {
                    state.tlc_state.all_tlcs().any(|tlc| tlc.payment_hash == p1)
                });
                Ok((drop_ack.hit(&s.runtime) && b_committed).then_some(()))
            },
        )?;
        self.runtime.partition(0, 1, false);
        drop_ack.heal(&mut self.runtime);

        let tower_a = self.tower(0)?.clone();
        let superseded_count = self.until(
            "A watchtower records superseded settlement variant for the round",
            |_| {
                let store = tower_a.store.borrow();
                let count = store
                    .as_ref()
                    .map(|store| {
                        store
                            .get_superseded_remote_settlements(&NodeId::local(), &channel)
                            .len()
                    })
                    .unwrap_or(0);
                Ok((count > 0).then_some(count))
            },
        )?;
        let a_state = self
            .runtime
            .channel(0, &channel)
            .context("missing A channel state")?;
        ensure!(
            a_state.tlc_state.waiting_ack && !a_state.session.superseded_settlements.is_empty(),
            "A did not retain superseded settlement while awaiting ACK"
        );
        self.runtime.note(format_args!(
            "watchtower recorded superseded settlement count={superseded_count} channel={channel}"
        ));

        // B force-closes with variant 2 ([p0, p1, p2]) while A still holds both
        // variant 1 (superseded) and variant 2 (pending) for the same round.
        let commitment = self.confirm_force_close(channel, 1, 0)?;
        ensure!(self.runtime.chain.confirmed_spend(&funding) == Some(commitment));

        // Settle p0 on-chain by revealing pre0 on A (whose watchtower holds the superseded
        // variants), and let p1 and p2 time out on-chain after advancing the virtual clock.
        self.reveal_preimage(0, p0, pre0)?;
        self.until(
            "watchtower settles fulfilled TLC p0 on superseded round",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([0, 1])?;
                }
                let fulfilled = s.runtime.nodes.borrow()[0]
                    .node
                    .as_ref()
                    .and_then(|node| node.store.get_invoice_status(&p0))
                    == Some(CkbInvoiceStatus::Paid)
                    && s.runtime.payment_status(1, p0) == Some(PaymentStatus::Success);
                Ok(fulfilled.then_some(()))
            },
        )?;

        let max_expiry = self
            .runtime
            .channel(0, &channel)
            .context("missing A closed channel")?
            .tlc_state
            .all_tlcs()
            .filter(|tlc| tlc.payment_hash == p1 || tlc.payment_hash == p2)
            .map(|tlc| tlc.expiry)
            .max()
            .context("missing p1/p2 TLCs on A")?;
        self.advance_clock_past(max_expiry);

        let spend = self.finish_onchain_close([0, 1], channel, commitment)?;
        self.until(
            "both endpoints reconcile timed-out TLCs on superseded round",
            |s| {
                Ok(
                    (s.runtime.payment_status(0, p1) == Some(PaymentStatus::Failed)
                        && s.runtime.payment_status(1, p2) == Some(PaymentStatus::Failed))
                    .then_some(()),
                )
            },
        )?;
        ensure!(
            self.runtime.nodes.borrow()[1]
                .node
                .as_ref()
                .and_then(|node| node.store.get_invoice_status(&p1))
                != Some(CkbInvoiceStatus::Paid)
                && self.runtime.nodes.borrow()[0]
                    .node
                    .as_ref()
                    .and_then(|node| node.store.get_invoice_status(&p2))
                    != Some(CkbInvoiceStatus::Paid),
            "timed-out invoice was marked Paid"
        );
        for node in [0, 1] {
            let store = self.runtime.store(node)?;
            fnn::simulation::verify_payment_hash_index(&store)
                .map_err(|error| anyhow::anyhow!("{} payment-hash index: {error}", NAMES[node]))?;
        }
        self.runtime.note(format_args!(
            "watchtower settled superseded-round commitment={commitment} spend={spend}"
        ));
        Ok(())
    }

    pub(super) fn settle_held_payment(
        &mut self,
        channel: Hash256,
        endpoints: [usize; 2],
        preimage: Hash256,
    ) -> Result<()> {
        let [payer, payee] = endpoints;
        let [from, to] = endpoints.map(|node| {
            self.runtime
                .channel(node, &channel)
                .context("missing channel state")
        });
        let from = from?;
        let to = to?;
        let funding = from
            .get_funding_transaction_outpoint()
            .context("funding output")?;
        let amount = (1_000 * 100_000_000).min(from.get_local_balance());
        ensure!(amount > 0, "no available balance for the on-chain probe");
        let expected = [
            from.get_local_balance() + u128::from(from.local_reserved_ckb_amount) - amount,
            to.get_local_balance() + u128::from(to.local_reserved_ckb_amount) + amount,
        ];
        let wallet_capacity = |node: usize, driver: &Driver| {
            driver
                .runtime
                .chain
                .watchtower_wallet_capacity(fnn::fiber::Privkey::from([(node + 1) as u8; 32]))
        };
        let initial = endpoints.map(|node| wallet_capacity(node, self));
        let payment = HashAlgorithm::CkbHash.hash(preimage).into();
        let payee_key = fnn::fiber::Privkey::from([(payee + 1) as u8; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_hash(payment)
            .payee_pub_key(self.runtime.peers[payee].into())
            .build_with_sign(|message| {
                secp256k1::SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0)
            })?;
        let invoice_string = invoice.to_string();
        let mut reply = self.runtime.request(payee, |reply| {
            FiberActorCommand::AddInvoice(invoice, None, reply)
        })?;
        self.until("hold invoice recorded", |_| {
            receive(&mut reply)?.transpose().map_err(Into::into)
        })?;
        let reply = self.runtime.request(payer, |reply| {
            FiberActorCommand::SendPaymentWithRouter(
                SendPaymentWithRouterCommand {
                    invoice: Some(invoice_string),
                    router: vec![RouterHop {
                        target: self.runtime.peers[payee],
                        channel_outpoint: funding.clone(),
                        amount_received: amount,
                        incoming_tlc_expiry: DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                    }],
                    ..Default::default()
                },
                reply,
            )
        })?;
        ensure!(self.reply("submit on-chain probe", reply)?.payment_hash == payment);

        let tlc_id = self.until("both nodes committed the held payment", |s| {
            let received = s.runtime.nodes.borrow()[payee]
                .node
                .as_ref()
                .and_then(|node| node.store.get_invoice_status(&payment))
                == Some(CkbInvoiceStatus::Received);
            let [from, to] = endpoints.map(|node| s.runtime.channel(node, &channel));
            let Some((from, to)) = from.zip(to) else {
                return Ok(None);
            };
            if !received || from.tlc_state.waiting_ack || to.tlc_state.waiting_ack {
                return Ok(None);
            }
            let local = from
                .tlc_state
                .all_tlcs()
                .any(|tlc| tlc.payment_hash == payment);
            Ok(local
                .then(|| {
                    to.tlc_state
                        .all_tlcs()
                        .find(|tlc| tlc.payment_hash == payment)
                        .map(|tlc| tlc.tlc_id)
                })
                .flatten())
        })?;
        self.runtime.note(format_args!(
            "in-flight TLC committed {payment} amount={amount}"
        ));

        let commitment = self.confirm_force_close(channel, payer, payee)?;
        ensure!(self.runtime.chain.confirmed_spend(&funding) == Some(commitment));
        let tower = self.tower(payee)?.clone();
        let tower_store = tower
            .store
            .borrow()
            .clone()
            .context("watchtower store unavailable")?;
        let external = tower.mode == TowerMode::External;
        let failures = tower.preimage_failures();
        if external {
            self.until(
                "external watchtowers persist the confirmed commitment snapshot",
                |s| {
                    for node in endpoints {
                        let tower = s.tower(node)?;
                        let store = tower.store.borrow();
                        let store = store.as_ref().context("watchtower store unavailable")?;
                        if !s
                            .runtime
                            .chain
                            .watchtower_matches_commitment(store, &channel, commitment)
                        {
                            return Ok(None);
                        }
                    }
                    Ok(Some(()))
                },
            )?;
            self.runtime.set_tower_partition(payee, true)?;
        }
        self.reveal_preimage(payee, payment, preimage)?;

        if external {
            self.until("external watchtower RPC fails during the partition", |_| {
                ensure!(
                    tower_store
                        .get_watch_preimage(&NodeId::local(), &payment)
                        .is_none(),
                    "external watchtower received the preimage through the partition"
                );
                Ok((tower.preimage_failures() > failures).then_some(()))
            })?;
            self.runtime.note(format_args!(
                "{} external watchtower RPC create_preimage failed during partition",
                NAMES[payee]
            ));
            self.runtime.set_tower_partition(payee, false)?;
            // Production forwarding logs a failed RPC and proceeds to the next event.
            self.reveal_preimage(payee, payment, preimage)?;
            self.until(
                "external watchtower persists the preimage after network recovery",
                |_| {
                    Ok((tower_store.get_watch_preimage(&NodeId::local(), &payment)
                        == Some(preimage))
                    .then_some(()))
                },
            )?;
            self.runtime.note(format_args!(
                "{} external watchtower RPC recovered with preimage",
                tower.name()
            ));
        }

        self.exercise_watchtower_faults(endpoints, payee, commitment)?;
        if external {
            self.runtime.crash(payee);
            let receipt = self.until(
                "external watchtower settles while the protected node is offline",
                |s| {
                    ensure!(
                        s.runtime.nodes.borrow()[payee].node.is_none(),
                        "protected node restarted before settlement"
                    );
                    if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                        for node in endpoints {
                            s.watchtower_check(node)?;
                        }
                    }
                    Ok(tower_store
                        .get_onchain_tlc_settlement(&NodeId::local(), &channel, tlc_id, &payment)
                        .filter(|_| s.runtime.chain.commitment_settled(commitment)))
                },
            )?;
            ensure!(
                receipt.preimage == Some(preimage),
                "external watchtower recorded the wrong preimage"
            );
            self.runtime.note(format_args!(
                "{} external watchtower settled while {} offline commitment={commitment} receipt={}",
                tower.name(), NAMES[payee], receipt.tx_hash,
            ));
            self.runtime.restart(payee);
            self.until("protected node and local chain observer restarted", |s| {
                Ok(s.runtime.nodes.borrow()[payee]
                    .settlement_observer
                    .is_some()
                    .then_some(()))
            })?;
        }

        let spend = self.finish_onchain_close(endpoints, channel, commitment)?;
        self.until("Fiber reconciles the on-chain payment", |s| {
            let settled = s.runtime.payment_status(payer, payment) == Some(PaymentStatus::Success)
                && s.runtime.nodes.borrow()[payee]
                    .node
                    .as_ref()
                    .and_then(|node| node.store.get_invoice_status(&payment))
                    == Some(CkbInvoiceStatus::Paid);
            Ok(settled.then_some(()))
        })?;
        let receipt = self.runtime.nodes.borrow()[payee]
            .node
            .as_ref()
            .context("recipient offline")?
            .store
            .get_onchain_tlc_settlement(&NodeId::local(), &channel, tlc_id, &payment)
            .context("watchtower did not record a settlement witness for the received TLC")?;
        ensure!(
            receipt.preimage == Some(preimage),
            "watchtower recorded the wrong preimage"
        );
        let payout =
            [0, 1].map(|side| wallet_capacity(endpoints[side], self).checked_sub(initial[side]));
        let [Some(payer_payout), Some(payee_payout)] = payout else {
            anyhow::bail!("settlement did not credit both wallets: {payout:?}");
        };
        // The on-chain payout includes reserves and pays CKB fees; off-chain snapshots stay frozen.
        for (actual, expected) in [payer_payout, payee_payout].into_iter().zip(expected) {
            ensure!(
                actual <= expected && expected - actual < 100_000_000,
                "unexpected on-chain payout {actual} for {expected} before chain fees"
            );
        }
        self.runtime.note(format_args!("watchtower settled in-flight TLC {payment} commitment={commitment} spend={spend} receipt={} payouts=[{payer_payout}, {payee_payout}]", receipt.tx_hash));
        Ok(())
    }

    fn reveal_preimage(&mut self, payee: usize, payment: Hash256, preimage: Hash256) -> Result<()> {
        let mut reply = self.runtime.request(payee, |reply| {
            FiberActorCommand::SettleInvoice(payment, preimage, reply)
        })?;
        self.until("preimage recorded after force close", |_| {
            receive(&mut reply)?.transpose().map_err(Into::into)
        })
    }

    pub(super) fn start_watchtowers(&mut self, endpoints: [usize; 2]) -> Result<()> {
        self.runtime.note(format_args!(
            "watchtower mode {:?}",
            self.tower(endpoints[0])?.mode
        ));
        for node in endpoints {
            self.tower(node)?.enabled.send_replace(true);
        }
        self.until("both watchtowers running", |s| {
            let [a, b] = endpoints;
            Ok(
                (s.tower(a)?.actor.borrow().is_some() && s.tower(b)?.actor.borrow().is_some())
                    .then_some(()),
            )
        })
    }

    pub(super) fn exercise_watchtower_faults(
        &mut self,
        endpoints: [usize; 2],
        node: usize,
        commitment: Hash256,
    ) -> Result<()> {
        let tower = self.tower(node)?.clone();
        let access = &tower.access;
        let endpoint = ChainEndpoint::Watchtower;
        let first_hit = self.runtime.tower_hits[node].len();
        access.set_for(endpoint, "get_transactions", ChainFault::Offline);
        let actor = tower.actor.borrow().clone().context("watchtower offline")?;
        let mailbox = slow_mailbox(&actor.get_cell(), Duration::from_millis(20));
        self.watchtower_check(node)?;
        self.until("watchtower scan waits in the mailbox", |s| {
            ensure!(
                s.runtime.tower_hits[node].len() == first_hit,
                "scan reached the indexer while the mailbox was delayed"
            );
            Ok((mailbox.hits() == 1).then_some(()))
        })?;
        self.runtime
            .note(format_args!("{} watchtower mailbox stalled", tower.name()));
        drop(mailbox);

        self.until("watchtower blocked on the indexer", |s| {
            Ok(s.runtime.tower_hits[node][first_hit..]
                .iter()
                .any(|hit| hit.endpoint == endpoint && hit.fault == ChainFault::Offline)
                .then_some(()))
        })?;

        tower.enabled.send_replace(false);
        self.until("only the watchtower stops", |s| {
            ensure!(
                s.runtime.link(endpoints[0], endpoints[1]),
                "watchtower crash affected the Fiber link"
            );
            Ok(tower.actor.borrow().is_none().then_some(()))
        })?;
        access.set_for(
            endpoint,
            "send_transaction",
            ChainFault::Timeout(Duration::from_millis(15)),
        );
        tower.enabled.send_replace(true);
        self.until("watchtower restarts from persisted state", |_| {
            Ok(tower.actor.borrow().is_some().then_some(()))
        })?;

        self.until("watchtower submission times out without spending", |s| {
            ensure!(
                s.runtime.chain.commitment_spend(commitment).is_none(),
                "timed out submission spent the commitment"
            );
            if s.runtime.tower_hits[node][first_hit..]
                .iter()
                .any(|hit| hit.endpoint == endpoint && hit.request == "send_transaction")
            {
                return Ok(Some(()));
            }
            if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                s.watchtower_check(node)?;
            }
            Ok(None)
        })?;
        access.set(ChainFault::Healthy);

        Ok(())
    }

    pub(super) fn finish_onchain_close(
        &mut self,
        endpoints: [usize; 2],
        channel: Hash256,
        commitment: Hash256,
    ) -> Result<Hash256> {
        let spend = self.until("watchtowers settle the commitment on both endpoints", |s| {
            if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                s.scan_onchain(endpoints)?;
            }
            let done = endpoints.into_iter().all(|node| {
                s.runtime.channel(node, &channel).is_some_and(|state| {
                    matches!(state.state, ChannelState::Closed(_))
                        && !state.is_waiting_onchain_settlement()
                })
            });
            Ok(done
                .then(|| s.runtime.chain.commitment_spend(commitment))
                .flatten())
        })?;
        self.runtime.note(format_args!("watchtowers completed settlement channel={channel} commitment={commitment} spend={spend}"));
        Ok(spend)
    }

    pub(super) fn scan_onchain(&self, nodes: impl IntoIterator<Item = usize>) -> Result<()> {
        for node in nodes {
            let tower = self.tower(node)?;
            if let Some(actor) = tower.actor.borrow().as_ref() {
                crate::host::send(actor, WatchtowerMessage::PeriodicCheck)?;
            }
            if self.runtime.node_available(node) {
                let slots = self.runtime.nodes.borrow();
                let slot = &slots[node];
                if let Some(observer) = &slot.settlement_observer {
                    crate::host::send(observer, WatchtowerMessage::PeriodicCheck)?;
                }
                let actor = &slot.node.as_ref().expect("available node").actor;
                crate::host::send(
                    actor,
                    NetworkActorMessage::new_command(FiberActorCommand::CheckChannelsShutdown),
                )?;
            }
        }
        Ok(())
    }

    fn tower(&self, node: usize) -> Result<&Tower> {
        Ok(&self
            .runtime
            .watchtowers
            .as_ref()
            .context("watchtower controls")?[node])
    }

    fn watchtower_check(&self, node: usize) -> Result<()> {
        let tower = self.tower(node)?;
        let actor = tower.actor.borrow();
        crate::host::send(
            actor.as_ref().context("watchtower offline")?,
            WatchtowerMessage::PeriodicCheck,
        )
    }
}
