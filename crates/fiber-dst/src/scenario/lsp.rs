use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use fiber_json_types::{
    CkbInvoiceStatus as JsonInvoiceStatus, GetInvoiceResult, GetPaymentCommandResult,
    InvoiceResult, PaymentStatus as JsonPaymentStatus,
};
use fnn::fiber::gossip::GossipMessageStore;
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{ChannelState, FiberActorCommand, Hash256, PaymentStatus};
use fnn::fiber_types::{CloseFlags, HashAlgorithm};
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use fnn::lsp::{
    HostedTenantStatus, LspPaymentDelivery, LspPaymentDeliveryStatus, LspServiceMessage, TenantId,
};
use fnn::simulation;
use fnn::store::NodeNamespace;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::sync::oneshot;

use crate::host::NAMES;
use crate::lsp::{LspRpcFault, WalletCommand, WALLET_HOSTS};

use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const LSP_NODE: usize = 1;
const PUBLIC_PAYER_A: usize = 0;
const PUBLIC_PAYER_C: usize = 2;
const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;
const LSP_BUFFER_DURATION_MS: u64 = 5_000;

impl Driver {
    pub(super) fn lsp_offline_delivery(&mut self) -> Result<()> {
        self.start_lsp_watchtower()?;
        let _public_edge = self.open_between(
            PUBLIC_PAYER_A,
            LSP_NODE,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        self.wait_trampoline_ready(PUBLIC_PAYER_A, LSP_NODE)?;
        self.wait_wallet_online(0)?;

        // Duplicate lsp_register_tenant: copy 1 succeeds, copy 2 is rejected as replayed request_id.
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DuplicateRequest {
                method: Some("lsp_register_tenant"),
                copies: 2,
                once: true,
            }));
        let tenant_id = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        ensure!(
            self.wallet_rpc(0).duplicates_sent() >= 1,
            "duplicate lsp_register_tenant was not exercised"
        );
        let channel_id = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_id, channel_id)?;
        self.runtime.note(format_args!(
            "LSP tenant {tenant_id} channel {channel_id} online"
        ));

        // Phase 1: Abrupt wallet crash + tenant eviction -> public payment buffered (Deferred)
        // -> LSP crashes & restarts -> wallet returns -> SDK signs -> duplicate settle_invoice -> Success!
        let pre1 = Hash256::from([0x31; 32]);
        let hash1: Hash256 = HashAlgorithm::CkbHash.hash(pre1).into();
        let inv1 = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            channel_id,
            amount: 1_000 * 100_000_000,
            payment_hash: hash1,
            preimage: pre1,
            reply,
        })?;
        self.runtime.crash_wallet(0);
        let evicted =
            self.lsp_call(|reply| LspServiceMessage::EvictTenant(tenant_id.clone(), reply))?;
        ensure!(
            evicted.runtime_status == fnn::lsp::TenantRuntimeStatus::Cold,
            "tenant did not become cold after eviction"
        );
        self.runtime
            .note(format_args!("LSP evicted cold tenant {tenant_id}"));

        let sent_hash1 = self.pay(
            PUBLIC_PAYER_A,
            SendPaymentCommand {
                invoice: Some(inv1.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_hash1 == hash1);
        self.until(
            "LSP buffers payment as Deferred while tenant is offline",
            |s| {
                Ok(s.lsp_delivery_now(hash1)
                    .filter(|d| matches!(d.status, LspPaymentDeliveryStatus::Deferred)))
            },
        )?;
        self.runtime
            .note(format_args!("LSP buffered payment {hash1} as Deferred"));

        // Crash and restart LSP while delivery is Deferred; verify durability and consumed request_id!
        self.runtime.crash(LSP_NODE);
        for _ in 0..4 {
            self.step()?;
        }
        self.runtime.restart(LSP_NODE);
        self.wait_lsp_ready()?;
        self.start_lsp_watchtower()?;
        self.until("Public A reconnects to restarted LSP", |s| {
            Ok(s.runtime.link(PUBLIC_PAYER_A, LSP_NODE).then_some(()))
        })?;
        let after_restart = self
            .lsp_delivery_now(hash1)
            .context("Deferred delivery lost across LSP restart")?;
        ensure!(
            matches!(after_restart.status, LspPaymentDeliveryStatus::Deferred),
            "unexpected status after LSP restart: {:?}",
            after_restart.status
        );
        self.runtime.note(format_args!(
            "LSP preserved Deferred delivery {hash1} across crash/restart"
        ));

        // Restart external SDK wallet from disk; verify replayed registration is STILL rejected post-restart.
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;
        let replay_err =
            self.wallet_call(0, |reply| WalletCommand::ReplayLastRegistration { reply })?;
        ensure!(
            replay_err != "unexpected_ok",
            "replayed registration request_id was unexpectedly accepted after LSP restart"
        );
        self.runtime.note(format_args!(
            "LSP rejected replayed registration after restart: {replay_err}"
        ));

        // Wake tenant so LSP dispatches buffered payment and SDK signs the private channel update.
        let _ = self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_id.clone(), reply))?;
        self.until(
            "wallet receives buffered hold invoice after returning online",
            |s| {
                let status = s
                    .wallet_invoice_status_from_store(&tenant_id, hash1)
                    .unwrap_or(None);
                Ok((status == Some(CkbInvoiceStatus::Received)).then_some(()))
            },
        )?;

        // Duplicate settle_invoice RPC -> verifies idempotent preimage settlement.
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DuplicateRequest {
                method: Some("settle_invoice"),
                copies: 2,
                once: true,
            }));
        self.wallet_call(0, |reply| WalletCommand::SettleInvoice {
            payment_hash: hash1,
            preimage: pre1,
            reply,
        })?;
        self.until(
            "buffered payment succeeds on both public payer and LSP",
            |s| {
                let payer_ok =
                    s.runtime.payment_status(PUBLIC_PAYER_A, hash1) == Some(PaymentStatus::Success);
                let delivery_ok = s
                    .lsp_delivery_now(hash1)
                    .is_some_and(|d| matches!(d.status, LspPaymentDeliveryStatus::Succeeded));
                let invoice_ok = s
                    .wallet_invoice_status_from_store(&tenant_id, hash1)
                    .unwrap_or(None)
                    == Some(CkbInvoiceStatus::Paid);
                Ok((payer_ok && delivery_ok && invoice_ok).then_some(()))
            },
        )?;
        self.runtime.note(format_args!(
            "LSP offline buffered payment {hash1} succeeded after restart"
        ));

        // Phase 2: Long offline past buffer_deadline -> delivery expires cleanly without stranding!
        let pre2 = Hash256::from([0x32; 32]);
        let hash2: Hash256 = HashAlgorithm::CkbHash.hash(pre2).into();
        let inv2 = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            channel_id,
            amount: 800 * 100_000_000,
            payment_hash: hash2,
            preimage: pre2,
            reply,
        })?;
        self.wait_tenant_idle(&tenant_id)?;
        self.runtime.crash_wallet(0);
        // Evict idle cold tenant so p2 buffers while wallet is long offline.
        self.evict_tenant_when_idle(&tenant_id)?;
        let sent_hash2 = self.pay(
            PUBLIC_PAYER_A,
            SendPaymentCommand {
                invoice: Some(inv2.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_hash2 == hash2);
        let deferred2 = self.until("LSP buffers p2 while wallet is long offline", |s| {
            Ok(s.lsp_delivery_now(hash2)
                .filter(|d| matches!(d.status, LspPaymentDeliveryStatus::Deferred)))
        })?;
        self.advance_clock_past(deferred2.buffer_deadline);
        self.until(
            "long-offline delivery expires and fails upstream payer cleanly",
            |s| {
                let payer_failed =
                    s.runtime.payment_status(PUBLIC_PAYER_A, hash2) == Some(PaymentStatus::Failed);
                let delivery_failed = s
                    .lsp_delivery_now(hash2)
                    .is_some_and(|d| matches!(d.status, LspPaymentDeliveryStatus::Failed { .. }));
                Ok((payer_failed && delivery_failed).then_some(()))
            },
        )?;
        self.runtime.note(format_args!(
            "LSP long-offline buffered payment {hash2} expired cleanly past buffer_deadline"
        ));

        // Bring wallet back online and audit all stores & payment-hash indexes.
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;
        let inv2_state: GetInvoiceResult =
            self.wallet_call(0, |reply| WalletCommand::GetInvoice {
                payment_hash: hash2,
                reply,
            })?;
        ensure!(
            inv2_state.status != JsonInvoiceStatus::Paid,
            "expired long-offline invoice was marked Paid"
        );
        self.verify_lsp_indexes(&[PUBLIC_PAYER_A, LSP_NODE], &[tenant_id])?;
        Ok(())
    }

    pub(super) fn lsp_signer_faults(&mut self) -> Result<()> {
        self.start_lsp_watchtower()?;
        let _public_edge = self.open_between(
            PUBLIC_PAYER_A,
            LSP_NODE,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        self.wait_trampoline_ready(PUBLIC_PAYER_A, LSP_NODE)?;
        self.wait_wallet_online(0)?;

        let tenant_id = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        let channel_id = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_id, channel_id)?;

        // Phase 1: Abrupt wallet crash mid-AwaitingSignature + dropped RPC response after commit
        // + duplicate submit_channel_signature + conflicting signature rejection.
        let pre1 = Hash256::from([0x41; 32]);
        let hash1: Hash256 = HashAlgorithm::CkbHash.hash(pre1).into();
        let inv1 = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            channel_id,
            amount: 900 * 100_000_000,
            payment_hash: hash1,
            preimage: pre1,
            reply,
        })?;
        // Pause external wallet signing so the tenant channel pauses in AwaitingSignature.
        self.runtime.wallets.borrow()[0].signing_paused.set(true);
        let sent_hash1 = self.pay(
            PUBLIC_PAYER_A,
            SendPaymentCommand {
                invoice: Some(inv1.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_hash1 == hash1);
        self.until("tenant channel pauses in AwaitingSignature", |s| {
            Ok(s.tenant_channel_awaiting_signature(&tenant_id, channel_id)
                .then_some(()))
        })?;
        self.runtime.note(format_args!(
            "SDK-0 abrupt crash while channel {channel_id} is AwaitingSignature"
        ));
        self.runtime.crash_wallet(0);
        for _ in 0..5 {
            self.step()?;
        }
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;

        // Drop the RPC response AFTER LSP commits submit_channel_signature, then duplicate the next one!
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DropResponseAfterCommit {
                method: Some("submit_channel_signature"),
                once: true,
            }));
        self.runtime.wallets.borrow()[0].signing_paused.set(false);
        self.until(
            "submit_channel_signature response dropped after commit",
            |s| Ok((s.wallet_rpc(0).responses_dropped() >= 1).then_some(())),
        )?;
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DuplicateRequest {
                method: Some("submit_channel_signature"),
                copies: 2,
                once: true,
            }));
        self.until(
            "payment p1 reaches Received despite crash, dropped response, and duplicate sig",
            |s| {
                Ok((s
                    .wallet_invoice_status_from_store(&tenant_id, hash1)
                    .unwrap_or(None)
                    == Some(CkbInvoiceStatus::Received))
                .then_some(()))
            },
        )?;
        ensure!(
            self.wallet_rpc(0).duplicates_sent() >= 1,
            "duplicate submit_channel_signature did not fire"
        );

        // Explicitly replay the last channel signature both identically (AlreadyApplied) and with
        // a corrupted byte (rejected).
        let exact_replay =
            self.wallet_call(0, |reply| WalletCommand::ReplayLastChannelSignature {
                conflicting: false,
                reply,
            })?;
        ensure!(
            exact_replay.contains("AlreadyApplied"),
            "exact channel signature replay returned {exact_replay}"
        );
        let conflict_replay =
            self.wallet_call(0, |reply| WalletCommand::ReplayLastChannelSignature {
                conflicting: true,
                reply,
            })?;
        ensure!(
            !conflict_replay.contains("Applied"),
            "conflicting channel signature was unexpectedly accepted: {conflict_replay}"
        );
        self.runtime.note(format_args!(
            "LSP channel signature replay={exact_replay} conflicting={conflict_replay}"
        ));

        self.wallet_call(0, |reply| WalletCommand::SettleInvoice {
            payment_hash: hash1,
            preimage: pre1,
            reply,
        })?;
        self.until("p1 settles cleanly after signer faults", |s| {
            Ok(
                (s.runtime.payment_status(PUBLIC_PAYER_A, hash1) == Some(PaymentStatus::Success)
                    && s.wallet_invoice_status_from_store(&tenant_id, hash1)
                        .unwrap_or(None)
                        == Some(CkbInvoiceStatus::Paid))
                .then_some(()),
            )
        })?;

        // Phase 2: Long offline with committed TLC -> LSP force-closes private channel
        // -> wallet returns online -> self-funded external watchtower settlement with duplicate RPC!
        let pre2 = Hash256::from([0x42; 32]);
        let hash2: Hash256 = HashAlgorithm::CkbHash.hash(pre2).into();
        let inv2 = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            channel_id,
            amount: 1_000 * 100_000_000,
            payment_hash: hash2,
            preimage: pre2,
            reply,
        })?;
        let sent_hash2 = self.pay(
            PUBLIC_PAYER_A,
            SendPaymentCommand {
                invoice: Some(inv2.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_hash2 == hash2);
        self.until("p2 committed on hosted tenant channel", |s| {
            Ok((s
                .wallet_invoice_status_from_store(&tenant_id, hash2)
                .unwrap_or(None)
                == Some(CkbInvoiceStatus::Received))
            .then_some(()))
        })?;

        // Wallet goes long offline; LSP force-closes the private channel.
        self.runtime.crash_wallet(0);
        let shutdown_reply = self.runtime.request(LSP_NODE, |reply| {
            FiberActorCommand::ControlFiberChannel(fnn::fiber::channel::ChannelCommandWithId {
                channel_id,
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
        self.reply("LSP force close hosted tenant channel", shutdown_reply)?;
        let commitment = self.until("LSP force close confirmed on chain", |s| {
            Ok(s.runtime.channel(LSP_NODE, &channel_id).and_then(|state| {
                matches!(state.state, ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL))
                    .then(|| state.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
            }))
        })?;
        self.runtime.note(format_args!(
            "LSP force-closed private channel {channel_id} commitment={commitment} while SDK-0 offline"
        ));

        // Advance virtual clock by ~1s (past 1-epoch commitment delay = 50ms in DST) while keeping
        // p2's TLC UNEXPIRED: LSP does not hold pre2 so it cannot claim p2; when SDK-0 returns
        // online and registers pre2 via tenant watchtower RPC `create_preimage`, the tenant
        // watchtower emits ExternalFundingRequired and SDK-0 self-funds & signs the on-chain
        // settlement (with duplicate submit_watchtower_signature injected).
        self.advance_clock_past(self.clock_ms());

        // Wallet returns online after long offline; registers pre2 via tenant watchtower RPC
        // and exercises duplicate submit_watchtower_signature!
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;
        self.wallet_call(0, |reply| WalletCommand::CreateWatchPreimage {
            payment_hash: hash2,
            preimage: pre2,
            reply,
        })?;
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DuplicateRequest {
                method: Some("submit_watchtower_signature"),
                copies: 2,
                once: true,
            }));
        self.until(
            "external SDK wallet self-funds and settles p2 TLC on-chain via watchtower",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([LSP_NODE])?;
                }
                let settled = s.runtime.chain.commitment_settled(commitment);
                let payer_ok =
                    s.runtime.payment_status(PUBLIC_PAYER_A, hash2) == Some(PaymentStatus::Success);
                let signed = s.runtime.wallets.borrow()[0].watchtower_sigs_applied >= 1;
                Ok((settled && payer_ok && signed).then_some(()))
            },
        )?;
        ensure!(
            self.wallet_rpc(0).duplicates_sent() >= 2,
            "duplicate submit_watchtower_signature did not fire"
        );
        self.runtime.note(format_args!(
            "SDK-0 self-funded external watchtower settled commitment={commitment} watchtower_sigs={}",
            self.runtime.wallets.borrow()[0].watchtower_sigs_applied
        ));

        self.verify_lsp_indexes(&[PUBLIC_PAYER_A, LSP_NODE], &[tenant_id])?;
        Ok(())
    }

    pub(super) fn lsp_random(&mut self, seed: u64, payments: usize, chaos: bool) -> Result<()> {
        self.start_lsp_watchtower()?;
        // Bidirectional public liquidity between A <-> LSP <-> C so payments work in all directions.
        let _edge_ab = self.open_between(
            PUBLIC_PAYER_A,
            LSP_NODE,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        let _edge_ba = self.open_between(
            LSP_NODE,
            PUBLIC_PAYER_A,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        let _edge_bc = self.open_between(
            LSP_NODE,
            PUBLIC_PAYER_C,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        let _edge_cb = self.open_between(
            PUBLIC_PAYER_C,
            LSP_NODE,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        self.wait_trampoline_ready(PUBLIC_PAYER_A, LSP_NODE)?;
        self.wait_trampoline_ready(PUBLIC_PAYER_C, LSP_NODE)?;

        let mut tenant_ids = Vec::new();
        let mut tenant_channels = Vec::new();
        for wallet in 0..2 {
            self.wait_wallet_online(wallet)?;
            let tenant_id = self.wallet_call(wallet, |reply| WalletCommand::Register { reply })?;
            let channel_id = self.wallet_call(wallet, |reply| WalletCommand::OpenChannel {
                funding_amount: TENANT_FUNDING_AMOUNT,
                reply,
            })?;
            self.wait_tenant_channel_online(&tenant_id, channel_id)?;
            tenant_ids.push(tenant_id);
            tenant_channels.push(channel_id);
        }
        self.runtime.note(format_args!(
            "lsp-random bootstrapped 2 public peers and 2 SDK wallets chaos={chaos} payments={payments}"
        ));

        let mut rng = StdRng::seed_from_u64(seed ^ 0x6c73_702d_6473);
        let mut succeeded = 0usize;
        let mut expired_long_offline = 0usize;
        let mut crashes_injected = 0usize;
        let mut duplicates_injected = 0usize;
        let mut responses_dropped = 0usize;

        for seq in 0..payments {
            let amount = (rng.random_range(100..=600) as u128) * 100_000_000;
            let mut pre_bytes = [0u8; 32];
            rng.fill(&mut pre_bytes);
            let preimage = Hash256::from(pre_bytes);
            let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            let kind = rng.random_range(0..4);

            match kind {
                // 0 or 1: Public peer (A or C) -> Hosted SDK wallet (0 or 1)
                0 | 1 => {
                    let payer = if kind == 0 {
                        PUBLIC_PAYER_A
                    } else {
                        PUBLIC_PAYER_C
                    };
                    let wallet = rng.random_range(0..2);
                    let tenant_id = &tenant_ids[wallet];
                    self.ensure_wallet_online(wallet)?;
                    let inv: InvoiceResult =
                        self.wallet_call(wallet, |reply| WalletCommand::CreateHoldInvoice {
                            channel_id: tenant_channels[wallet],
                            amount,
                            payment_hash,
                            preimage,
                            reply,
                        })?;

                    let fault_roll = if chaos { rng.random_range(0..7) } else { 99 };
                    match fault_roll {
                        // Long offline past buffer_deadline!
                        0 => {
                            self.wait_tenant_idle(tenant_id)?;
                            self.runtime.crash_wallet(wallet);
                            self.evict_tenant_when_idle(tenant_id)?;
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                            let deferred =
                                self.until("random long-offline payment buffered", |s| {
                                    Ok(s.lsp_delivery_now(payment_hash).filter(|d| {
                                        matches!(d.status, LspPaymentDeliveryStatus::Deferred)
                                    }))
                                })?;
                            self.advance_clock_past(deferred.buffer_deadline);
                            self.until("random long-offline payment expired", |s| {
                                Ok((s.runtime.payment_status(payer, payment_hash)
                                    == Some(PaymentStatus::Failed))
                                .then_some(()))
                            })?;
                            expired_long_offline += 1;
                            self.runtime.restart_wallet(wallet);
                            self.wait_wallet_online(wallet)?;
                            let _ = self.lsp_call(|reply| {
                                LspServiceMessage::EnsureTenant(tenant_id.clone(), reply)
                            })?;
                            self.wait_tenant_channel_online(tenant_id, tenant_channels[wallet])?;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} {}->{} long-offline expired {payment_hash}",
                                NAMES[payer], WALLET_HOSTS[wallet]
                            ));
                            continue;
                        }
                        // Abrupt wallet crash mid-delivery + restart!
                        1 => {
                            self.runtime.wallets.borrow()[wallet]
                                .signing_paused
                                .set(true);
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                            self.until("random abrupt crash awaits signature", |s| {
                                Ok(s.tenant_channel_awaiting_signature(
                                    tenant_id,
                                    tenant_channels[wallet],
                                )
                                .then_some(()))
                            })?;
                            self.runtime.crash_wallet(wallet);
                            crashes_injected += 1;
                            for _ in 0..rng.random_range(3..=8) {
                                self.step()?;
                            }
                            self.runtime.restart_wallet(wallet);
                            self.wait_wallet_online(wallet)?;
                            self.runtime.wallets.borrow()[wallet]
                                .signing_paused
                                .set(false);
                        }
                        // Duplicate submit_channel_signature + duplicate settle_invoice!
                        2 => {
                            self.wallet_rpc(wallet).set_fault(Some(
                                LspRpcFault::DuplicateRequest {
                                    method: Some("submit_channel_signature"),
                                    copies: 2,
                                    once: true,
                                },
                            ));
                            duplicates_injected += 1;
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                        }
                        // Drop response after commit on submit_channel_signature!
                        3 => {
                            self.wallet_rpc(wallet).set_fault(Some(
                                LspRpcFault::DropResponseAfterCommit {
                                    method: Some("submit_channel_signature"),
                                    once: true,
                                },
                            ));
                            responses_dropped += 1;
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                        }
                        // Network partition SDK <-> LSP then repair!
                        4 => {
                            self.runtime.partition_wallet(wallet, true)?;
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                            for _ in 0..rng.random_range(6..=14) {
                                self.step()?;
                            }
                            self.runtime.partition_wallet(wallet, false)?;
                        }
                        // Drop one outgoing RPC request so wallet retries on next poll!
                        5 => {
                            self.wallet_rpc(wallet)
                                .set_fault(Some(LspRpcFault::DropRequest {
                                    method: Some("submit_channel_signature"),
                                    once: true,
                                }));
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                        }
                        _ => {
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                        }
                    }

                    self.until("random inbound hold invoice received", |s| {
                        Ok((s
                            .wallet_invoice_status_from_store(tenant_id, payment_hash)
                            .unwrap_or(None)
                            == Some(CkbInvoiceStatus::Received))
                        .then_some(()))
                    })?;
                    if chaos && rng.random_ratio(1, 3) {
                        self.wallet_rpc(wallet)
                            .set_fault(Some(LspRpcFault::DuplicateRequest {
                                method: Some("settle_invoice"),
                                copies: 2,
                                once: true,
                            }));
                        duplicates_injected += 1;
                    }
                    self.wallet_call(wallet, |reply| WalletCommand::SettleInvoice {
                        payment_hash,
                        preimage,
                        reply,
                    })?;
                    self.until("random inbound payment succeeded", |s| {
                        let delivery_ok = s.lsp_delivery_now(payment_hash).is_some_and(|d| {
                            matches!(d.status, LspPaymentDeliveryStatus::Succeeded)
                        });
                        Ok((s.runtime.payment_status(payer, payment_hash)
                            == Some(PaymentStatus::Success)
                            && delivery_ok
                            && s.wallet_invoice_status_from_store(tenant_id, payment_hash)
                                .unwrap_or(None)
                                == Some(CkbInvoiceStatus::Paid))
                        .then_some(()))
                    })?;
                    succeeded += 1;
                    self.runtime.note(format_args!(
                        "lsp-random #{seq} {}->{} succeeded {payment_hash} amount={amount}",
                        NAMES[payer], WALLET_HOSTS[wallet]
                    ));
                }
                // 2: Hosted SDK wallet -> Public peer (A or C)
                2 => {
                    let wallet = rng.random_range(0..2);
                    let payee = if rng.random_bool(0.5) {
                        PUBLIC_PAYER_A
                    } else {
                        PUBLIC_PAYER_C
                    };
                    self.ensure_wallet_online(wallet)?;
                    let payee_key = fnn::fiber::Privkey::from([(payee + 1) as u8; 32]);
                    let invoice = InvoiceBuilder::new(Currency::Fibd)
                        .amount(Some(amount))
                        .payment_hash(payment_hash)
                        .payee_pub_key(self.runtime.peers[payee].into())
                        .allow_trampoline_routing(true)
                        .build_with_sign(|msg| {
                            secp256k1::SECP256K1.sign_ecdsa_recoverable(msg, &payee_key.0)
                        })?;
                    let invoice_str = invoice.to_string();
                    let mut reply = self.runtime.request(payee, |reply| {
                        FiberActorCommand::AddInvoice(invoice, Some(preimage), reply)
                    })?;
                    self.until("public payee invoice recorded", |_| {
                        receive(&mut reply)?.transpose().map_err(Into::into)
                    })?;

                    if chaos && rng.random_ratio(1, 2) {
                        self.wallet_rpc(wallet)
                            .set_fault(Some(LspRpcFault::DuplicateRequest {
                                method: Some("submit_channel_signature"),
                                copies: 2,
                                once: true,
                            }));
                        duplicates_injected += 1;
                    }
                    let _: GetPaymentCommandResult =
                        self.wallet_call(wallet, |reply| WalletCommand::SendPayment {
                            channel_id: tenant_channels[wallet],
                            invoice: invoice_str,
                            amount,
                            payment_hash,
                            use_trampoline: true,
                            reply,
                        })?;
                    self.until("random outbound tenant payment succeeded", |s| {
                        let payee_paid = s.runtime.nodes.borrow()[payee]
                            .node
                            .as_ref()
                            .and_then(|node| node.store.get_invoice_status(&payment_hash))
                            == Some(CkbInvoiceStatus::Paid);
                        Ok(payee_paid.then_some(()))
                    })?;
                    self.until("tenant payment session marked Success", |s| {
                        let payment =
                            s.wallet_payment_status_from_store(&tenant_ids[wallet], payment_hash);
                        Ok((payment == Some(PaymentStatus::Success)).then_some(()))
                    })?;
                    succeeded += 1;
                    self.runtime.note(format_args!(
                        "lsp-random #{seq} {}->{} succeeded {payment_hash} amount={amount}",
                        WALLET_HOSTS[wallet], NAMES[payee]
                    ));
                }
                // 3: Hosted SDK wallet -> other Hosted SDK wallet via LSP!
                _ => {
                    let from_wallet = rng.random_range(0..2);
                    let to_wallet = 1 - from_wallet;
                    self.ensure_wallet_online(from_wallet)?;
                    self.ensure_wallet_online(to_wallet)?;
                    let inv: InvoiceResult =
                        self.wallet_call(to_wallet, |reply| WalletCommand::CreateHoldInvoice {
                            channel_id: tenant_channels[to_wallet],
                            amount,
                            payment_hash,
                            preimage,
                            reply,
                        })?;
                    if chaos && rng.random_ratio(1, 2) {
                        self.wallet_rpc(to_wallet)
                            .set_fault(Some(LspRpcFault::DelayRequest {
                                method: Some("submit_channel_signature"),
                                duration: Duration::from_millis(60),
                                once: true,
                            }));
                    }
                    let _: GetPaymentCommandResult =
                        self.wallet_call(from_wallet, |reply| WalletCommand::SendPayment {
                            channel_id: tenant_channels[from_wallet],
                            invoice: inv.invoice_address,
                            amount,
                            payment_hash,
                            use_trampoline: false,
                            reply,
                        })?;
                    self.until("tenant-to-tenant hold invoice received", |s| {
                        Ok((s
                            .wallet_invoice_status_from_store(&tenant_ids[to_wallet], payment_hash)
                            .unwrap_or(None)
                            == Some(CkbInvoiceStatus::Received))
                        .then_some(()))
                    })?;
                    self.wallet_call(to_wallet, |reply| WalletCommand::SettleInvoice {
                        payment_hash,
                        preimage,
                        reply,
                    })?;
                    self.until("tenant-to-tenant payment succeeded", |s| {
                        let paid = s
                            .wallet_invoice_status_from_store(&tenant_ids[to_wallet], payment_hash)
                            .unwrap_or(None)
                            == Some(CkbInvoiceStatus::Paid);
                        let payer_ok = s.wallet_payment_status_from_store(
                            &tenant_ids[from_wallet],
                            payment_hash,
                        ) == Some(PaymentStatus::Success);
                        Ok((paid && payer_ok).then_some(()))
                    })?;
                    succeeded += 1;
                    self.runtime.note(format_args!(
                        "lsp-random #{seq} {}->{} succeeded {payment_hash} amount={amount}",
                        WALLET_HOSTS[from_wallet], WALLET_HOSTS[to_wallet]
                    ));
                }
            }
        }

        for (wallet, &wallet_name) in WALLET_HOSTS.iter().enumerate().take(2) {
            self.ensure_wallet_online(wallet)?;
            let channels =
                self.wallet_call(wallet, |reply| WalletCommand::ListChannels { reply })?;
            ensure!(
                !channels.channels.is_empty(),
                "{wallet_name} lost its hosted channel"
            );
        }
        self.verify_lsp_indexes(&[PUBLIC_PAYER_A, LSP_NODE, PUBLIC_PAYER_C], &tenant_ids)?;
        self.runtime.note(format_args!(
            "lsp-random reconciled payments={payments} succeeded={succeeded} expired_long_offline={expired_long_offline} crashes={crashes_injected} duplicates={duplicates_injected} responses_dropped={responses_dropped}"
        ));
        Ok(())
    }

    fn start_lsp_watchtower(&mut self) -> Result<()> {
        if let Some(towers) = &self.runtime.watchtowers {
            towers[LSP_NODE].enabled.send_replace(true);
            let tower = towers[LSP_NODE].clone();
            self.until("LSP built-in watchtower started", |_| {
                Ok(tower.actor.borrow().is_some().then_some(()))
            })?;
        }
        Ok(())
    }

    fn wait_lsp_ready(&mut self) -> Result<()> {
        self.until("LSP service online", |s| {
            Ok(s.runtime.lsp_slot.borrow().is_some().then_some(()))
        })
    }

    fn wait_wallet_online(&mut self, wallet: usize) -> Result<()> {
        self.until("external SDK wallet online", |s| {
            Ok(s.runtime.wallets.borrow()[wallet].online.then_some(()))
        })
    }

    fn ensure_wallet_online(&mut self, wallet: usize) -> Result<()> {
        if !self.runtime.wallets.borrow()[wallet].online {
            self.runtime.restart_wallet(wallet);
            self.wait_wallet_online(wallet)?;
        }
        Ok(())
    }

    fn wait_trampoline_ready(&mut self, payer: usize, lsp: usize) -> Result<()> {
        let lsp_pubkey = self.runtime.peers[lsp];
        self.until("public payer learns LSP trampoline feature", |s| {
            let store = s.runtime.store(payer)?;
            let ready = store
                .get_latest_node_announcement(&lsp_pubkey)
                .is_some_and(|node| node.features.supports_trampoline_routing());
            Ok(ready.then_some(()))
        })
    }

    fn wallet_rpc(&self, wallet: usize) -> crate::lsp::LspRpcControl {
        self.runtime.wallets.borrow()[wallet].rpc.clone()
    }

    fn wallet_call<T>(
        &mut self,
        wallet: usize,
        command: impl FnOnce(oneshot::Sender<Result<T>>) -> WalletCommand,
    ) -> Result<T> {
        let mut rx = self.runtime.wallet_request(wallet, command)?;
        self.until("external SDK wallet command", |_| {
            receive(&mut rx)?.transpose()
        })
    }

    fn lsp_call<T>(
        &mut self,
        command: impl FnOnce(ractor::RpcReplyPort<Result<T, String>>) -> LspServiceMessage,
    ) -> Result<T> {
        let actor = self.runtime.lsp_actor()?;
        let (tx, mut rx) = oneshot::channel();
        actor
            .send_message(command(tx.into()))
            .map_err(|err| anyhow!("send LSP service command: {err}"))?;
        self.until("LSP service command", |_| {
            receive(&mut rx)?.transpose().map_err(|err| anyhow!(err))
        })
    }

    fn lsp_delivery_now(&self, payment_hash: Hash256) -> Option<LspPaymentDelivery> {
        let store = self.runtime.store(LSP_NODE).ok()?;
        let lsp_store = store.namespaced(NodeNamespace::lsp_metadata());
        let mut list =
            fnn::lsp::LspPaymentDeliveryStore::list_lsp_payment_deliveries_by_payment_hash(
                &lsp_store,
                &payment_hash,
            )
            .ok()?;
        list.sort_by_key(|d| (!d.status.is_final(), d.updated_at));
        list.pop()
    }

    fn wait_tenant_idle(&mut self, tenant_id: &TenantId) -> Result<()> {
        self.until("hosted tenant channels idle", |s| {
            let store = s.runtime.store(LSP_NODE)?;
            let lsp_store = store.namespaced(NodeNamespace::lsp_metadata());
            let no_pending_deliveries =
                fnn::lsp::LspPaymentDeliveryStore::list_pending_lsp_payment_deliveries(&lsp_store)
                    .map(|list| list.iter().all(|d| d.tenant_id != *tenant_id))
                    .unwrap_or(false);
            let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
            let channels =
                fnn::fiber::channel::ChannelActorStateStore::get_all_channel_states(&tenant_store);
            let idle = channels.iter().all(|c| {
                !c.tlc_state.waiting_ack
                    && !c.any_tlc_pending()
                    && !c.signing_context.is_awaiting_signature()
            });
            Ok((no_pending_deliveries && idle).then_some(()))
        })
    }

    fn evict_tenant_when_idle(&mut self, tenant_id: &TenantId) -> Result<()> {
        self.wait_tenant_idle(tenant_id)?;
        for _ in 0..50 {
            let status =
                self.lsp_call(|reply| LspServiceMessage::EvictTenant(tenant_id.clone(), reply))?;
            if status.runtime_status == fnn::lsp::TenantRuntimeStatus::Cold {
                return Ok(());
            }
            self.step()?;
        }
        anyhow::bail!("timed out evicting hosted tenant {tenant_id}")
    }

    fn wait_tenant_channel_online(
        &mut self,
        tenant_id: &TenantId,
        channel_id: Hash256,
    ) -> Result<()> {
        self.until("hosted tenant channel online on LSP", |s| {
            let lsp_ready = s
                .runtime
                .channel(LSP_NODE, &channel_id)
                .is_some_and(|state| {
                    state.state == ChannelState::ChannelReady
                        && state.connectivity_state == fnn::fiber::ChannelConnectivityState::Online
                });
            let tenant_ready = s.runtime.store(LSP_NODE).ok().is_some_and(|store| {
                let ts = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
                fnn::fiber::channel::ChannelActorStateStore::get_channel_actor_state(
                    &ts,
                    &channel_id,
                )
                .is_some_and(|state| {
                    state.state == ChannelState::ChannelReady
                        && state.connectivity_state == fnn::fiber::ChannelConnectivityState::Online
                })
            });
            Ok((lsp_ready && tenant_ready).then_some(()))
        })
    }

    fn tenant_channel_awaiting_signature(&self, tenant_id: &TenantId, channel_id: Hash256) -> bool {
        let Ok(store) = self.runtime.store(LSP_NODE) else {
            return false;
        };
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        fnn::fiber::channel::ChannelActorStateStore::get_channel_actor_state(
            &tenant_store,
            &channel_id,
        )
        .is_some_and(|state| state.signing_context.is_awaiting_signature())
    }

    fn wallet_invoice_status_from_store(
        &self,
        tenant_id: &TenantId,
        payment_hash: Hash256,
    ) -> Result<Option<CkbInvoiceStatus>> {
        let store = self.runtime.store(LSP_NODE)?;
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        Ok(tenant_store.get_invoice_status(&payment_hash))
    }

    fn wallet_payment_status_from_store(
        &self,
        tenant_id: &TenantId,
        payment_hash: Hash256,
    ) -> Option<PaymentStatus> {
        use fnn::fiber::payment::PaymentSessionExt;
        let store = self.runtime.store(LSP_NODE).ok()?;
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        fnn::fiber::graph::NetworkGraphStateStore::get_payment_session(&tenant_store, payment_hash)
            .map(|session| session.effective_status(&tenant_store))
    }

    fn verify_lsp_indexes(&self, public_nodes: &[usize], tenants: &[TenantId]) -> Result<()> {
        for &node in public_nodes {
            let store = self.runtime.store(node)?;
            simulation::verify_payment_hash_index(&store)
                .map_err(|err| anyhow!("{} payment-hash index: {err}", NAMES[node]))?;
        }
        let lsp_store = self.runtime.store(LSP_NODE)?;
        for tenant in tenants {
            let tenant_store = lsp_store.namespaced(NodeNamespace::hosted_tenant(tenant.as_str()));
            simulation::verify_payment_hash_index(&tenant_store)
                .map_err(|err| anyhow!("tenant {tenant} payment-hash index: {err}"))?;
        }
        Ok(())
    }

    pub(super) fn lsp_config_for_scenario() -> fnn::lsp::LspConfig {
        let mut config = fnn::lsp::LspConfig::default();
        config.max_buffer_duration_ms = LSP_BUFFER_DURATION_MS;
        config
    }
}

#[allow(dead_code)]
fn _assert_types(_: HostedTenantStatus, _: JsonPaymentStatus) {}
