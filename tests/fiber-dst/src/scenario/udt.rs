use anyhow::{ensure, Context, Result};
use ckb_types::packed::Script;
use fnn::fiber::network::{AcceptChannelCommand, OpenChannelCommand};
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{
    ChannelConnectivityState, ChannelState, FiberActorCommand, Hash256, PaymentStatus, Privkey,
};
use fnn::fiber_types::HashAlgorithm;
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use fnn::simulation;
use rand::{rngs::StdRng, Rng, SeedableRng};
use secp256k1::SECP256K1;

use crate::host::NAMES;

use super::faults::{Fault, ProtocolPoint};
use super::{receive, Driver};

const UDT_FUNDING_AMOUNT: u128 = 100_000;
const UDT_RELAY_FEE_PPM: u128 = 1_000;

pub(super) struct UdtChannel {
    pub(super) sender: usize,
    pub(super) receiver: usize,
    pub(super) channel: Hash256,
    pub(super) before: [u128; 2],
}

pub(super) struct UdtSummary {
    pub(super) channels: usize,
    pub(super) succeeded: usize,
    pub(super) cancelled: usize,
}

impl Driver {
    /// Standalone deterministic UDT multi-hop fixture on a 3-node chain `A - B - C`.
    pub(super) fn udt_multihop(&mut self, seed: u64) -> Result<()> {
        let summary = self.run_udt_multihop_workload(seed, [0, 1, 2], true)?;
        ensure!(
            summary.channels == 2 && summary.succeeded >= 2 && summary.cancelled == 1,
            "unexpected UDT multihop summary: channels={} succeeded={} cancelled={}",
            summary.channels,
            summary.succeeded,
            summary.cancelled
        );
        Ok(())
    }

    /// Open a 2-hop public UDT channel chain `a - relay - dst` using `Script::default()` (the
    /// mock UDT type script registered in `MockContext`), execute bidirectional multi-hop UDT
    /// payments with a relay crash at `ForwardFulfill` and a hold-invoice cancellation, and
    /// verify exact UDT balance conservation and payment-hash index integrity.
    pub(super) fn run_udt_multihop_workload(
        &mut self,
        seed: u64,
        nodes: [usize; 3],
        inject_faults: bool,
    ) -> Result<UdtSummary> {
        let [a, relay, dst] = nodes;
        let udt_script = Script::default();
        let ch_ab = self.open_udt_channel(a, relay, UDT_FUNDING_AMOUNT, 0, udt_script.clone())?;
        let ch_bd = self.open_udt_channel(
            relay,
            dst,
            UDT_FUNDING_AMOUNT,
            UDT_RELAY_FEE_PPM,
            udt_script.clone(),
        )?;
        self.step()?;
        self.coverage.add("udt_channels", 2);

        let mut rng = StdRng::seed_from_u64(seed ^ 0x7564_745f_6473_7401);
        let mut net_ab: i128 = 0;
        let mut net_bd: i128 = 0;
        let mut succeeded = 0usize;
        let mut cancelled = 0usize;

        // 1. Forward multi-hop UDT payment A -> relay -> dst, crashing `relay` at ForwardFulfill!
        let fwd_amount: u128 = 10_000;
        let fwd_pre = Hash256::from(rng.random::<[u8; 32]>());
        let fwd_inv = self.add_udt_invoice(dst, fwd_amount, fwd_pre, true, udt_script.clone())?;
        let fwd_hash: Hash256 = HashAlgorithm::CkbHash.hash(fwd_pre).into();

        let cut = if inject_faults {
            Some(
                Fault::ProtocolPause {
                    node: relay,
                    channel: ch_bd.channel,
                    point: ProtocolPoint::ForwardFulfill,
                    preceding_hits: 0,
                }
                .inject(&mut self.runtime)?,
            )
        } else {
            None
        };

        let sent = self.pay(
            a,
            SendPaymentCommand {
                invoice: Some(fwd_inv),
                max_fee_rate: Some(20_000),
                ..Default::default()
            },
        )?;
        ensure!(sent == fwd_hash);

        if let Some(cut) = cut {
            self.until("UDT relay reaches ForwardFulfill pause", |s| {
                Ok(cut.hit(&s.runtime).then_some(()))
            })?;
            ensure!(
                self.runtime.payment_status(a, fwd_hash) != Some(PaymentStatus::Success),
                "UDT payer observed success before relay crash"
            );
            let crash = Fault::Crash(relay).inject(&mut self.runtime)?;
            cut.heal(&mut self.runtime);
            for _ in 0..5 {
                self.step()?;
            }
            crash.heal(&mut self.runtime);
        }

        self.until("forward UDT multi-hop payment succeeds", |s| {
            let paid = s.runtime.nodes.borrow()[dst]
                .node
                .as_ref()
                .and_then(|n| n.store.get_invoice_status(&fwd_hash))
                == Some(CkbInvoiceStatus::Paid);
            Ok(
                (paid && s.runtime.payment_status(a, fwd_hash) == Some(PaymentStatus::Success))
                    .then_some(()),
            )
        })?;
        self.wait_udt_channels_drained(&[&ch_ab, &ch_bd])?;
        self.wait_payment_actor(a)?;
        let fwd_fee = self.udt_channel_delta(&ch_ab, net_ab)?;
        ensure!(
            fwd_fee >= fwd_amount as i128,
            "UDT forward payment did not debit A->relay: delta={fwd_fee}"
        );
        net_ab += fwd_fee;
        net_bd += fwd_amount as i128;
        succeeded += 1;
        self.coverage.hit("udt_payments");

        // 2. Reverse multi-hop UDT payment dst -> relay -> A (validates reverse routing & UDT fees).
        let rev_amount: u128 = 6_000;
        let rev_pre = Hash256::from(rng.random::<[u8; 32]>());
        let rev_inv = self.add_udt_invoice(a, rev_amount, rev_pre, true, udt_script.clone())?;
        let rev_hash: Hash256 = HashAlgorithm::CkbHash.hash(rev_pre).into();
        let sent_rev = self.pay(
            dst,
            SendPaymentCommand {
                invoice: Some(rev_inv),
                max_fee_rate: Some(20_000),
                ..Default::default()
            },
        )?;
        ensure!(sent_rev == rev_hash);
        self.until("reverse UDT multi-hop payment succeeds", |s| {
            let paid = s.runtime.nodes.borrow()[a]
                .node
                .as_ref()
                .and_then(|n| n.store.get_invoice_status(&rev_hash))
                == Some(CkbInvoiceStatus::Paid);
            Ok(
                (paid && s.runtime.payment_status(dst, rev_hash) == Some(PaymentStatus::Success))
                    .then_some(()),
            )
        })?;
        self.wait_udt_channels_drained(&[&ch_ab, &ch_bd])?;
        self.wait_payment_actor(dst)?;
        net_ab -= rev_amount as i128;
        let rev_debit_bd = self.udt_channel_delta(&ch_bd, net_bd)?;
        ensure!(
            rev_debit_bd <= -(rev_amount as i128),
            "UDT reverse payment did not debit dst->relay: delta={rev_debit_bd}"
        );
        net_bd += rev_debit_bd;
        succeeded += 1;
        self.coverage.hit("udt_payments");

        // 3. Multi-hop UDT hold invoice cancelled by receiver -> must refund exact UDT balances!
        let cancel_amount: u128 = 4_000;
        let cancel_pre = Hash256::from(rng.random::<[u8; 32]>());
        let cancel_inv = self.add_udt_invoice(dst, cancel_amount, cancel_pre, false, udt_script)?;
        let cancel_hash: Hash256 = HashAlgorithm::CkbHash.hash(cancel_pre).into();
        let sent_cancel = self.pay(
            a,
            SendPaymentCommand {
                invoice: Some(cancel_inv),
                max_fee_rate: Some(20_000),
                ..Default::default()
            },
        )?;
        ensure!(sent_cancel == cancel_hash);
        self.until("UDT hold invoice reaches Received on dst", |s| {
            Ok((s.runtime.store(dst)?.get_invoice_status(&cancel_hash)
                == Some(CkbInvoiceStatus::Received))
            .then_some(()))
        })?;
        let mut cancel_reply = self.runtime.request(dst, |reply| {
            FiberActorCommand::CancelInvoice(cancel_hash, reply)
        })?;
        self.until("dst cancels UDT hold invoice", |_| {
            receive(&mut cancel_reply)?.transpose().map_err(Into::into)
        })?;
        self.until("cancelled UDT multi-hop payment fails cleanly", |s| {
            Ok(
                (s.runtime.payment_status(a, cancel_hash) == Some(PaymentStatus::Failed))
                    .then_some(()),
            )
        })?;
        self.wait_udt_channels_drained(&[&ch_ab, &ch_bd])?;
        self.wait_payment_actor(a)?;
        cancelled += 1;
        self.coverage.hit("udt_payments");

        // Strict UDT balance conservation & payment-hash index audit.
        for (ch, expected_net) in [(&ch_ab, net_ab), (&ch_bd, net_bd)] {
            let s_a = self
                .runtime
                .channel(ch.sender, &ch.channel)
                .context("missing UDT sender state")?;
            let s_b = self
                .runtime
                .channel(ch.receiver, &ch.channel)
                .context("missing UDT receiver state")?;
            let expected_sender = (ch.before[0] as i128 - expected_net) as u128;
            let expected_receiver = (ch.before[1] as i128 + expected_net) as u128;
            ensure!(
                s_a.get_local_balance() == expected_sender
                    && s_a.get_remote_balance() == expected_receiver
                    && s_b.get_local_balance() == expected_receiver
                    && s_b.get_remote_balance() == expected_sender,
                "UDT balance mismatch on {}: a=[{}, {}] b=[{}, {}] expected=[{expected_sender}, {expected_receiver}]",
                ch.channel,
                s_a.get_local_balance(),
                s_a.get_remote_balance(),
                s_b.get_remote_balance(),
                s_b.get_local_balance(),
            );
            ensure!(
                !s_a.any_tlc_pending() && !s_b.any_tlc_pending(),
                "UDT channel {} has pending TLCs",
                ch.channel
            );
        }
        for node in nodes {
            let store = self.runtime.store(node)?;
            simulation::verify_payment_hash_index(&store)
                .map_err(|err| anyhow::anyhow!("{} UDT payment-hash index: {err}", NAMES[node]))?;
        }
        self.runtime.note(format_args!(
            "UDT multihop reconciled {}-{}-{} channels=2 succeeded={succeeded} cancelled={cancelled}",
            NAMES[a], NAMES[relay], NAMES[dst]
        ));
        Ok(UdtSummary {
            channels: 2,
            succeeded,
            cancelled,
        })
    }

    fn open_udt_channel(
        &mut self,
        a: usize,
        b: usize,
        amount: u128,
        fee: u128,
        udt_script: Script,
    ) -> Result<UdtChannel> {
        self.until("UDT peers connected", |s| {
            Ok(s.runtime.link(a, b).then_some(()))
        })?;
        let payee = self.runtime.peers[b];
        let reply = self.runtime.request(a, |reply| {
            FiberActorCommand::OpenChannel(
                OpenChannelCommand {
                    pubkey: payee,
                    funding_amount: amount,
                    public: true,
                    one_way: false,
                    shutdown_script: None,
                    funding_udt_type_script: Some(udt_script),
                    commitment_fee_rate: None,
                    commitment_delay_epoch: None,
                    funding_fee_rate: None,
                    tlc_expiry_delta: None,
                    tlc_min_value: None,
                    tlc_fee_proportional_millionths: Some(fee),
                    max_tlc_value_in_flight: None,
                    max_tlc_number_in_flight: None,
                },
                reply,
            )
        })?;
        let temporary = self.reply("open UDT channel", reply)?.channel_id;
        self.until("UDT acceptance request", |s| {
            Ok(s.runtime.nodes.borrow()[b]
                .pending_channels
                .contains(&temporary)
                .then_some(()))
        })?;
        let accept = self.runtime.request(b, |reply| {
            FiberActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: temporary,
                    funding_amount: amount,
                    shutdown_script: None,
                    max_tlc_value_in_flight: None,
                    max_tlc_number_in_flight: None,
                    min_tlc_value: None,
                    tlc_fee_proportional_millionths: Some(fee),
                    tlc_expiry_delta: None,
                },
                reply,
            )
        })?;
        let channel = self.reply("accept UDT channel", accept)?.new_channel_id;
        self.wait_online(a, b, &channel)?;
        let st_a = self
            .runtime
            .channel(a, &channel)
            .context("missing UDT opener state")?;
        let st_b = self
            .runtime
            .channel(b, &channel)
            .context("missing UDT acceptor state")?;
        ensure!(
            st_a.funding_udt_type_script.is_some()
                && st_b.funding_udt_type_script.is_some()
                && st_a.get_local_balance() == amount
                && st_b.get_local_balance() == amount,
            "UDT channel {channel} initial balances mismatch"
        );
        self.runtime.note(format_args!(
            "opened UDT V2 channel {}-{} {channel} amount={amount}",
            NAMES[a], NAMES[b]
        ));
        Ok(UdtChannel {
            sender: a,
            receiver: b,
            channel,
            before: [st_a.get_local_balance(), st_b.get_local_balance()],
        })
    }

    fn add_udt_invoice(
        &mut self,
        payee: usize,
        amount: u128,
        preimage: Hash256,
        known_preimage: bool,
        udt_script: Script,
    ) -> Result<String> {
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
        let key = Privkey::from([(payee + 1) as u8; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_hash(payment_hash)
            .udt_type_script(udt_script)
            .payee_pub_key(self.runtime.peers[payee].into())
            .build_with_sign(|msg| SECP256K1.sign_ecdsa_recoverable(msg, &key.0))?;
        let invoice_str = invoice.to_string();
        let mut reply = self.runtime.request(payee, |reply| {
            FiberActorCommand::AddInvoice(invoice, known_preimage.then_some(preimage), reply)
        })?;
        self.until("UDT invoice recorded", |_| {
            receive(&mut reply)?.transpose().map_err(Into::into)
        })?;
        Ok(invoice_str)
    }

    fn wait_udt_channels_drained(&mut self, channels: &[&UdtChannel]) -> Result<()> {
        let res = self.until("UDT channels drained and online", |s| {
            let drained = channels.iter().all(|ch| {
                s.runtime.link(ch.sender, ch.receiver)
                    && [ch.sender, ch.receiver].into_iter().all(|node| {
                        s.runtime.channel(node, &ch.channel).is_some_and(|st| {
                            st.state == ChannelState::ChannelReady
                                && st.connectivity_state == ChannelConnectivityState::Online
                                && !st.tlc_state.waiting_ack
                                && !st.any_tlc_pending()
                        })
                    })
            });
            Ok(drained.then_some(()))
        });
        res
    }

    fn udt_channel_delta(&self, ch: &UdtChannel, prior_net: i128) -> Result<i128> {
        let st = self
            .runtime
            .channel(ch.sender, &ch.channel)
            .context("missing UDT channel state")?;
        let total_net = ch.before[0] as i128 - st.get_local_balance() as i128;
        Ok(total_net - prior_net)
    }
}
