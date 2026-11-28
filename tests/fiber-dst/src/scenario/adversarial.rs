use anyhow::{ensure, Context, Result};
use fiber_lsp_sdk::SigningPolicy;
use fnn::fiber::graph::NetworkGraphStateStore;
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{FiberActorCommand, Hash256, PaymentStatus, Privkey};
use fnn::fiber_types::HashAlgorithm;
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use secp256k1::SECP256K1;

use crate::lsp::{SdkSignerFault, WalletCommand};

use super::faults::{Fault, ProtocolPoint};
use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;

impl Driver {
    /// Race two full-value payers on distinct channels against a single-path immediate invoice.
    /// Pauses the payee after marking the invoice `Received` (and before sending `RemoveTlc(Fulfill)`),
    /// verifying that exactly one payer succeeds and the payee never double-settles.
    pub(super) fn single_path_double_pay(&mut self) -> Result<()> {
        let edge_ac = self.public_edge(0, 2, FUNDING_AMOUNT, 0)?;
        let edge_bc = self.public_edge(1, 2, FUNDING_AMOUNT, 0)?;
        self.step()?;

        let amount = 500 * 100_000_000;
        let preimage = Hash256::from([0x71; 32]);
        let payee_key = Privkey::from([3; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_preimage(preimage)
            .payment_secret(Hash256::from([0x72; 32]))
            .payee_pub_key(self.runtime.peers[2].into())
            .allow_mpp(false)
            .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0))?;
        let payment_hash = *invoice.payment_hash();
        let invoice_str = invoice.to_string();
        let mut add_reply = self.runtime.request(2, |reply| {
            FiberActorCommand::AddInvoice(invoice, Some(preimage), reply)
        })?;
        self.until("single-path invoice added on C", |_| {
            receive(&mut add_reply)?.transpose().map_err(Into::into)
        })?;

        // Pause C right after marking the immediate single-path invoice `Received` and before
        // dispatching `RemoveTlc(Fulfill)` on the winning channel so the competing TLC arrives
        // while the invoice is `Received` (not yet `Paid`).
        let pause = Fault::ProtocolPause {
            node: 2,
            channel: payment_hash,
            point: ProtocolPoint::InvoiceMarkedReceived,
            preceding_hits: 0,
        }
        .inject(&mut self.runtime)?;

        let sent_a = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(invoice_str.clone()),
                ..Default::default()
            },
        )?;
        let sent_b = self.pay(
            1,
            SendPaymentCommand {
                invoice: Some(invoice_str),
                ..Default::default()
            },
        )?;
        ensure!(sent_a == payment_hash && sent_b == payment_hash);

        self.until(
            "payee C marks single-path invoice Received while both channels carry competing TLCs",
            |s| {
                let both_channels_have_tlc = [edge_ac.channel, edge_bc.channel].iter().all(|ch| {
                    s.runtime.channel(2, ch).is_some_and(|st| {
                        st.tlc_state
                            .all_tlcs()
                            .any(|t| t.payment_hash == payment_hash)
                    })
                });
                Ok((pause.hit(&s.runtime) && both_channels_have_tlc).then_some(()))
            },
        )?;
        self.coverage.hit("single_path_double_pay_race");
        self.runtime.note(format_args!(
            "single-path double-pay race armed on C for {payment_hash}; crashing C before RemoveTlc(Fulfill)"
        ));

        // Crash C while the invoice is `Received` and BOTH channels still carry committed full-value
        // TLCs, then heal the pause and restart C. Only the persisted selected TLC may be fulfilled.
        let crash = Fault::Crash(2).inject(&mut self.runtime)?;
        pause.heal(&mut self.runtime);
        for _ in 0..5 {
            self.step()?;
        }
        crash.heal(&mut self.runtime);

        self.until("both competing payers reach terminal payment status", |s| {
            let sa = s.runtime.payment_status(0, payment_hash);
            let sb = s.runtime.payment_status(1, payment_hash);
            Ok(
                (sa.is_some_and(|st| st.is_final()) && sb.is_some_and(|st| st.is_final()))
                    .then_some(()),
            )
        })?;
        self.wait_payment_actor(0)?;
        self.wait_payment_actor(1)?;

        let status_a = self.runtime.payment_status(0, payment_hash);
        let status_b = self.runtime.payment_status(1, payment_hash);
        let succeeded = usize::from(status_a == Some(PaymentStatus::Success))
            + usize::from(status_b == Some(PaymentStatus::Success));
        let failed = usize::from(status_a == Some(PaymentStatus::Failed))
            + usize::from(status_b == Some(PaymentStatus::Failed));
        ensure!(
            succeeded == 1 && failed == 1,
            "single-path double-pay violation: status_a={status_a:?} status_b={status_b:?}"
        );

        let c_ac = self
            .runtime
            .channel(2, &edge_ac.channel)
            .context("missing C->A channel")?;
        let c_bc = self
            .runtime
            .channel(2, &edge_bc.channel)
            .context("missing C->B channel")?;
        let gain_ac = c_ac
            .get_local_balance()
            .checked_sub(edge_ac.before[1])
            .context("negative C balance gain on A-C")?;
        let gain_bc = c_bc
            .get_local_balance()
            .checked_sub(edge_bc.before[1])
            .context("negative C balance gain on B-C")?;
        ensure!(
            gain_ac + gain_bc == amount,
            "single-path invoice double-settled on balances: gain_ac={gain_ac} gain_bc={gain_bc} expected={amount}"
        );
        ensure!(
            self.runtime.store(2)?.get_invoice_status(&payment_hash)
                == Some(CkbInvoiceStatus::Paid),
            "single-path invoice not marked Paid"
        );
        self.runtime.note(format_args!(
            "single-path double-pay verified: status_a={status_a:?} status_b={status_b:?} gain_ac={gain_ac} gain_bc={gain_bc}"
        ));
        Ok(())
    }

    /// Exercise adversarial external-signer SDK flows:
    /// - Probe A: SigningPolicy::Manual + DismissNextConfirmation on an outbound failure refund
    ///   (Zero-Mutation Oracle asserts persistent signer store is unchanged, and follow-up
    ///   `authorize_fulfillment` is NOT poisoned by a premature `Resolution::Cancel`).
    /// - Probe B: Byzantine `TamperNextCommitmentBalance` from LSP rejected with zero store mutation.
    /// - Probe C: Mixed batch removing 1 fulfilled + 1 failed outbound TLC in a single commitment.
    pub(super) fn lsp_signer_adversarial(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_lsp_watchtower()?;
        let _public_edge =
            self.open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?;
        self.wait_trampoline_ready(0, lsp_node)?;
        self.wait_wallet_online(0)?;

        let tenant_id = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        let channel_id = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_id, channel_id)?;

        // Seed SDK-0 with inbound liquidity first so it can send multiple outbound payments.
        let seed_pre = Hash256::from([0x80; 32]);
        let seed_hash: Hash256 = HashAlgorithm::CkbHash.hash(seed_pre).into();
        let seed_inv = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            amount: 10_000 * 100_000_000,
            payment_hash: seed_hash,
            preimage: seed_pre,
            reply,
        })?;
        self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(seed_inv.invoice_address),
                ..Default::default()
            },
        )?;
        self.until("seed inbound payment reaches Received", |s| {
            Ok((s
                .wallet_invoice_status_from_store(&tenant_id, seed_hash)
                .unwrap_or(None)
                == Some(CkbInvoiceStatus::Received))
            .then_some(()))
        })?;
        self.wallet_call(0, |reply| WalletCommand::SettleInvoice {
            payment_hash: seed_hash,
            preimage: seed_pre,
            reply,
        })?;
        self.until("seed inbound payment settled", |s| {
            Ok(
                (s.runtime.payment_status(0, seed_hash) == Some(PaymentStatus::Success))
                    .then_some(()),
            )
        })?;
        self.wait_tenant_idle(&tenant_id)?;

        // Switch SDK-0 to SigningPolicy::Manual so every commitment flows through NeedConfirmation.
        self.wallet_call(0, |reply| WalletCommand::SetSigningPolicy {
            policy: SigningPolicy::Manual,
            reply,
        })?;

        // Probe A: Outbound refund commitment under SigningPolicy::Manual + DismissNextConfirmation.
        let cancel_pre = Hash256::from([0x81; 32]);
        let cancel_hash: Hash256 = HashAlgorithm::CkbHash.hash(cancel_pre).into();
        let cancel_amount = 800 * 100_000_000;
        let cancel_inv = self.create_public_hold_invoice(0, cancel_amount, cancel_hash)?;
        let _ = self.wallet_call(0, |reply| WalletCommand::SendPayment {
            channel_id,
            invoice: cancel_inv,
            amount: cancel_amount,
            payment_hash: cancel_hash,
            use_trampoline: true,
            reply,
        })?;
        self.until("public node A receives hold TLC for Probe A", |s| {
            Ok((s.runtime.store(0)?.get_invoice_status(&cancel_hash)
                == Some(CkbInvoiceStatus::Received))
            .then_some(()))
        })?;

        // Arm DismissNextConfirmation BEFORE cancelling on Node A so when LSP proposes the
        // outbound-refund commitment, SDK-0 prepares it under Manual policy and dismisses it.
        // Our Zero-Mutation Oracle in WalletHost immediately aborts if `store` was mutated!
        self.runtime.wallets.borrow()[0]
            .sdk_fault
            .set(Some(SdkSignerFault::DismissNextConfirmation));
        let mut cancel_reply = self.runtime.request(0, |reply| {
            FiberActorCommand::CancelInvoice(cancel_hash, reply)
        })?;
        self.until("public node A cancels hold invoice", |_| {
            receive(&mut cancel_reply)?.transpose().map_err(Into::into)
        })?;
        self.until(
            "SDK-0 dismisses outbound-refund NeedConfirmation with zero store mutation",
            |s| Ok((s.runtime.wallets.borrow()[0].sdk_confirmations_dismissed >= 1).then_some(())),
        )?;
        self.coverage.hit("sdk_confirm_dismissed");

        // Now verify the dismissed refund did NOT prematurely poison the signer with Cancel
        // by letting the normal confirmation complete and settle the refund cleanly.
        self.until(
            "outbound Probe A payment cleanly fails after confirmation",
            |s| {
                Ok((s.wallet_payment_status_from_store(&tenant_id, cancel_hash)
                    == Some(PaymentStatus::Failed))
                .then_some(()))
            },
        )?;
        self.wait_tenant_idle(&tenant_id)?;

        // Probe B: Byzantine TamperNextCommitmentBalance on an inbound payment.
        let tamper_pre = Hash256::from([0x82; 32]);
        let tamper_hash: Hash256 = HashAlgorithm::CkbHash.hash(tamper_pre).into();
        let tamper_inv = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            amount: 600 * 100_000_000,
            payment_hash: tamper_hash,
            preimage: tamper_pre,
            reply,
        })?;
        self.runtime.wallets.borrow()[0]
            .sdk_fault
            .set(Some(SdkSignerFault::TamperNextCommitmentBalance));
        self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(tamper_inv.invoice_address),
                ..Default::default()
            },
        )?;
        self.until(
            "SDK-0 rejects Byzantine tampered commitment balance with zero store mutation",
            |s| Ok((s.runtime.wallets.borrow()[0].sdk_rejections_verified >= 1).then_some(())),
        )?;
        self.coverage.hit("sdk_tamper_rejected");
        self.until(
            "tampered inbound payment recovers on clean retry poll",
            |s| {
                Ok((s
                    .wallet_invoice_status_from_store(&tenant_id, tamper_hash)
                    .unwrap_or(None)
                    == Some(CkbInvoiceStatus::Received))
                .then_some(()))
            },
        )?;
        self.wallet_call(0, |reply| WalletCommand::SettleInvoice {
            payment_hash: tamper_hash,
            preimage: tamper_pre,
            reply,
        })?;
        self.until("Probe B inbound payment succeeds", |s| {
            Ok(
                (s.runtime.payment_status(0, tamper_hash) == Some(PaymentStatus::Success))
                    .then_some(()),
            )
        })?;
        self.wait_tenant_idle(&tenant_id)?;

        // Probe C: Mixed batch removing 1 fulfilled + 1 failed outbound TLC in a single commitment.
        let batch_ok_pre = Hash256::from([0x83; 32]);
        let batch_ok_hash: Hash256 = HashAlgorithm::CkbHash.hash(batch_ok_pre).into();
        let batch_fail_pre = Hash256::from([0x84; 32]);
        let batch_fail_hash: Hash256 = HashAlgorithm::CkbHash.hash(batch_fail_pre).into();
        let batch_amount = 500 * 100_000_000;
        let inv_ok = self.create_public_hold_invoice(0, batch_amount, batch_ok_hash)?;
        let inv_fail = self.create_public_hold_invoice(0, batch_amount, batch_fail_hash)?;

        let _ = self.wallet_call(0, |reply| WalletCommand::SendPayment {
            channel_id,
            invoice: inv_ok,
            amount: batch_amount,
            payment_hash: batch_ok_hash,
            use_trampoline: true,
            reply,
        })?;
        let _ = self.wallet_call(0, |reply| WalletCommand::SendPayment {
            channel_id,
            invoice: inv_fail,
            amount: batch_amount,
            payment_hash: batch_fail_hash,
            use_trampoline: true,
            reply,
        })?;
        self.until("both Probe C hold invoices reach Received on Node A", |s| {
            let store = s.runtime.store(0)?;
            Ok(
                (store.get_invoice_status(&batch_ok_hash) == Some(CkbInvoiceStatus::Received)
                    && store.get_invoice_status(&batch_fail_hash)
                        == Some(CkbInvoiceStatus::Received))
                .then_some(()),
            )
        })?;

        // Pause SDK-0 signing so both the fulfill and the fail arrive at the LSP->tenant link
        // and get folded into a single commitment update before SDK-0 resumes signing.
        self.runtime.wallets.borrow()[0].signing_paused.set(true);
        let mut settle_reply = self.runtime.request(0, |reply| {
            FiberActorCommand::SettleInvoice(batch_ok_hash, batch_ok_pre, reply)
        })?;
        self.until("Node A settles batch_ok_hash", |_| {
            receive(&mut settle_reply)?.transpose().map_err(Into::into)
        })?;
        let mut cancel_reply = self.runtime.request(0, |reply| {
            FiberActorCommand::CancelInvoice(batch_fail_hash, reply)
        })?;
        self.until("Node A cancels batch_fail_hash", |_| {
            receive(&mut cancel_reply)?.transpose().map_err(Into::into)
        })?;
        // Advance several ticks so both removals propagate back to the hosted channel.
        for _ in 0..40 {
            self.step()?;
        }
        self.runtime.wallets.borrow()[0].signing_paused.set(false);
        self.until(
            "mixed batch outbound fulfill + refund both reach expected final statuses",
            |s| {
                let ok_done = s.wallet_payment_status_from_store(&tenant_id, batch_ok_hash)
                    == Some(PaymentStatus::Success);
                let fail_done = s.wallet_payment_status_from_store(&tenant_id, batch_fail_hash)
                    == Some(PaymentStatus::Failed);
                Ok((ok_done && fail_done).then_some(()))
            },
        )?;
        self.coverage.hit("sdk_mixed_batch_settled");
        self.wait_tenant_idle(&tenant_id)?;
        self.verify_lsp_indexes(&[0, lsp_node], &[tenant_id])?;
        self.runtime.note(format_args!(
            "lsp_signer_adversarial passed: {}",
            self.coverage.format_summary()
        ));
        Ok(())
    }

    /// Exercise two force-close micro-boundaries on a 4-node diamond (0->1->3 and 0->2->3):
    /// - Probe A (`PeerFailRecordedBeforeAck`): relay 1 records peer `RemoveTlcFail(InvoiceCancelled)`
    ///   from payee 3, pauses before ACK, and 3 force-closes `1-3`. Payer 0 MUST receive the
    ///   terminal `InvoiceCancelled` error without retrying over the alternate route `0->2->3`.
    /// - Probe B (`RemoveConfirmedBeforeRelay`): relay 2 receives ACK confirming `RemoveTlcFulfill`
    ///   (`removed_confirmed_at` set, `AppliedFlags::REMOVE` still clear), crashes before relaying
    ///   upstream, 3 force-closes `2-3`, relay 2 restarts. Relay 2 MUST still deliver the preimage
    ///   upstream so payer 0 succeeds.
    pub(super) fn force_close_adversarial(&mut self) -> Result<()> {
        for node in 0..4 {
            self.runtime
                .watchtowers
                .as_ref()
                .context("watchtower controls")?[node]
                .enabled
                .send_replace(true);
        }
        // Make 0->1->3 strictly cheaper (0 ppm) than 0->2->3 (50_000 ppm) so Probe A
        // deterministically routes first through relay 1.
        let _e01 = self.public_edge(0, 1, FUNDING_AMOUNT, 0)?;
        let e13 = self.public_edge(1, 3, FUNDING_AMOUNT, 0)?;
        let _e02 = self.public_edge(0, 2, FUNDING_AMOUNT, 50_000)?;
        let e23 = self.public_edge(2, 3, FUNDING_AMOUNT, 50_000)?;
        self.step()?;

        // Probe A: PeerFailRecordedBeforeAck on relay 1 (channel 1-3).
        let cancel_pre = Hash256::from([0x91; 32]);
        let cancel_hash: Hash256 = HashAlgorithm::CkbHash.hash(cancel_pre).into();
        let cancel_amount = 500 * 100_000_000;
        let cancel_inv = self.create_public_hold_invoice(3, cancel_amount, cancel_hash)?;
        let sent_cancel = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(cancel_inv),
                max_fee_rate: Some(200_000),
                ..Default::default()
            },
        )?;
        ensure!(sent_cancel == cancel_hash);
        self.until("Node 3 receives Probe A hold TLC via relay 1", |s| {
            let received = s.runtime.store(3)?.get_invoice_status(&cancel_hash)
                == Some(CkbInvoiceStatus::Received);
            let on_13 = s.runtime.channel(1, &e13.channel).is_some_and(|st| {
                st.tlc_state
                    .all_tlcs()
                    .any(|t| t.payment_hash == cancel_hash)
            });
            Ok((received && on_13).then_some(()))
        })?;

        let fail_pause = Fault::ProtocolPause {
            node: 1,
            channel: e13.channel,
            point: ProtocolPoint::PeerFailRecordedBeforeAck,
            preceding_hits: 0,
        }
        .inject(&mut self.runtime)?;
        let mut cancel_reply = self.runtime.request(3, |reply| {
            FiberActorCommand::CancelInvoice(cancel_hash, reply)
        })?;
        self.until("Node 3 cancels Probe A hold invoice", |_| {
            receive(&mut cancel_reply)?.transpose().map_err(Into::into)
        })?;
        self.until("relay 1 records peer RemoveTlcFail before ACK", |s| {
            Ok(fail_pause.hit(&s.runtime).then_some(()))
        })?;
        self.coverage.hit("peer_fail_before_ack");

        // Crash relay 1 right at PeerFailRecordedBeforeAck and partition 1-3 so relay 1 never
        // sends RevokeAndAckV2; then restart relay 1 (still partitioned from Node 3) and have
        // relay 1 force-close 1-3 with its newer commitment that already folded out the peer's
        // RemoveTlcFail(InvoiceCancelled). Relay 1 must preserve that recorded error upstream!
        let part_13 = Fault::Partition {
            from: 1,
            to: 3,
            one_way: false,
        }
        .inject(&mut self.runtime)?;
        let crash_1 = Fault::Crash(1).inject(&mut self.runtime)?;
        fail_pause.heal(&mut self.runtime);
        for _ in 0..5 {
            self.step()?;
        }
        crash_1.heal(&mut self.runtime);
        self.until("relay 1 restarts and reconnects to upstream payer 0", |s| {
            Ok((s.runtime.node_available(1) && s.runtime.link(0, 1)).then_some(()))
        })?;
        let commit_13 = self.confirm_force_close(e13.channel, 1, 3)?;
        let _ = self.finish_onchain_close([1, 3], e13.channel, commit_13)?;
        part_13.heal(&mut self.runtime);

        self.until("payer 0 finishes failed Probe A payment", |s| {
            Ok(
                (s.runtime.payment_status(0, cancel_hash) == Some(PaymentStatus::Failed))
                    .then_some(()),
            )
        })?;
        self.wait_payment_actor(0)?;
        let session = self
            .runtime
            .store(0)?
            .get_payment_session(cancel_hash)
            .context("missing payer 0 session for Probe A")?;
        let attempt_count = session.attempts().count();
        ensure!(
            attempt_count == 1,
            "payer 0 retried terminal InvoiceCancelled on alternate route (attempts={attempt_count})"
        );
        ensure!(
            session.last_error_code == Some(fnn::fiber_types::TlcErrorCode::InvoiceCancelled),
            "payer 0 decoded wrong error code across force close: expected Some(InvoiceCancelled), got {:?} (last_error={:?})",
            session.last_error_code,
            session.last_error
        );
        for attempt in session.attempts() {
            ensure!(
                attempt
                    .last_error
                    .as_deref()
                    .is_some_and(|err| err.contains("InvoiceCancelled")),
                "payer 0 attempt {} decoded wrong error across force close: {:?}",
                attempt.id,
                attempt.last_error
            );
        }
        self.runtime.note(format_args!(
            "Probe A passed: preserved peer RemoveTlcFail across force close (attempts={attempt_count})"
        ));

        // Probe B: RemoveConfirmedBeforeRelay on relay 2 (channel 2-3).
        let fulfill_pre = Hash256::from([0x92; 32]);
        let fulfill_hash: Hash256 = HashAlgorithm::CkbHash.hash(fulfill_pre).into();
        let fulfill_amount = 400 * 100_000_000;
        let fulfill_inv = self.create_public_hold_invoice(3, fulfill_amount, fulfill_hash)?;
        let sent_fulfill = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(fulfill_inv),
                max_fee_rate: Some(200_000),
                ..Default::default()
            },
        )?;
        ensure!(sent_fulfill == fulfill_hash);
        self.until("Node 3 receives Probe B hold TLC via relay 2", |s| {
            let received = s.runtime.store(3)?.get_invoice_status(&fulfill_hash)
                == Some(CkbInvoiceStatus::Received);
            let on_23 = s.runtime.channel(2, &e23.channel).is_some_and(|st| {
                st.tlc_state
                    .all_tlcs()
                    .any(|t| t.payment_hash == fulfill_hash)
            });
            Ok((received && on_23).then_some(()))
        })?;

        let relay_pause = Fault::ProtocolPause {
            node: 2,
            channel: e23.channel,
            point: ProtocolPoint::RemoveConfirmedBeforeRelay,
            preceding_hits: 0,
        }
        .inject(&mut self.runtime)?;
        let mut settle_reply = self.runtime.request(3, |reply| {
            FiberActorCommand::SettleInvoice(fulfill_hash, fulfill_pre, reply)
        })?;
        self.until("Node 3 settles Probe B hold invoice", |_| {
            receive(&mut settle_reply)?.transpose().map_err(Into::into)
        })?;
        self.until(
            "relay 2 persists removed_confirmed_at before relaying fulfill upstream",
            |s| Ok(relay_pause.hit(&s.runtime).then_some(())),
        )?;
        self.coverage.hit("remove_confirmed_before_relay");
        ensure!(
            self.runtime.payment_status(0, fulfill_hash) != Some(PaymentStatus::Success),
            "payer 0 already received preimage before crash"
        );

        // Crash relay 2 right at the RemoveConfirmedBeforeRelay boundary, force-close 2-3 from
        // Node 3 while relay 2 is offline, then restart relay 2 and scan on-chain!
        let crash = Fault::Crash(2).inject(&mut self.runtime)?;
        relay_pause.heal(&mut self.runtime);
        for _ in 0..5 {
            self.step()?;
        }
        let close_reply = self.runtime.request(3, |reply| {
            FiberActorCommand::ControlFiberChannel(fnn::fiber::channel::ChannelCommandWithId {
                channel_id: e23.channel,
                command: fnn::fiber::channel::ChannelCommand::Shutdown(
                    fnn::fiber::channel::ShutdownCommand {
                        close_script: None,
                        fee_rate: None,
                        force: true,
                    },
                    reply,
                ),
            })
        })?;
        self.reply(
            "Node 3 force-closes 2-3 while relay 2 is offline",
            close_reply,
        )?;
        let commit_23 = self.until("Node 3 force close 2-3 confirmed locally", |s| {
            Ok(s.runtime.channel(3, &e23.channel).and_then(|st| {
                matches!(st.state, fnn::fiber::ChannelState::Closed(_))
                    .then(|| st.shutdown_transaction_hash.clone())
                    .flatten()
            }))
        })?;
        crash.heal(&mut self.runtime);
        let _ = self.finish_onchain_close([2, 3], e23.channel, Hash256::from(commit_23))?;

        self.until(
            "relay 2 still relays confirmed fulfillment preimage upstream to payer 0",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([0, 2, 3])?;
                }
                Ok(
                    (s.runtime.payment_status(0, fulfill_hash) == Some(PaymentStatus::Success))
                        .then_some(()),
                )
            },
        )?;
        self.wait_payment_actor(0)?;
        self.runtime.note(format_args!(
            "Probe B passed: relay 2 delivered preimage upstream across RemoveConfirmedBeforeRelay crash+force-close"
        ));
        Ok(())
    }

    pub(super) fn create_public_hold_invoice(
        &mut self,
        payee: usize,
        amount: u128,
        payment_hash: Hash256,
    ) -> Result<String> {
        let payee_key = Privkey::from([(payee + 1) as u8; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_hash(payment_hash)
            .payee_pub_key(self.runtime.peers[payee].into())
            .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0))?;
        let invoice_str = invoice.to_string();
        let mut reply = self.runtime.request(payee, |reply| {
            FiberActorCommand::AddInvoice(invoice, None, reply)
        })?;
        self.until("public hold invoice added", |_| {
            receive(&mut reply)?.transpose().map_err(Into::into)
        })?;
        Ok(invoice_str)
    }
}
