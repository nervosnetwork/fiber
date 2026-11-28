use anyhow::{anyhow, ensure, Context, Result};
use ckb_types::core::EpochNumberWithFraction;
use fiber_json_types::ChannelSigningTransition;
use fnn::fiber::channel::{
    ChannelActorState, ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, ChannelEvent,
    ShutdownCommand,
};
use fnn::fiber::config::{DEFAULT_FINAL_TLC_EXPIRY_DELTA, MILLI_SECONDS_PER_EPOCH};
use fnn::fiber::payment::{SendPaymentCommand, SendPaymentWithRouterCommand};
use fnn::fiber::{
    ChannelConnectivityState, ChannelState, FiberActorCommand, Hash256, NetworkActorMessage,
    PaymentStatus,
};
use fnn::fiber_types::{CloseFlags, HashAlgorithm, RouterHop, ShuttingDownFlags};
use fnn::invoice::{CkbInvoiceStatus, InvoiceStore};
use fnn::lsp::{LspPaymentDeliveryStore, LspServiceMessage, TenantId};
use fnn::store::NodeNamespace;
use tokio::sync::oneshot;

use crate::host::{send, FrameKindFilter, FrameMatcher};
use crate::lsp::{RefusalTarget, WalletCommand};
use crate::oracle::{ConsistencyOracle, Endpoint, LivenessOracle};

use super::faults::Fault;
use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;
const PEER_RESPONSE_TIMEOUT_ADVANCE_MS: u64 = 35_000;
const REESTABLISH_TIMEOUT_ADVANCE_MS: u64 = 310_000;
const EXTERNAL_FUNDING_TIMEOUT_ADVANCE_MS: u64 = 310_000;
const DEFAULT_FUNDING_TIMEOUT_ADVANCE_MS: u64 = 86_500_000;

fn tlc_expiry_delay_ms(commitment_delay_epoch: u64) -> u64 {
    let delay_epoch = EpochNumberWithFraction::from_full_value(commitment_delay_epoch);
    ((delay_epoch.number() as f64 + delay_epoch.index() as f64 / delay_epoch.length() as f64)
        * MILLI_SECONDS_PER_EPOCH as f64
        * 2.0
        / 3.0) as u64
}

impl Driver {
    /// Exercise P2P liveness hangs and autonomous escape-hatch force-closes:
    /// - Probe 1: Blackholed `RevokeAndAckV2` -> autonomous `PEER_CHANNEL_RESPONSE_TIMEOUT` force-close.
    /// - Probe 2: Blackholed `RemoveTlc` -> autonomous `force_close_expiring_offered_tlcs` on relay.
    /// - Probe 3: Blackholed `ReestablishChannelV2` -> autonomous `is_reestablish_stalled` force-close.
    pub(super) fn liveness_hang_and_auto_force_close(&mut self) -> Result<()> {
        self.start_all_watchtowers(3)?;

        // Probe 1: Blackholed RevokeAndAckV2 -> autonomous PEER_CHANNEL_RESPONSE_TIMEOUT force close.
        let ch1 = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        let funding1 = self
            .runtime
            .channel(0, &ch1)
            .context("missing ch1 opener state")?
            .get_funding_transaction_outpoint()
            .context("missing ch1 funding outpoint")?;

        let bh_ack = Fault::BlackholePeer {
            from: 1,
            to: 0,
            matcher: FrameMatcher::channel(FrameKindFilter::RevokeAndAckV2, ch1),
        }
        .inject(&mut self.runtime)?;

        let pre1 = Hash256::from([0xB1; 32]);
        let p1: Hash256 = HashAlgorithm::CkbHash.hash(pre1).into();
        let amount1 = 500 * 100_000_000;
        let inv1 = self.create_public_hold_invoice(1, amount1, p1)?;
        self.send_direct_routed_payment(0, 1, funding1, inv1, amount1, p1)?;

        self.until(
            "Node 0 waiting_ack on ch1 and RevokeAndAckV2 blackholed",
            |s| {
                let waiting = s
                    .runtime
                    .channel(0, &ch1)
                    .is_some_and(|st| st.tlc_state.waiting_ack);
                Ok((waiting && bh_ack.hit(&s.runtime)).then_some(()))
            },
        )?;
        self.coverage.hit("liveness_ack_blackhole_force_close");

        // Advance past PEER_CHANNEL_RESPONSE_TIMEOUT (30s) without calling Shutdown manually.
        self.advance_clock_by(PEER_RESPONSE_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([0, 1])?;

        let commit1 = self.until(
            "Node 0 autonomously force-closes ch1 after RevokeAndAckV2 blackhole",
            |s| {
                Ok(s.runtime.channel(0, &ch1).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                }))
            },
        )?;
        self.runtime.note(format_args!(
            "Probe 1: Node 0 autonomously force-closed ch1={ch1} commit={commit1}"
        ));

        let ch1_max_expiry = [0, 1]
            .into_iter()
            .filter_map(|node| self.runtime.channel(node, &ch1))
            .flat_map(|st| {
                st.tlc_state
                    .all_tlcs()
                    .map(|tlc| tlc.expiry)
                    .collect::<Vec<_>>()
            })
            .max();
        match ch1_max_expiry {
            Some(expiry) => self.advance_clock_past(expiry),
            None => self.advance_clock_past(self.clock_ms()),
        }
        let _ = self.finish_onchain_close([0, 1], ch1, commit1)?;
        bh_ack.heal(&mut self.runtime);

        self.until("Node 0 payment p1 reaches Failed after Probe 1", |s| {
            if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                s.scan_onchain([0, 1])?;
            }
            Ok((s.runtime.payment_status(0, p1) == Some(PaymentStatus::Failed)).then_some(()))
        })?;
        self.wait_payment_actor(0)?;
        ensure!(
            self.runtime.store(1)?.get_invoice_status(&p1) != Some(CkbInvoiceStatus::Paid),
            "Probe 1 timed-out hold invoice p1 was marked Paid on Node 1"
        );

        // Probe 2: Blackholed RemoveTlc -> autonomous force_close_expiring_offered_tlcs on relay 1.
        let e_ch2 = self.public_edge(0, 1, FUNDING_AMOUNT, 0)?;
        let e_bc = self.public_edge(1, 2, FUNDING_AMOUNT, 0)?;
        let ch2 = e_ch2.channel;
        let ch_bc = e_bc.channel;
        self.step()?;

        let pre2 = Hash256::from([0xB2; 32]);
        let p2: Hash256 = HashAlgorithm::CkbHash.hash(pre2).into();
        let amount2 = 600 * 100_000_000;
        let inv2 = self.create_public_hold_invoice(2, amount2, p2)?;
        let sent_p2 = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(inv2),
                ..Default::default()
            },
        )?;
        ensure!(sent_p2 == p2);

        self.until("p2 is Received on Node 2 and committed on both hops", |s| {
            let received =
                s.runtime.store(2)?.get_invoice_status(&p2) == Some(CkbInvoiceStatus::Received);
            let committed_ab = [0, 1].into_iter().all(|n| {
                s.runtime.channel(n, &ch2).is_some_and(|st| {
                    !st.tlc_state.waiting_ack
                        && st.tlc_state.all_tlcs().any(|tlc| tlc.payment_hash == p2)
                })
            });
            let committed_bc = [1, 2].into_iter().all(|n| {
                s.runtime.channel(n, &ch_bc).is_some_and(|st| {
                    !st.tlc_state.waiting_ack
                        && st.tlc_state.all_tlcs().any(|tlc| tlc.payment_hash == p2)
                })
            });
            Ok((received && committed_ab && committed_bc).then_some(()))
        })?;

        let bh_remove = Fault::BlackholePeer {
            from: 2,
            to: 1,
            matcher: FrameMatcher::channel(FrameKindFilter::RemoveTlc, ch_bc),
        }
        .inject(&mut self.runtime)?;

        let (expiry_bc, delay_bc) = {
            let st = self
                .runtime
                .channel(1, &ch_bc)
                .context("missing relay 1 ch_bc state")?;
            let expiry = st
                .tlc_state
                .offered_tlcs
                .tlcs
                .iter()
                .find(|tlc| tlc.payment_hash == p2)
                .context("missing offered TLC p2 on relay 1 ch_bc")?
                .expiry;
            (expiry, tlc_expiry_delay_ms(st.commitment_delay_epoch))
        };
        let safety_target = expiry_bc.saturating_sub(delay_bc / 2);
        let advance_delta = safety_target.saturating_sub(self.clock_ms()).max(1);
        self.advance_clock_by(advance_delta);
        self.trigger_channel_maintenance([0, 1, 2])?;
        self.notify_channel_event(1, ch_bc, ChannelEvent::MaintainChannelTlcs)?;

        let commit_bc = self.until(
            "relay 1 autonomously force-closes ch_bc on expiring offered TLC",
            |s| {
                Ok(s.runtime.channel(1, &ch_bc).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                }))
            },
        )?;
        self.coverage
            .hit("liveness_expiring_offered_tlc_force_close");
        self.runtime.note(format_args!(
            "Probe 2: relay 1 autonomously force-closed ch_bc={ch_bc} commit={commit_bc}"
        ));

        self.advance_clock_past(expiry_bc);
        let _ = self.finish_onchain_close([1, 2], ch_bc, commit_bc)?;
        bh_remove.heal(&mut self.runtime);

        self.until(
            "payer 0 payment p2 reaches Failed (refunded) after Probe 2",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([0, 1, 2])?;
                }
                Ok((s.runtime.payment_status(0, p2) == Some(PaymentStatus::Failed)).then_some(()))
            },
        )?;
        self.wait_payment_actor(0)?;
        self.until("upstream ch2 settled pending refund TLCs cleanly", |s| {
            let idle = [0, 1].into_iter().all(|n| {
                s.runtime.channel(n, &ch2).is_some_and(|st| {
                    st.state == ChannelState::ChannelReady
                        && !st.tlc_state.waiting_ack
                        && !st.any_tlc_pending()
                })
            });
            Ok(idle.then_some(()))
        })?;
        ensure!(
            self.runtime.store(2)?.get_invoice_status(&p2) != Some(CkbInvoiceStatus::Paid),
            "Probe 2 timed-out hold invoice p2 was marked Paid on Node 2"
        );

        // Probe 3: Reconnect blackhole ReestablishChannelV2 -> autonomous is_reestablish_stalled.
        let ch3 = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        let bh_reest = Fault::BlackholePeer {
            from: 1,
            to: 0,
            matcher: FrameMatcher::channel(FrameKindFilter::ReestablishChannelV2, ch3),
        }
        .inject(&mut self.runtime)?;

        self.runtime.reset_link(0, 1);
        self.until(
            "Node 0 reconnects and stalls in reestablishing on ch3",
            |s| {
                let connected = s.runtime.link(0, 1);
                let stalled = s.runtime.channel(0, &ch3).is_some_and(|st| {
                    st.reestablishing && st.connectivity_state == ChannelConnectivityState::Syncing
                });
                Ok((connected && stalled && bh_reest.hit(&s.runtime)).then_some(()))
            },
        )?;
        self.coverage
            .hit("liveness_reestablish_stalled_force_close");

        self.advance_clock_by(REESTABLISH_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([0, 1])?;

        let commit3 = self.until(
            "Node 0 autonomously force-closes ch3 after REESTABLISH_TIMEOUT",
            |s| {
                Ok(s.runtime.channel(0, &ch3).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                }))
            },
        )?;
        self.runtime.note(format_args!(
            "Probe 3: Node 0 autonomously force-closed stalled ch3={ch3} commit={commit3}"
        ));

        self.advance_clock_past(self.clock_ms());
        let _ = self.finish_onchain_close([0, 1], ch3, commit3)?;
        bh_reest.heal(&mut self.runtime);
        self.wait_online(0, 1, &ch2)?;

        LivenessOracle::assert_converged(
            &self.runtime,
            &[(Endpoint::Public(0), p1), (Endpoint::Public(0), p2)],
            &[],
        )?;
        ConsistencyOracle::audit(
            &self.runtime,
            &[
                (Endpoint::Public(0), Endpoint::Public(1), p1),
                (Endpoint::Public(0), Endpoint::Public(2), p2),
            ],
            &[
                (Endpoint::Public(0), Endpoint::Public(1), ch1),
                (Endpoint::Public(1), Endpoint::Public(2), ch_bc),
                (Endpoint::Public(0), Endpoint::Public(1), ch3),
            ],
        )?;
        self.runtime
            .note("liveness_hang_and_auto_force_close passed");
        Ok(())
    }

    /// Exercise permanent external SDK signing refusal and LSP/tenant escape hatches:
    /// - Probe 1: Permanent `OpeningFundingSubmit` refusal -> external funding timeout cleanup.
    /// - Probe 2: Permanent `RefusalTarget::All` with active TLC -> autonomous
    ///   `force_close_expired_signature` + on-chain watchtower settlement + payer refund.
    pub(super) fn sdk_permanent_refusal_and_escape(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_lsp_watchtower()?;
        let public_edge =
            self.open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?;
        self.wait_trampoline_ready(0, lsp_node)?;
        self.wait_wallet_online(0)?;

        let tenant_id = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;

        // Probe 1: Permanent refusal OpeningFundingSubmit -> external funding timeout cleanup.
        let refusals_before = self.runtime.wallets.borrow()[0].sdk_refusals_hit;
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .refusal = Some(RefusalTarget::OpeningFundingSubmit);

        let aborted_ch = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        ensure!(
            self.runtime.wallets.borrow()[0].sdk_refusals_hit > refusals_before,
            "SDK-0 did not record OpeningFundingSubmit refusal"
        );
        self.coverage.hit("sdk_refusal_opening_funding_submit");
        self.runtime.note(format_args!(
            "Probe 1: SDK-0 refused OpeningFundingSubmit for aborted_ch={aborted_ch}"
        ));

        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .refusal = None;

        self.advance_clock_by(EXTERNAL_FUNDING_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([0, lsp_node])?;

        // Restart LSP and re-activate tenant so ChannelActor::post_start re-evaluates
        // schedule_external_funding_timeout_check against the advanced virtual clock and
        // immediately dispatches ChannelEvent::CheckFundingTimeout.
        self.runtime.crash(lsp_node);
        for _ in 0..4 {
            self.step()?;
        }
        self.runtime.restart(lsp_node);
        self.until("LSP service online after restart", |s| {
            Ok(s.runtime.lsp_slot.borrow().is_some().then_some(()))
        })?;
        self.start_lsp_watchtower()?;
        self.until("Public A reconnects to restarted LSP", |s| {
            Ok(s.runtime.link(0, lsp_node).then_some(()))
        })?;
        self.ensure_tenant_active(&tenant_id)?;

        self.until(
            "aborted external-funding channel cleaned up in tenant store",
            |s| {
                Ok(s.tenant_channel_state(&tenant_id, aborted_ch)?
                    .is_none()
                    .then_some(()))
            },
        )?;

        // Give tenant's TxAbort a window to clean up LSP's acceptor record; if the abort fired
        // before in-process peer registration completed, advance past LSP's acceptor funding
        // timeout and trigger CheckFundingTimeout on LSP as well.
        let mut lsp_cleaned = false;
        for _ in 0..200 {
            if self.runtime.funding_record_absent(lsp_node, aborted_ch)? {
                lsp_cleaned = true;
                break;
            }
            self.step()?;
        }
        if !lsp_cleaned {
            self.advance_clock_by(DEFAULT_FUNDING_TIMEOUT_ADVANCE_MS);
            self.notify_channel_event(lsp_node, aborted_ch, ChannelEvent::CheckFundingTimeout)?;
            self.until(
                "aborted external-funding channel cleaned up on LSP after funding timeout",
                |s| {
                    Ok(s.runtime
                        .funding_record_absent(lsp_node, aborted_ch)?
                        .then_some(()))
                },
            )?;
        }
        ensure!(
            self.runtime.funding_record_absent(lsp_node, aborted_ch)?
                && self.tenant_channel_state(&tenant_id, aborted_ch)?.is_none(),
            "aborted external-funding channel {aborted_ch} was not cleaned up on both sides"
        );
        self.wait_online(0, lsp_node, &public_edge.channel)?;
        self.runtime.note(format_args!(
            "Probe 1: aborted external-funding channel {aborted_ch} cleaned up on LSP and tenant"
        ));

        // Probe 2: Permanent SDK refusal with active TLC -> autonomous force_close_expired_signature.
        let ch_ok = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_id, ch_ok)?;

        let pre1 = Hash256::from([0xC1; 32]);
        let p1: Hash256 = HashAlgorithm::CkbHash.hash(pre1).into();
        let amount1 = 1_000 * 100_000_000;
        let inv1 = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            amount: amount1,
            payment_hash: p1,
            preimage: pre1,
            reply,
        })?;
        let sent_p1 = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(inv1.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_p1 == p1);
        self.until("p1 hold invoice committed and Received on tenant", |s| {
            let received = s
                .wallet_invoice_status_from_store(&tenant_id, p1)
                .unwrap_or(None)
                == Some(CkbInvoiceStatus::Received);
            let tenant_idle = s
                .tenant_channel_state(&tenant_id, ch_ok)?
                .is_some_and(|st| {
                    !st.signing_context.is_awaiting_signature() && !st.tlc_state.waiting_ack
                });
            let lsp_idle = s
                .runtime
                .channel(lsp_node, &ch_ok)
                .is_some_and(|st| !st.tlc_state.waiting_ack);
            Ok((received && tenant_idle && lsp_idle).then_some(()))
        })?;

        // Arm permanent RefusalTarget::All and trigger a second payment p2 so ch_ok enters
        // AwaitingSignature while unsettled hold TLC p1 is active on ch_ok.
        let refusals_before_p2 = self.runtime.wallets.borrow()[0].sdk_refusals_hit;
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .refusal = Some(RefusalTarget::All);

        let pre2 = Hash256::from([0xC2; 32]);
        let p2: Hash256 = HashAlgorithm::CkbHash.hash(pre2).into();
        let amount2 = 500 * 100_000_000;
        let inv2 = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            amount: amount2,
            payment_hash: p2,
            preimage: pre2,
            reply,
        })?;
        let sent_p2 = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(inv2.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_p2 == p2);

        self.until(
            "tenant channel ch_ok enters AwaitingSignature and SDK-0 permanently refuses",
            |s| {
                let awaiting = s
                    .tenant_channel_state(&tenant_id, ch_ok)?
                    .is_some_and(|st| st.signing_context.is_awaiting_signature());
                let refused = s.runtime.wallets.borrow()[0].sdk_refusals_hit > refusals_before_p2;
                Ok((awaiting && refused).then_some(()))
            },
        )?;
        self.coverage.hit("sdk_permanent_refusal_awaiting_sig");

        // Advance virtual clock into ch_ok's TLC signature safety deadline (before public hop 0-1
        // expires!) so MaintainChannelTlcs fires periodic_signature_tlc_deadline_reached ->
        // force_close_expired_signature.
        let (p1_expiry, ch_ok_delay_ms) = {
            let st = self
                .tenant_channel_state(&tenant_id, ch_ok)?
                .context("missing tenant ch_ok state")?;
            let expiry = st
                .tlc_state
                .all_tlcs()
                .find(|tlc| tlc.payment_hash == p1)
                .context("missing p1 TLC on tenant ch_ok")?
                .expiry;
            (expiry, tlc_expiry_delay_ms(st.commitment_delay_epoch))
        };
        let deadline_target = p1_expiry.saturating_sub(ch_ok_delay_ms / 2);
        let delta = deadline_target.saturating_sub(self.clock_ms()).max(1);
        self.advance_clock_by(delta);
        self.trigger_channel_maintenance([0, lsp_node])?;

        let commit_ok = self.until(
            "ch_ok autonomously force-closes past TLC signature safety deadline",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                }
                let lsp_closed = s.runtime.channel(lsp_node, &ch_ok).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags)
                            if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                                || flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                });
                let tenant_closed = s
                    .tenant_channel_state(&tenant_id, ch_ok)?
                    .is_some_and(|st| {
                        matches!(st.state, ChannelState::Closed(_))
                            && !st.signing_context.is_awaiting_signature()
                    });
                Ok(tenant_closed.then_some(lsp_closed).flatten())
            },
        )?;
        self.coverage
            .hit("sdk_refusal_auto_force_close_expired_sig");
        self.runtime.note(format_args!(
            "Probe 2: ch_ok={ch_ok} autonomously force-closed commit={commit_ok} under permanent SDK refusal"
        ));

        // Advance past all TLC expiries on ch_ok so LSP's watchtower settles on-chain even while
        // RefusalTarget::All remains armed, and both p1 & p2 fail cleanly on Public Node 0.
        let max_expiry = self
            .runtime
            .channel(lsp_node, &ch_ok)
            .into_iter()
            .flat_map(|st| {
                st.tlc_state
                    .all_tlcs()
                    .map(|tlc| tlc.expiry)
                    .collect::<Vec<_>>()
            })
            .chain(std::iter::once(p1_expiry))
            .max()
            .unwrap_or(p1_expiry);
        self.advance_clock_past(max_expiry);

        // Phase 2a: Even while RefusalTarget::All remains armed (SDK permanently offline/refusing),
        // LSP's own watchtower MUST reclaim expired offered TLC p1 on-chain and fail p1/p2 cleanly
        // back to Public Node 0 so LSP & the public network never hang or lose funds.
        let tenant_ctx = self
            .lsp_call(|reply| LspServiceMessage::GetTenantRpcContext(tenant_id.clone(), reply))?;
        self.until(
            "LSP settles expired TLC on ch_ok and fails p1/p2 on Public Node 0 while SDK refuses",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                }
                if s.runtime.elapsed().as_millis().is_multiple_of(250) {
                    s.trigger_channel_maintenance([0, lsp_node])?;
                    tenant_ctx.check_channels();
                    tenant_ctx.check_channels_shutdown();
                }
                let lsp_closed = s
                    .runtime
                    .channel(lsp_node, &ch_ok)
                    .is_some_and(|st| matches!(st.state, ChannelState::Closed(_)));
                let p1_failed = s.runtime.payment_status(0, p1) == Some(PaymentStatus::Failed);
                let p2_failed = s.runtime.payment_status(0, p2) == Some(PaymentStatus::Failed);
                let deliveries_final = s.lsp_deliveries_final(lsp_node, &[p1, p2])?;
                Ok((lsp_closed && p1_failed && p2_failed && deliveries_final).then_some(()))
            },
        )?;
        self.wait_payment_actor(0)?;

        // Phase 2b: Clear refusal so SDK-0 can self-fund & claim its own remaining balance output
        // on commit_ok, completing full on-chain settlement across both endpoints.
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .refusal = None;

        self.until("SDK-0 claims remaining on-chain output on commit_ok", |s| {
            if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                s.scan_onchain([lsp_node])?;
                tenant_ctx.check_channels_shutdown();
            }
            let chain_settled = s.runtime.chain.commitment_settled(commit_ok);
            let lsp_done = s.runtime.channel(lsp_node, &ch_ok).is_some_and(|st| {
                matches!(st.state, ChannelState::Closed(_)) && !st.is_waiting_onchain_settlement()
            });
            let tenant_done = s
                .tenant_channel_state(&tenant_id, ch_ok)?
                .is_some_and(|st| {
                    matches!(st.state, ChannelState::Closed(_))
                        && !st.is_waiting_onchain_settlement()
                        && !st.signing_context.is_awaiting_signature()
                });
            Ok((chain_settled && lsp_done && tenant_done).then_some(()))
        })?;

        self.until("public channel 0-1 finishes refund ACK cleanly", |s| {
            let idle = [0, lsp_node].into_iter().all(|n| {
                s.runtime
                    .channel(n, &public_edge.channel)
                    .is_some_and(|st| {
                        st.state == ChannelState::ChannelReady
                            && !st.tlc_state.waiting_ack
                            && !st.any_tlc_pending()
                    })
            });
            Ok(idle.then_some(()))
        })?;

        LivenessOracle::assert_converged(
            &self.runtime,
            &[(Endpoint::Public(0), p1), (Endpoint::Public(0), p2)],
            &[p1, p2],
        )?;
        ConsistencyOracle::audit(
            &self.runtime,
            &[
                (Endpoint::Public(0), Endpoint::Tenant(tenant_id.clone()), p1),
                (Endpoint::Public(0), Endpoint::Tenant(tenant_id.clone()), p2),
            ],
            &[(
                Endpoint::Public(lsp_node),
                Endpoint::Tenant(tenant_id),
                ch_ok,
            )],
        )?;
        self.runtime.note("sdk_permanent_refusal_and_escape passed");
        Ok(())
    }

    /// Exercise cooperative-close escalation to force-close across P2P and hosted SDK paths:
    /// - Probe 1: Blackholed `ClosingSignedV2` -> autonomous `superseded_by_force_close` escalation.
    /// - Probe 2: Cooperative close with pending hold TLC -> `force_close_expiring_offered_tlcs`
    ///   preempts `ShuttingDown`.
    /// - Probe 3: SDK permanently refuses `SendClosingSigned` -> LSP counterparty autonomously
    ///   escalates to force-close and both LSP & tenant converge to `Closed`.
    pub(super) fn shutdown_escalation_and_race(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_watchtowers([0, lsp_node])?;

        // Probe 1: Public coop close hung by blackholed ClosingSignedV2 -> superseded_by_force_close.
        let ch1 = self
            .open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?
            .channel;
        let bh_1_to_0 = Fault::BlackholePeer {
            from: lsp_node,
            to: 0,
            matcher: FrameMatcher::channel(FrameKindFilter::ClosingSignedV2, ch1),
        }
        .inject(&mut self.runtime)?;
        let bh_0_to_1 = Fault::BlackholePeer {
            from: 0,
            to: lsp_node,
            matcher: FrameMatcher::channel(FrameKindFilter::ClosingSignedV2, ch1),
        }
        .inject(&mut self.runtime)?;

        let close_ch1 = self.runtime.request(0, |reply| {
            FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: ch1,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: None,
                        fee_rate: None,
                        force: false,
                    },
                    reply,
                ),
            })
        })?;
        self.reply("Node 0 initiates cooperative close on ch1", close_ch1)?;

        self.until(
            "Node 0 enters ShuttingDown on ch1 while ClosingSignedV2 is blackholed",
            |s| {
                let shutting_down =
                    s.runtime.channel(0, &ch1).is_some_and(|st| {
                        matches!(st.state, ChannelState::ShuttingDown(_))
                            && st.session.closing.as_ref().is_some_and(|c| {
                                c.response.is_some() && c.remote_response.is_none()
                            })
                    });
                Ok((shutting_down && bh_1_to_0.hit(&s.runtime)).then_some(()))
            },
        )?;
        self.coverage.hit("shutdown_closing_signed_blackhole");

        self.advance_clock_by(PEER_RESPONSE_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([0, lsp_node])?;
        self.notify_channel_event(0, ch1, ChannelEvent::MaintainChannelTlcs)?;

        let commit1 = self.until(
            "Node 0 autonomously escalates hung cooperative close ch1 to force-close",
            |s| {
                Ok(s.runtime.channel(0, &ch1).and_then(|st| {
                    let escalated = matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    ) && st
                        .session
                        .closing
                        .as_ref()
                        .is_some_and(|c| c.superseded_by_force_close);
                    escalated
                        .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                        .flatten()
                }))
            },
        )?;
        self.coverage.hit("shutdown_superseded_by_force_close");
        self.runtime.note(format_args!(
            "Probe 1: Node 0 escalated hung coop close ch1={ch1} to force-close commit={commit1}"
        ));

        self.advance_clock_past(self.clock_ms());
        let _ = self.finish_onchain_close([0, lsp_node], ch1, commit1)?;
        bh_1_to_0.heal(&mut self.runtime);
        bh_0_to_1.heal(&mut self.runtime);

        // Probe 2: Coop close with pending hold TLC -> force_close_expiring_offered_tlcs preempts ShuttingDown.
        let ch2 = self
            .open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?
            .channel;
        let funding2 = self
            .runtime
            .channel(0, &ch2)
            .context("missing ch2 opener state")?
            .get_funding_transaction_outpoint()
            .context("missing ch2 funding outpoint")?;

        let pre_hold = Hash256::from([0xD2; 32]);
        let p_hold: Hash256 = HashAlgorithm::CkbHash.hash(pre_hold).into();
        let hold_amount = 500 * 100_000_000;
        let hold_inv = self.create_public_hold_invoice(lsp_node, hold_amount, p_hold)?;
        self.send_direct_routed_payment(0, lsp_node, funding2, hold_inv, hold_amount, p_hold)?;

        self.until("hold TLC p_hold committed on ch2", |s| {
            let received = s.runtime.store(lsp_node)?.get_invoice_status(&p_hold)
                == Some(CkbInvoiceStatus::Received);
            let committed = [0, lsp_node].into_iter().all(|n| {
                s.runtime.channel(n, &ch2).is_some_and(|st| {
                    !st.tlc_state.waiting_ack
                        && st
                            .tlc_state
                            .all_tlcs()
                            .any(|tlc| tlc.payment_hash == p_hold)
                })
            });
            Ok((received && committed).then_some(()))
        })?;

        let (hold_expiry, ch2_delay_ms) = {
            let st = self
                .runtime
                .channel(0, &ch2)
                .context("missing Node 0 ch2 state")?;
            let expiry = st
                .tlc_state
                .offered_tlcs
                .tlcs
                .iter()
                .find(|tlc| tlc.payment_hash == p_hold)
                .context("missing offered TLC p_hold on Node 0 ch2")?
                .expiry;
            (expiry, tlc_expiry_delay_ms(st.commitment_delay_epoch))
        };

        let close_ch2 = self.runtime.request(0, |reply| {
            FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: ch2,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: None,
                        fee_rate: None,
                        force: false,
                    },
                    reply,
                ),
            })
        })?;
        self.reply(
            "Node 0 initiates cooperative close on ch2 with pending TLC",
            close_ch2,
        )?;

        self.until(
            "ch2 waits in ShuttingDown(AWAITING_PENDING_TLCS) for pending hold TLC",
            |s| {
                let waiting_a = s.runtime.channel(0, &ch2).is_some_and(|st| {
                    matches!(
                        st.state,
                        ChannelState::ShuttingDown(flags)
                            if flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS)
                    )
                });
                let waiting_b = s
                    .runtime
                    .channel(lsp_node, &ch2)
                    .is_some_and(|st| matches!(st.state, ChannelState::ShuttingDown(_)));
                Ok((waiting_a && waiting_b).then_some(()))
            },
        )?;

        // Advance past expiry - tlc_expiry_delay so force_close_expiring_offered_tlcs preempts ShuttingDown.
        let preempt_target = hold_expiry.saturating_sub(ch2_delay_ms / 2);
        let delta = preempt_target.saturating_sub(self.clock_ms()).max(1);
        self.advance_clock_by(delta);
        self.trigger_channel_maintenance([0, lsp_node])?;
        self.notify_channel_event(0, ch2, ChannelEvent::MaintainChannelTlcs)?;

        let commit2 = self.until(
            "Node 0 autonomously preempts ShuttingDown on ch2 with force-close",
            |s| {
                Ok(s.runtime.channel(0, &ch2).and_then(|st| {
                    let escalated = matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    ) && st
                        .session
                        .closing
                        .as_ref()
                        .is_some_and(|c| c.superseded_by_force_close);
                    escalated
                        .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                        .flatten()
                }))
            },
        )?;
        self.coverage.hit("shutdown_preempted_by_expiring_tlc");
        self.runtime.note(format_args!(
            "Probe 2: Node 0 preempted ShuttingDown on ch2={ch2} with force-close commit={commit2}"
        ));

        self.advance_clock_past(hold_expiry);
        let _ = self.finish_onchain_close([0, lsp_node], ch2, commit2)?;

        self.until("Node 0 payment p_hold fails cleanly after Probe 2", |s| {
            if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                s.scan_onchain([0, lsp_node])?;
            }
            Ok((s.runtime.payment_status(0, p_hold) == Some(PaymentStatus::Failed)).then_some(()))
        })?;
        self.wait_payment_actor(0)?;
        ensure!(
            self.runtime.store(lsp_node)?.get_invoice_status(&p_hold)
                != Some(CkbInvoiceStatus::Paid),
            "Probe 2 timed-out hold invoice p_hold was marked Paid"
        );

        // Probe 3: SDK permanently refuses SendClosingSigned -> LSP counterparty escalates to force-close.
        let _public_edge =
            self.open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?;
        self.wait_trampoline_ready(0, lsp_node)?;
        self.wait_wallet_online(0)?;

        let tenant_id = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        let ch_hosted = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_id, ch_hosted)?;

        let refusals_before = self.runtime.wallets.borrow()[0].sdk_refusals_hit;
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .refusal = Some(RefusalTarget::Transition(
            ChannelSigningTransition::SendClosingSigned,
        ));

        self.wallet_call(0, |reply| WalletCommand::CooperativeClose {
            channel_id: ch_hosted,
            fee_rate: 1_000,
            reply,
        })?;

        self.until(
            "SDK-0 refuses SendClosingSigned and LSP waits in ShuttingDown on ch_hosted",
            |s| {
                let refused = s.runtime.wallets.borrow()[0].sdk_refusals_hit > refusals_before;
                let tenant_awaiting = s
                    .tenant_channel_state(&tenant_id, ch_hosted)?
                    .is_some_and(|st| st.signing_context.is_awaiting_signature());
                let lsp_waiting =
                    s.runtime.channel(lsp_node, &ch_hosted).is_some_and(|st| {
                        matches!(st.state, ChannelState::ShuttingDown(_))
                            && st.session.closing.as_ref().is_some_and(|c| {
                                c.response.is_some() && c.remote_response.is_none()
                            })
                    });
                Ok((refused && tenant_awaiting && lsp_waiting).then_some(()))
            },
        )?;
        self.coverage.hit("sdk_refusal_send_closing_signed");

        self.advance_clock_by(PEER_RESPONSE_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([lsp_node])?;

        let commit_hosted = self.until(
            "LSP counterparty autonomously escalates hung hosted close to force-close",
            |s| {
                Ok(s.runtime.channel(lsp_node, &ch_hosted).and_then(|st| {
                    let escalated = matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    ) && st
                        .session
                        .closing
                        .as_ref()
                        .is_some_and(|c| c.superseded_by_force_close);
                    escalated
                        .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                        .flatten()
                }))
            },
        )?;
        self.coverage.hit("lsp_counterparty_escalated_hosted_close");
        self.runtime.note(format_args!(
            "Probe 3: LSP escalated hung hosted close ch_hosted={ch_hosted} to force-close commit={commit_hosted}"
        ));

        let tenant_ctx = self
            .lsp_call(|reply| LspServiceMessage::GetTenantRpcContext(tenant_id.clone(), reply))?;
        self.advance_clock_past(self.clock_ms());
        self.until(
            "ch_hosted force-close settles on-chain and both LSP & tenant converge to Closed",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                    tenant_ctx.check_channels_shutdown();
                }
                let chain_settled = s.runtime.chain.commitment_settled(commit_hosted);
                let lsp_done = s.runtime.channel(lsp_node, &ch_hosted).is_some_and(|st| {
                    matches!(st.state, ChannelState::Closed(_))
                        && !st.is_waiting_onchain_settlement()
                });
                let tenant_done =
                    s.tenant_channel_state(&tenant_id, ch_hosted)?
                        .is_some_and(|st| {
                            matches!(st.state, ChannelState::Closed(_))
                                && !st.is_waiting_onchain_settlement()
                                && !st.signing_context.is_awaiting_signature()
                        });
                Ok((chain_settled && lsp_done && tenant_done).then_some(()))
            },
        )?;

        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .refusal = None;

        LivenessOracle::assert_converged(&self.runtime, &[(Endpoint::Public(0), p_hold)], &[])?;
        ConsistencyOracle::audit(
            &self.runtime,
            &[(Endpoint::Public(0), Endpoint::Public(lsp_node), p_hold)],
            &[
                (Endpoint::Public(0), Endpoint::Public(lsp_node), ch1),
                (Endpoint::Public(0), Endpoint::Public(lsp_node), ch2),
                (
                    Endpoint::Public(lsp_node),
                    Endpoint::Tenant(tenant_id),
                    ch_hosted,
                ),
            ],
        )?;
        self.runtime.note("shutdown_escalation_and_race passed");
        Ok(())
    }

    fn start_all_watchtowers(&mut self, count: usize) -> Result<()> {
        let towers = self
            .runtime
            .watchtowers
            .as_ref()
            .context("watchtower controls")?
            .clone();
        ensure!(
            towers.len() >= count,
            "expected at least {count} watchtowers, found {}",
            towers.len()
        );
        for tower in towers.iter().take(count) {
            tower.enabled.send_replace(true);
        }
        self.until("all watchtowers running", |_| {
            Ok(towers
                .iter()
                .take(count)
                .all(|tower| tower.actor.borrow().is_some())
                .then_some(()))
        })
    }

    fn send_direct_routed_payment(
        &mut self,
        payer: usize,
        payee: usize,
        funding: ckb_types::packed::OutPoint,
        invoice: String,
        amount: u128,
        expected_hash: Hash256,
    ) -> Result<()> {
        let reply = self.runtime.request(payer, |reply| {
            FiberActorCommand::SendPaymentWithRouter(
                SendPaymentWithRouterCommand {
                    invoice: Some(invoice),
                    router: vec![RouterHop {
                        target: self.runtime.peers[payee],
                        channel_outpoint: funding,
                        amount_received: amount,
                        incoming_tlc_expiry: DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                    }],
                    ..Default::default()
                },
                reply,
            )
        })?;
        ensure!(
            self.reply("submit direct routed payment", reply)?
                .payment_hash
                == expected_hash
        );
        Ok(())
    }

    fn notify_channel_event(
        &self,
        node: usize,
        channel_id: Hash256,
        event: ChannelEvent,
    ) -> Result<()> {
        let slots = self.runtime.nodes.borrow();
        let actor = &slots[node]
            .node
            .as_ref()
            .context("node offline for channel event")?
            .actor;
        send(
            actor,
            NetworkActorMessage::new_command(FiberActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::NotifyEvent(event),
                },
            )),
        )
    }

    fn ensure_tenant_active(&mut self, tenant_id: &TenantId) -> Result<()> {
        let actor = self.runtime.lsp_actor()?;
        let (tx, mut rx) = oneshot::channel();
        actor
            .send_message(LspServiceMessage::EnsureTenant(
                tenant_id.clone(),
                tx.into(),
            ))
            .map_err(|err| anyhow!("send EnsureTenant: {err}"))?;
        let res = self.until("LSP EnsureTenant reply", |_| receive(&mut rx))?;
        res.map(|_| ()).map_err(|err| anyhow!(err))
    }

    pub(super) fn tenant_channel_state(
        &self,
        tenant_id: &TenantId,
        channel_id: Hash256,
    ) -> Result<Option<ChannelActorState>> {
        let lsp_node = self.lsp_node()?;
        let store = self.runtime.store(lsp_node)?;
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        Ok(tenant_store.get_channel_actor_state(&channel_id))
    }

    fn lsp_deliveries_final(&self, lsp_node: usize, hashes: &[Hash256]) -> Result<bool> {
        let store = self.runtime.store(lsp_node)?;
        let lsp_store = store.namespaced(NodeNamespace::lsp_metadata());
        for payment_hash in hashes {
            let deliveries = lsp_store
                .list_lsp_payment_deliveries_by_payment_hash(payment_hash)
                .map_err(|err| anyhow!("list LSP deliveries for {payment_hash}: {err}"))?;
            if deliveries.is_empty() || deliveries.iter().any(|d| !d.status.is_final()) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
