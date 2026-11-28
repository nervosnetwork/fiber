use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use fiber_json_types::{
    CkbInvoiceStatus as JsonInvoiceStatus, GetInvoiceResult, GetPaymentCommandResult,
    InvoiceResult, PaymentStatus as JsonPaymentStatus,
};
use fnn::fiber::gossip::{GossipMessageStore, GossipMessageUpdates};
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::types::BroadcastMessageWithTimestamp;
use fnn::fiber::{
    ChannelState, FiberActorCommand, Hash256, NetworkActorMessage, PaymentStatus,
    PublicNetworkEvent,
};
use fnn::fiber_types::{CloseFlags, HashAlgorithm};
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use fnn::lsp::{
    HostedTenantStatus, LspPaymentDelivery, LspPaymentDeliveryStatus, LspServiceMessage, TenantId,
};
use fnn::simulation;
use fnn::store::NodeNamespace;
use rand::seq::SliceRandom;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::sync::oneshot;

use crate::chain_fault::{ChainEndpoint, ChainFault};
use crate::host::{send, NAMES};
use crate::lsp::{LspRpcFault, SdkSignerFault, WalletCommand, WALLET_HOSTS};
use crate::sqlite_vfs::DiskFault;

use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const FIXTURE_LSP_NODE: usize = 1;
const PUBLIC_PAYER_A: usize = 0;
const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;
const LSP_BUFFER_DURATION_MS: u64 = 5_000;
const WATCHTOWER_HOLD_TTL_MS: u64 = 30 * 60 * 1000;

pub(super) struct RandomLspState {
    pub(super) lsp_node: usize,
    pub(super) tenant_ids: Vec<TenantId>,
    pub(super) tenant_channels: Vec<Hash256>,
    pub(super) secondary_channel_0: Hash256,
}

pub(super) struct RandomLspSummary {
    pub(super) payments: usize,
    pub(super) succeeded: usize,
    pub(super) cancelled_invoices: usize,
    pub(super) expired_long_offline: usize,
    pub(super) crashes_injected: usize,
    pub(super) lsp_crashes_injected: usize,
    pub(super) sdk_faults_injected: usize,
    pub(super) token_refreshes: usize,
    pub(super) closing_sigs_applied: usize,
    pub(super) duplicates_injected: usize,
    pub(super) responses_dropped: usize,
    pub(super) concurrent_payments: usize,
    pub(super) self_funded_watchtower: bool,
}

impl Driver {
    pub(super) fn lsp_offline_delivery(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_lsp_watchtower()?;
        let _public_edge = self.open_between(
            PUBLIC_PAYER_A,
            lsp_node,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        self.wait_trampoline_ready(PUBLIC_PAYER_A, lsp_node)?;
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
        self.runtime.crash(lsp_node);
        for _ in 0..4 {
            self.step()?;
        }
        self.runtime.restart(lsp_node);
        self.wait_lsp_ready()?;
        self.start_lsp_watchtower()?;
        self.until("Public A reconnects to restarted LSP", |s| {
            Ok(s.runtime.link(PUBLIC_PAYER_A, lsp_node).then_some(()))
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
        self.verify_lsp_indexes(&[PUBLIC_PAYER_A, lsp_node], &[tenant_id])?;
        Ok(())
    }

    pub(super) fn lsp_signer_faults(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_lsp_watchtower()?;
        let _public_edge = self.open_between(
            PUBLIC_PAYER_A,
            lsp_node,
            FUNDING_AMOUNT,
            0,
            true,
            OpeningFault::None,
        )?;
        self.wait_trampoline_ready(PUBLIC_PAYER_A, lsp_node)?;
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
        let shutdown_reply = self.runtime.request(lsp_node, |reply| {
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
            Ok(s.runtime.channel(lsp_node, &channel_id).and_then(|state| {
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
                    s.scan_onchain([lsp_node])?;
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

        self.verify_lsp_indexes(&[PUBLIC_PAYER_A, lsp_node], &[tenant_id])?;
        Ok(())
    }

    pub(super) fn bootstrap_random_lsp(
        &mut self,
        public_nodes: usize,
        chaos: bool,
    ) -> Result<RandomLspState> {
        let lsp_node = self.lsp_node()?;
        self.wait_lsp_ready()?;
        self.sync_lsp_trampoline_gossip(public_nodes)?;

        let mut tenant_ids = Vec::new();
        let mut tenant_channels = Vec::new();
        for wallet in 0..3 {
            self.wait_wallet_online(wallet)?;
            if wallet == 0 {
                self.wallet_rpc(wallet)
                    .set_fault(Some(LspRpcFault::DuplicateRequest {
                        method: Some("lsp_register_tenant"),
                        copies: 2,
                        once: true,
                    }));
            }
            let tenant_id = self.wallet_call(wallet, |reply| WalletCommand::Register { reply })?;
            tenant_ids.push(tenant_id);
        }

        // Open SDK-0's primary channel (with external-funding RPC drop + duplicate submit under chaos)
        // AND a secondary channel on SDK-0 to exercise multi-channel tenant isolation & cooperative close.
        if chaos {
            self.wallet_rpc(0)
                .set_fault(Some(LspRpcFault::DropResponseAfterCommit {
                    method: Some("open_channel_with_external_funding"),
                    once: true,
                }));
        }
        let ch0_primary = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_ids[0], ch0_primary)?;

        if chaos {
            self.wallet_rpc(0)
                .set_fault(Some(LspRpcFault::DuplicateRequest {
                    method: Some("submit_signed_funding_tx"),
                    copies: 2,
                    once: true,
                }));
        }
        let secondary_channel_0 = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT / 2,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_ids[0], secondary_channel_0)?;

        // Open SDK-1's hosted channel (fills active tenant slots 2/2).
        let ch1 = self.wallet_call(1, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_ids[1], ch1)?;

        // Verify max_active_tenants=2 rejects starting SDK-2 while SDK-0 and SDK-1 are active,
        // then evict idle SDK-1 so SDK-2 opens its channel cleanly and returns cold.
        let limit_err = self
            .lsp_call_raw(|reply| LspServiceMessage::EnsureTenant(tenant_ids[2].clone(), reply))?;
        ensure!(
            limit_err.is_err(),
            "max_active_tenants=2 unexpectedly allowed a 3rd active tenant"
        );
        self.wait_tenant_idle(&tenant_ids[1])?;
        self.evict_tenant_when_idle(&tenant_ids[1])?;
        let _ =
            self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_ids[2].clone(), reply))?;
        let ch2 = self.wallet_call(2, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_ids[2], ch2)?;
        self.wait_tenant_idle(&tenant_ids[2])?;
        self.evict_tenant_when_idle(&tenant_ids[2])?;
        let _ =
            self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_ids[1].clone(), reply))?;
        self.wait_tenant_channel_online(&tenant_ids[1], ch1)?;

        tenant_channels.push(ch0_primary);
        tenant_channels.push(ch1);
        tenant_channels.push(ch2);

        self.runtime.note(format_args!(
            "random LSP {} bootstrapped {} public peers and 3 SDK wallets (max_active_tenants=2, SDK-0 dual-channel)",
            NAMES[lsp_node], public_nodes
        ));
        Ok(RandomLspState {
            lsp_node,
            tenant_ids,
            tenant_channels,
            secondary_channel_0,
        })
    }

    pub(super) fn run_random_lsp_workload(
        &mut self,
        seed: u64,
        state: &RandomLspState,
        public_nodes: usize,
        payments: usize,
        chaos: bool,
    ) -> Result<RandomLspSummary> {
        let lsp_node = state.lsp_node;
        let tenant_ids = &state.tenant_ids;
        let tenant_channels = &state.tenant_channels;

        self.wait_lsp_ready()?;
        self.sync_lsp_trampoline_gossip(public_nodes)?;
        for wallet in 0..2 {
            self.ensure_wallet_online(wallet)?;
            let _ = self.lsp_call(|reply| {
                LspServiceMessage::EnsureTenant(tenant_ids[wallet].clone(), reply)
            })?;
            self.wait_tenant_channel_online(&tenant_ids[wallet], tenant_channels[wallet])?;
        }

        let public_peers: Vec<usize> = (0..public_nodes).filter(|&n| n != lsp_node).collect();
        let mut rng = StdRng::seed_from_u64(seed ^ 0x6c73_702d_6473);
        let mut succeeded = 0usize;
        let mut cancelled_invoices = 0usize;
        let mut expired_long_offline = 0usize;
        let mut crashes_injected = 0usize;
        let mut lsp_crashes_injected = 0usize;
        let mut sdk_faults_injected = 0usize;
        let mut duplicates_injected = 0usize;
        let mut responses_dropped = 0usize;

        for seq in 0..payments {
            // Keep amounts modest (10..60 CKB) so multi-hop public channels retain liquidity
            // across the combined public + LSP workload.
            let amount = (rng.random_range(100..=600) as u128) * 10_000_000;
            let mut pre_bytes = [0u8; 32];
            rng.fill(&mut pre_bytes);
            let preimage = Hash256::from(pre_bytes);
            let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            let kind = rng.random_range(0..4);

            match kind {
                // 0 or 1: Any reachable public peer (direct or multi-hop!) -> Hosted SDK wallet
                0 | 1 => {
                    let wallet = rng.random_range(0..2);
                    let tenant_id = &tenant_ids[wallet];
                    self.ensure_wallet_online(wallet)?;
                    let inv: InvoiceResult =
                        self.wallet_call(wallet, |reply| WalletCommand::CreateHoldInvoice {
                            amount,
                            payment_hash,
                            preimage,
                            reply,
                        })?;
                    let mut candidates = public_peers.clone();
                    candidates.shuffle(&mut rng);
                    let Some(payer) = self.pick_public_payer(&candidates, &inv.invoice_address)?
                    else {
                        self.run_tenant_to_tenant_payment(
                            seq,
                            1 - wallet,
                            wallet,
                            tenant_ids,
                            tenant_channels,
                            amount,
                            payment_hash,
                            preimage,
                            &inv.invoice_address,
                            chaos,
                            &mut rng,
                        )?;
                        succeeded += 1;
                        continue;
                    };

                    let fault_roll = if chaos { rng.random_range(0..13) } else { 99 };
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
                            self.wait_tenant_idle(tenant_id)?;
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
                                Ok(s.tenant_any_channel_awaiting_signature(tenant_id)
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
                        // Duplicate submit_channel_signature!
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
                        // Crash & restart LSP node itself while a Deferred payment is buffered!
                        6 => {
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
                            self.until("random LSP crash buffered payment", |s| {
                                Ok(s.lsp_delivery_now(payment_hash).filter(|d| {
                                    matches!(d.status, LspPaymentDeliveryStatus::Deferred)
                                }))
                            })?;
                            self.runtime.crash(lsp_node);
                            crashes_injected += 1;
                            lsp_crashes_injected += 1;
                            for _ in 0..4 {
                                self.step()?;
                            }
                            self.runtime.restart(lsp_node);
                            self.wait_lsp_ready()?;
                            self.runtime.restart_wallet(wallet);
                            self.wait_wallet_online(wallet)?;
                            let _ = self.lsp_call(|reply| {
                                LspServiceMessage::EnsureTenant(tenant_id.clone(), reply)
                            })?;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} LSP {} restarted with Deferred {payment_hash}",
                                NAMES[lsp_node]
                            ));
                        }
                        // Crash & restart LSP node mid-flight while hosted channel awaits signature!
                        7 => {
                            self.wait_tenant_idle(tenant_id)?;
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
                            self.until("random LSP mid-flight crash awaits signature", |s| {
                                Ok(s.tenant_any_channel_awaiting_signature(tenant_id)
                                    .then_some(()))
                            })?;
                            self.runtime.crash(lsp_node);
                            crashes_injected += 1;
                            lsp_crashes_injected += 1;
                            for _ in 0..4 {
                                self.step()?;
                            }
                            self.runtime.restart(lsp_node);
                            self.wait_lsp_ready()?;
                            let _ = self.lsp_call(|reply| {
                                LspServiceMessage::EnsureTenant(tenant_id.clone(), reply)
                            })?;
                            self.runtime.wallets.borrow()[wallet]
                                .signing_paused
                                .set(false);
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} LSP {} restarted mid-flight {payment_hash}",
                                NAMES[lsp_node]
                            ));
                        }
                        // Cancel hold invoice (either while Received or while Deferred!) -> fails upstream cleanly.
                        8 => {
                            let cancel_while_deferred = rng.random_bool(0.5);
                            if cancel_while_deferred {
                                self.wait_tenant_idle(tenant_id)?;
                                self.evict_tenant_when_idle(tenant_id)?;
                                let _ = self.pay(
                                    payer,
                                    SendPaymentCommand {
                                        invoice: Some(inv.invoice_address),
                                        ..Default::default()
                                    },
                                )?;
                                self.until("random cancel-deferred payment buffered", |s| {
                                    Ok(s.lsp_delivery_now(payment_hash).filter(|d| {
                                        matches!(d.status, LspPaymentDeliveryStatus::Deferred)
                                    }))
                                })?;
                                let _ = self.lsp_call(|reply| {
                                    LspServiceMessage::EnsureTenant(tenant_id.clone(), reply)
                                })?;
                            } else {
                                let _ = self.pay(
                                    payer,
                                    SendPaymentCommand {
                                        invoice: Some(inv.invoice_address),
                                        ..Default::default()
                                    },
                                )?;
                                self.until("random cancel-received hold invoice received", |s| {
                                    Ok((s
                                        .wallet_invoice_status_from_store(tenant_id, payment_hash)
                                        .unwrap_or(None)
                                        == Some(CkbInvoiceStatus::Received))
                                    .then_some(()))
                                })?;
                            }
                            let _ =
                                self.wallet_call(wallet, |reply| WalletCommand::CancelInvoice {
                                    payment_hash,
                                    reply,
                                })?;
                            self.until("random cancelled hosted invoice fails upstream", |s| {
                                let payer_failed = s.runtime.payment_status(payer, payment_hash)
                                    == Some(PaymentStatus::Failed);
                                let delivery_failed =
                                    s.lsp_delivery_now(payment_hash).is_some_and(|d| {
                                        matches!(d.status, LspPaymentDeliveryStatus::Failed { .. })
                                    });
                                Ok((payer_failed && delivery_failed).then_some(()))
                            })?;
                            cancelled_invoices += 1;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} {}->{} cancel_invoice (deferred={cancel_while_deferred}) failed cleanly {payment_hash}",
                                NAMES[payer], WALLET_HOSTS[wallet]
                            ));
                            continue;
                        }
                        // SDK pre-persist crash: confirm() mutates memory, skips disk save, crashes!
                        9 => {
                            self.runtime.wallets.borrow()[wallet]
                                .sdk_fault
                                .set(Some(SdkSignerFault::SkipPersistAfterConfirmCrash));
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                            self.until("SDK SkipPersistAfterConfirmCrash triggered", |s| {
                                Ok((!s.runtime.wallets.borrow()[wallet].online).then_some(()))
                            })?;
                            sdk_faults_injected += 1;
                            crashes_injected += 1;
                            self.runtime.restart_wallet(wallet);
                            self.wait_wallet_online(wallet)?;
                        }
                        // SDK post-persist pre-RPC crash: confirm() saves disk & nonces, crashes before RPC!
                        10 => {
                            self.runtime.wallets.borrow()[wallet]
                                .sdk_fault
                                .set(Some(SdkSignerFault::CrashAfterPersistBeforeSubmit));
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                            self.until("SDK CrashAfterPersistBeforeSubmit triggered", |s| {
                                Ok((!s.runtime.wallets.borrow()[wallet].online).then_some(()))
                            })?;
                            sdk_faults_injected += 1;
                            crashes_injected += 1;
                            self.runtime.restart_wallet(wallet);
                            self.wait_wallet_online(wallet)?;
                        }
                        // Live Biscuit token revocation mid-flight -> SDK sees Unauthorized and auto-refreshes!
                        11 => {
                            self.wait_tenant_idle(tenant_id)?;
                            let prev_refreshes =
                                self.runtime.wallets.borrow()[wallet].token_refreshes;
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
                            self.until("random token-revoke awaits signature", |s| {
                                Ok(s.tenant_any_channel_awaiting_signature(tenant_id)
                                    .then_some(()))
                            })?;
                            let new_epoch = self.lsp_call(|reply| {
                                LspServiceMessage::RevokeTenantTokens(tenant_id.clone(), reply)
                            })?;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} revoked tokens for {} -> epoch={new_epoch}",
                                WALLET_HOSTS[wallet]
                            ));
                            self.runtime.wallets.borrow()[wallet]
                                .signing_paused
                                .set(false);
                            self.until("wallet refreshed revoked token", |s| {
                                Ok((s.runtime.wallets.borrow()[wallet].token_refreshes
                                    > prev_refreshes)
                                    .then_some(()))
                            })?;
                        }
                        // LSP SQLite DiskFault mid-delivery + restart recovery!
                        12 => {
                            let disk_fault = if rng.random_bool(0.5) {
                                DiskFault::WriteIo
                            } else {
                                DiskFault::SyncIo
                            };
                            self.runtime.disks[lsp_node].arm(disk_fault);
                            let _ = self.pay(
                                payer,
                                SendPaymentCommand {
                                    invoice: Some(inv.invoice_address),
                                    ..Default::default()
                                },
                            )?;
                            self.until("LSP SQLite disk fault hit", |s| {
                                Ok(s.runtime.disks[lsp_node].hit().map(|_| ()))
                            })?;
                            // Wait a few ticks for any actor panic/stop to propagate, then recover cleanly.
                            for _ in 0..4 {
                                self.step()?;
                            }
                            self.runtime.disks[lsp_node].clear();
                            if !self.runtime.node_available(lsp_node) {
                                self.runtime.crash(lsp_node);
                                self.runtime.restart(lsp_node);
                                lsp_crashes_injected += 1;
                                crashes_injected += 1;
                                self.wait_lsp_ready()?;
                            }
                            let _ = self.lsp_call(|reply| {
                                LspServiceMessage::EnsureTenant(tenant_id.clone(), reply)
                            })?;
                            let outcome = self.until(
                                "post-disk-fault delivery reaches terminal or Received",
                                |s| {
                                    if s.wallet_invoice_status_from_store(tenant_id, payment_hash)
                                        .unwrap_or(None)
                                        == Some(CkbInvoiceStatus::Received)
                                    {
                                        return Ok(Some(true));
                                    }
                                    if s.runtime.payment_status(payer, payment_hash)
                                        == Some(PaymentStatus::Failed)
                                    {
                                        return Ok(Some(false));
                                    }
                                    Ok(None)
                                },
                            )?;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} LSP SQLite {disk_fault:?} recovered received={outcome} {payment_hash}"
                            ));
                            if !outcome {
                                continue;
                            }
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
                // 2: Hosted SDK wallet -> Any reachable public peer (via LSP trampoline + multi-hop!)
                2 => {
                    let wallet = rng.random_range(0..2);
                    self.ensure_wallet_online(wallet)?;
                    let mut candidates = public_peers.clone();
                    candidates.shuffle(&mut rng);
                    let mut sent_to = None;
                    for payee in candidates {
                        if !self.runtime.node_available(payee) {
                            continue;
                        }
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
                            self.wallet_rpc(wallet).set_fault(Some(
                                LspRpcFault::DuplicateRequest {
                                    method: Some("submit_channel_signature"),
                                    copies: 2,
                                    once: true,
                                },
                            ));
                            duplicates_injected += 1;
                        }
                        match self.wallet_call(wallet, |reply| WalletCommand::SendPayment {
                            channel_id: tenant_channels[wallet],
                            invoice: invoice_str,
                            amount,
                            payment_hash,
                            use_trampoline: true,
                            reply,
                        }) {
                            Ok(_) => {
                                sent_to = Some(payee);
                                break;
                            }
                            Err(_) => {
                                self.wallet_rpc(wallet).set_fault(None);
                                continue;
                            }
                        }
                    }
                    if let Some(payee) = sent_to {
                        let outcome =
                            self.until("random outbound tenant payment finalized", |s| {
                                let payee_paid =
                                    s.runtime.nodes.borrow()[payee].node.as_ref().and_then(
                                        |node| node.store.get_invoice_status(&payment_hash),
                                    ) == Some(CkbInvoiceStatus::Paid);
                                let status = s.wallet_payment_status_from_store(
                                    &tenant_ids[wallet],
                                    payment_hash,
                                );
                                if payee_paid && status == Some(PaymentStatus::Success) {
                                    return Ok(Some(PaymentStatus::Success));
                                }
                                if status == Some(PaymentStatus::Failed) {
                                    return Ok(Some(PaymentStatus::Failed));
                                }
                                Ok(None)
                            })?;
                        if outcome == PaymentStatus::Success {
                            succeeded += 1;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} {}->{} succeeded {payment_hash} amount={amount}",
                                WALLET_HOSTS[wallet], NAMES[payee]
                            ));
                        } else {
                            self.wait_tenant_idle(&tenant_ids[wallet])?;
                            self.runtime.note(format_args!(
                                "lsp-random #{seq} {}->{} failed and refunded {payment_hash} amount={amount}",
                                WALLET_HOSTS[wallet], NAMES[payee]
                            ));
                        }
                    } else {
                        let to_wallet = 1 - wallet;
                        self.ensure_wallet_online(to_wallet)?;
                        let inv: InvoiceResult = self.wallet_call(to_wallet, |reply| {
                            WalletCommand::CreateHoldInvoice {
                                amount,
                                payment_hash,
                                preimage,
                                reply,
                            }
                        })?;
                        self.run_tenant_to_tenant_payment(
                            seq,
                            wallet,
                            to_wallet,
                            tenant_ids,
                            tenant_channels,
                            amount,
                            payment_hash,
                            preimage,
                            &inv.invoice_address,
                            chaos,
                            &mut rng,
                        )?;
                        succeeded += 1;
                    }
                }
                // 3: Hosted SDK wallet -> other Hosted SDK wallet via LSP!
                _ => {
                    let from_wallet = rng.random_range(0..2);
                    let to_wallet = 1 - from_wallet;
                    self.ensure_wallet_online(from_wallet)?;
                    self.ensure_wallet_online(to_wallet)?;
                    let inv: InvoiceResult =
                        self.wallet_call(to_wallet, |reply| WalletCommand::CreateHoldInvoice {
                            amount,
                            payment_hash,
                            preimage,
                            reply,
                        })?;
                    self.run_tenant_to_tenant_payment(
                        seq,
                        from_wallet,
                        to_wallet,
                        tenant_ids,
                        tenant_channels,
                        amount,
                        payment_hash,
                        preimage,
                        &inv.invoice_address,
                        chaos,
                        &mut rng,
                    )?;
                    succeeded += 1;
                }
            }
        }

        let mut concurrent_payments = 0usize;
        if chaos {
            let (wave_count, wave_cancelled, wave_crashes, wave_sdk_faults, wave_dups, wave_drops) =
                self.run_random_lsp_concurrent_wave(state, &public_peers, &mut rng)?;
            concurrent_payments = wave_count;
            succeeded += wave_count.saturating_sub(wave_cancelled);
            cancelled_invoices += wave_cancelled;
            crashes_injected += wave_crashes;
            sdk_faults_injected += wave_sdk_faults;
            duplicates_injected += wave_dups;
            responses_dropped += wave_drops;

            self.run_random_lsp_lifecycle_probe(state, &public_peers, &mut rng)?;
            succeeded += 1;
        }

        for (wallet, &wallet_name) in WALLET_HOSTS.iter().enumerate().take(2) {
            self.ensure_wallet_online(wallet)?;
            self.wait_tenant_idle(&tenant_ids[wallet])?;
            let channels =
                self.wallet_call(wallet, |reply| WalletCommand::ListChannels { reply })?;
            ensure!(
                !channels.channels.is_empty(),
                "{wallet_name} lost its hosted channel"
            );
        }
        self.wait_payment_actor(lsp_node)?;
        let all_public: Vec<usize> = (0..public_nodes).collect();
        self.verify_lsp_indexes(&all_public, tenant_ids)?;
        let token_refreshes: usize = self
            .runtime
            .wallets
            .borrow()
            .iter()
            .map(|w| w.token_refreshes as usize)
            .sum();
        let closing_sigs_applied: usize = self
            .runtime
            .wallets
            .borrow()
            .iter()
            .map(|w| w.closing_sigs_applied as usize)
            .sum();
        self.runtime.note(format_args!(
            "lsp-random reconciled payments={payments} succeeded={succeeded} concurrent={concurrent_payments} cancelled={cancelled_invoices} expired_long_offline={expired_long_offline} crashes={crashes_injected} lsp_crashes={lsp_crashes_injected} sdk_faults={sdk_faults_injected} token_refreshes={token_refreshes} closing_sigs={closing_sigs_applied} duplicates={duplicates_injected} responses_dropped={responses_dropped}"
        ));
        Ok(RandomLspSummary {
            payments,
            succeeded,
            cancelled_invoices,
            expired_long_offline,
            crashes_injected,
            lsp_crashes_injected,
            sdk_faults_injected,
            token_refreshes,
            closing_sigs_applied,
            duplicates_injected,
            responses_dropped,
            concurrent_payments,
            self_funded_watchtower: false,
        })
    }

    #[allow(clippy::type_complexity)]
    fn run_random_lsp_concurrent_wave(
        &mut self,
        state: &RandomLspState,
        public_peers: &[usize],
        rng: &mut StdRng,
    ) -> Result<(usize, usize, usize, usize, usize, usize)> {
        let lsp_node = state.lsp_node;
        let tenant_ids = &state.tenant_ids;
        let tenant_channels = &state.tenant_channels;
        self.wait_lsp_ready()?;
        for wallet in 0..2 {
            self.ensure_wallet_online(wallet)?;
            let _ = self.lsp_call(|reply| {
                LspServiceMessage::EnsureTenant(tenant_ids[wallet].clone(), reply)
            })?;
            self.wait_tenant_channel_online(&tenant_ids[wallet], tenant_channels[wallet])?;
        }
        let _ = lsp_node;

        // Submit 6 concurrent payments across SDK-0 (dual-channel) & SDK-1 before settling any:
        // - 2x Public -> SDK-0 (on primary + secondary channels)
        // - 2x Public -> SDK-1 (one settled, one cancelled via cancel_invoice)
        // - 1x SDK-0 -> SDK-1 (concurrent tenant-to-tenant)
        // - 1x SDK-0 -> Public peer (concurrent outbound trampoline)
        let mut inflight: Vec<(Option<usize>, usize, Hash256, Hash256, bool)> = Vec::new();
        let mut crashes = 0usize;
        let mut sdk_faults = 0usize;
        let mut duplicates = 0usize;
        let mut drops = 0usize;
        let mut cancelled = 0usize;

        let targets = [
            (0usize, false),
            (0usize, false),
            (1usize, false),
            (1usize, true), // cancel_invoice while held!
        ];

        for (idx, &(wallet, will_cancel)) in targets.iter().enumerate() {
            let amount = (rng.random_range(120..=260) as u128) * 10_000_000;
            let mut pre_bytes = [0u8; 32];
            rng.fill(&mut pre_bytes);
            let preimage = Hash256::from(pre_bytes);
            let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            let inv: InvoiceResult =
                self.wallet_call(wallet, |reply| WalletCommand::CreateHoldInvoice {
                    amount,
                    payment_hash,
                    preimage,
                    reply,
                })?;
            let mut candidates = public_peers.to_vec();
            candidates.shuffle(rng);
            if let Some(payer) = self.pick_public_payer(&candidates, &inv.invoice_address)? {
                if idx == 0 {
                    self.wallet_rpc(wallet)
                        .set_fault(Some(LspRpcFault::DuplicateRequest {
                            method: Some("submit_channel_signature"),
                            copies: 2,
                            once: true,
                        }));
                    duplicates += 1;
                } else if idx == 2 {
                    self.wallet_rpc(wallet)
                        .set_fault(Some(LspRpcFault::DropResponseAfterCommit {
                            method: Some("submit_channel_signature"),
                            once: true,
                        }));
                    drops += 1;
                }
                let _ = self.pay(
                    payer,
                    SendPaymentCommand {
                        invoice: Some(inv.invoice_address),
                        ..Default::default()
                    },
                )?;
                inflight.push((Some(payer), wallet, payment_hash, preimage, will_cancel));
            }
        }

        // Concurrent tenant-to-tenant payment while inbound public payments are in flight.
        let t2t_amount = (rng.random_range(120..=260) as u128) * 10_000_000;
        let mut pre_bytes = [0u8; 32];
        rng.fill(&mut pre_bytes);
        let t2t_preimage = Hash256::from(pre_bytes);
        let t2t_hash: Hash256 = HashAlgorithm::CkbHash.hash(t2t_preimage).into();
        let t2t_inv: InvoiceResult =
            self.wallet_call(1, |reply| WalletCommand::CreateHoldInvoice {
                amount: t2t_amount,
                payment_hash: t2t_hash,
                preimage: t2t_preimage,
                reply,
            })?;
        let _: GetPaymentCommandResult =
            self.wallet_call(0, |reply| WalletCommand::SendPayment {
                channel_id: tenant_channels[0],
                invoice: t2t_inv.invoice_address,
                amount: t2t_amount,
                payment_hash: t2t_hash,
                use_trampoline: false,
                reply,
            })?;
        inflight.push((None, 1, t2t_hash, t2t_preimage, false));

        // Arm an SDK post-persist pre-RPC crash on SDK-0 while concurrent payments are signing!
        self.runtime.wallets.borrow()[0]
            .sdk_fault
            .set(Some(SdkSignerFault::CrashAfterPersistBeforeSubmit));
        for _ in 0..20 {
            self.step()?;
            if !self.runtime.wallets.borrow()[0].online {
                break;
            }
        }
        if !self.runtime.wallets.borrow()[0].online {
            sdk_faults += 1;
            crashes += 1;
        } else {
            self.runtime.wallets.borrow()[0].sdk_fault.set(None);
            self.runtime.crash_wallet(0);
            crashes += 1;
        }
        for _ in 0..4 {
            self.step()?;
        }
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;

        // Wait until all concurrent payments are held simultaneously before settling/cancelling.
        self.until("all concurrent LSP/SDK hold invoices received", |s| {
            let all_received = inflight.iter().all(|&(_, wallet, hash, _, _)| {
                s.wallet_invoice_status_from_store(&tenant_ids[wallet], hash)
                    .unwrap_or(None)
                    == Some(CkbInvoiceStatus::Received)
            });
            Ok(all_received.then_some(()))
        })?;

        // Busy tenant with held TLCs / active deliveries MUST reject eviction!
        let busy_evict = self
            .lsp_call_raw(|reply| LspServiceMessage::EvictTenant(tenant_ids[1].clone(), reply))?;
        ensure!(
            busy_evict.is_err(),
            "EvictTenant unexpectedly evicted busy tenant SDK-1 with held concurrent payments"
        );

        for &(_, wallet, hash, preimage, will_cancel) in &inflight {
            if will_cancel {
                let _ = self.wallet_call(wallet, |reply| WalletCommand::CancelInvoice {
                    payment_hash: hash,
                    reply,
                })?;
                cancelled += 1;
            } else {
                self.wallet_call(wallet, |reply| WalletCommand::SettleInvoice {
                    payment_hash: hash,
                    preimage,
                    reply,
                })?;
            }
        }
        self.until("all concurrent LSP/SDK payments finalized", |s| {
            let all_done = inflight
                .iter()
                .all(|&(payer, wallet, hash, _, will_cancel)| {
                    if will_cancel {
                        let payer_failed = payer.is_some_and(|p| {
                            s.runtime.payment_status(p, hash) == Some(PaymentStatus::Failed)
                        });
                        let delivery_failed = s.lsp_delivery_now(hash).is_some_and(|d| {
                            matches!(d.status, LspPaymentDeliveryStatus::Failed { .. })
                        });
                        return payer_failed && delivery_failed;
                    }
                    let paid = s
                        .wallet_invoice_status_from_store(&tenant_ids[wallet], hash)
                        .unwrap_or(None)
                        == Some(CkbInvoiceStatus::Paid);
                    let payer_ok = match payer {
                        Some(public_payer) => {
                            s.runtime.payment_status(public_payer, hash)
                                == Some(PaymentStatus::Success)
                                && s.lsp_delivery_now(hash).is_some_and(|d| {
                                    matches!(d.status, LspPaymentDeliveryStatus::Succeeded)
                                })
                        }
                        None => {
                            s.wallet_payment_status_from_store(&tenant_ids[0], hash)
                                == Some(PaymentStatus::Success)
                        }
                    };
                    paid && payer_ok
                });
            Ok(all_done.then_some(()))
        })?;

        let count = inflight.len();
        self.runtime.note(format_args!(
            "lsp-random concurrent wave settled count={count} cancelled={cancelled} crashes={crashes} sdk_faults={sdk_faults} duplicates={duplicates} responses_dropped={drops}"
        ));
        Ok((count, cancelled, crashes, sdk_faults, duplicates, drops))
    }

    fn run_random_lsp_lifecycle_probe(
        &mut self,
        state: &RandomLspState,
        public_peers: &[usize],
        rng: &mut StdRng,
    ) -> Result<()> {
        let lsp_node = state.lsp_node;
        let tenant_ids = &state.tenant_ids;
        let tenant_channels = &state.tenant_channels;

        // Part A: Cold-tenant eviction churn (max_active_tenants=2 with 3 tenants).
        // Buffer a payment to cold SDK-2 while SDK-0 & SDK-1 occupy both active slots,
        // confirm capacity enforcement, evict idle SDK-1, wake SDK-2, and settle!
        self.ensure_wallet_online(2)?;
        self.wait_tenant_idle(&tenant_ids[1])?;
        let amount = 250 * 100_000_000u128;
        let mut pre_bytes = [0u8; 32];
        rng.fill(&mut pre_bytes);
        let preimage = Hash256::from(pre_bytes);
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();

        // Briefly swap SDK-1 -> SDK-2 to create the invoice, then return SDK-2 cold & SDK-1 hot.
        self.evict_tenant_when_idle(&tenant_ids[1])?;
        let _ =
            self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_ids[2].clone(), reply))?;
        let inv2: InvoiceResult =
            self.wallet_call(2, |reply| WalletCommand::CreateHoldInvoice {
                amount,
                payment_hash,
                preimage,
                reply,
            })?;
        self.wait_tenant_idle(&tenant_ids[2])?;
        self.evict_tenant_when_idle(&tenant_ids[2])?;
        let _ =
            self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_ids[1].clone(), reply))?;

        let mut candidates = public_peers.to_vec();
        candidates.shuffle(rng);
        if let Some(payer) = self.pick_public_payer(&candidates, &inv2.invoice_address)? {
            let _ = self.pay(
                payer,
                SendPaymentCommand {
                    invoice: Some(inv2.invoice_address),
                    ..Default::default()
                },
            )?;
            self.until("cold SDK-2 payment buffered as Deferred", |s| {
                Ok(s.lsp_delivery_now(payment_hash)
                    .filter(|d| matches!(d.status, LspPaymentDeliveryStatus::Deferred)))
            })?;
            // Capacity cap must still block EnsureTenant(SDK-2) until an active slot is freed.
            let full_err = self.lsp_call_raw(|reply| {
                LspServiceMessage::EnsureTenant(tenant_ids[2].clone(), reply)
            })?;
            ensure!(
                full_err.is_err(),
                "expected active tenant limit error while SDK-0 and SDK-1 are active"
            );
            self.wait_tenant_idle(&tenant_ids[1])?;
            self.evict_tenant_when_idle(&tenant_ids[1])?;
            let _ = self
                .lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_ids[2].clone(), reply))?;
            self.until("woken SDK-2 receives buffered hold invoice", |s| {
                Ok((s
                    .wallet_invoice_status_from_store(&tenant_ids[2], payment_hash)
                    .unwrap_or(None)
                    == Some(CkbInvoiceStatus::Received))
                .then_some(()))
            })?;
            self.wallet_call(2, |reply| WalletCommand::SettleInvoice {
                payment_hash,
                preimage,
                reply,
            })?;
            self.until(
                "SDK-2 buffered payment succeeded after eviction churn",
                |s| {
                    Ok((s.runtime.payment_status(payer, payment_hash)
                        == Some(PaymentStatus::Success)
                        && s.lsp_delivery_now(payment_hash).is_some_and(|d| {
                            matches!(d.status, LspPaymentDeliveryStatus::Succeeded)
                        }))
                    .then_some(()))
                },
            )?;
            self.wait_tenant_idle(&tenant_ids[2])?;
            self.evict_tenant_when_idle(&tenant_ids[2])?;
            let _ = self
                .lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_ids[1].clone(), reply))?;
            self.wait_tenant_channel_online(&tenant_ids[1], tenant_channels[1])?;
            self.runtime.note(format_args!(
                "lsp-random eviction-churn SDK-2 buffered payment settled {payment_hash}"
            ));
        }

        // Part B: Cooperative close (SendClosingSigned) of SDK-0's secondary channel while
        // keeping SDK-0's primary channel open and healthy!
        self.ensure_wallet_online(0)?;
        self.wait_tenant_idle(&tenant_ids[0])?;
        let prev_close_sigs = self.runtime.wallets.borrow()[0].closing_sigs_applied;
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DuplicateRequest {
                method: Some("submit_channel_signature"),
                copies: 2,
                once: true,
            }));
        self.wallet_call(0, |reply| WalletCommand::CooperativeClose {
            channel_id: state.secondary_channel_0,
            fee_rate: 1_000,
            reply,
        })?;
        self.until(
            "SDK-0 cooperative close signed (SendClosingSigned) and confirmed on-chain",
            |s| {
                let closed_on_lsp = s
                    .runtime
                    .channel(lsp_node, &state.secondary_channel_0)
                    .is_some_and(|st| {
                        matches!(st.state, ChannelState::Closed(flags) if flags.contains(CloseFlags::COOPERATIVE))
                    });
                let signed = s.runtime.wallets.borrow()[0].closing_sigs_applied > prev_close_sigs;
                Ok((closed_on_lsp && signed).then_some(()))
            },
        )?;
        self.wait_tenant_channel_online(&tenant_ids[0], tenant_channels[0])?;
        self.runtime.note(format_args!(
            "lsp-random cooperative-close SendClosingSigned closed secondary channel {} while primary {} stayed online",
            state.secondary_channel_0, tenant_channels[0]
        ));
        Ok(())
    }

    pub(super) fn run_random_lsp_offline_force_close_probe(
        &mut self,
        seed: u64,
        state: &RandomLspState,
        public_nodes: usize,
    ) -> Result<bool> {
        let lsp_node = state.lsp_node;
        let Some(towers) = &self.runtime.watchtowers else {
            return Ok(false);
        };
        if towers[lsp_node].mode != crate::tower::TowerMode::BuiltIn {
            return Ok(false);
        }
        self.start_lsp_watchtower()?;
        let tenant_id = &state.tenant_ids[0];
        let channel_id = state.tenant_channels[0];
        self.ensure_wallet_online(0)?;
        self.wait_tenant_idle(tenant_id)?;
        self.wait_tenant_channel_online(tenant_id, channel_id)?;

        let mut rng = StdRng::seed_from_u64(seed ^ 0x6c73_7077_6174_6368);
        let mut candidates: Vec<usize> = (0..public_nodes).filter(|&n| n != lsp_node).collect();
        candidates.shuffle(&mut rng);

        // Commit 2 simultaneous hold TLCs (>= 61 CKB each) on SDK-0's hosted channel before
        // force-closing offline so watchtower settlement executes multi-round ExternalFundingRequired!
        let mut held_tlcs: Vec<(usize, u128, Hash256, Hash256)> = Vec::new();
        for _ in 0..2 {
            let mut chosen = None;
            for amount in [500 * 100_000_000u128, 200 * 100_000_000u128] {
                let mut pre_bytes = [0u8; 32];
                rng.fill(&mut pre_bytes);
                let preimage = Hash256::from(pre_bytes);
                let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
                let inv: InvoiceResult =
                    self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
                        amount,
                        payment_hash,
                        preimage,
                        reply,
                    })?;
                if let Some(payer) = self.pick_public_payer(&candidates, &inv.invoice_address)? {
                    chosen = Some((payer, amount, preimage, payment_hash, inv.invoice_address));
                    break;
                }
            }
            if let Some((payer, amount, preimage, payment_hash, invoice_address)) = chosen {
                let sent = self.pay(
                    payer,
                    SendPaymentCommand {
                        invoice: Some(invoice_address),
                        ..Default::default()
                    },
                )?;
                ensure!(sent == payment_hash);
                held_tlcs.push((payer, amount, preimage, payment_hash));
            }
        }
        if held_tlcs.is_empty() {
            return Ok(false);
        }
        self.until(
            "random LSP force-close multi-TLCs committed on hosted channel",
            |s| {
                let all_received = held_tlcs.iter().all(|&(_, _, _, hash)| {
                    s.wallet_invoice_status_from_store(tenant_id, hash)
                        .unwrap_or(None)
                        == Some(CkbInvoiceStatus::Received)
                });
                Ok(all_received.then_some(()))
            },
        )?;

        // Crash SDK-0 so it is offline while LSP force-closes the hosted private channel.
        self.runtime.crash_wallet(0);
        let shutdown_reply = self.runtime.request(lsp_node, |reply| {
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
        self.reply("random LSP force close hosted channel", shutdown_reply)?;
        let commitment = self.until("random LSP force close confirmed on chain", |s| {
            Ok(s.runtime.channel(lsp_node, &channel_id).and_then(|st| {
                matches!(st.state, ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL))
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
            }))
        })?;
        self.runtime.note(format_args!(
            "lsp-random force-closed hosted channel {channel_id} commitment={commitment} while SDK-0 offline tlcs={}",
            held_tlcs.len()
        ));

        // Advance past commitment delay while keeping TLCs unexpired.
        self.advance_clock_past(self.clock_ms());

        // SDK-0 returns online and registers preimages with tenant watchtower.
        // On half the seeds, pause signing across WATCHTOWER_HOLD_TTL_MS (30 min) so the tenant
        // watchtower creates an ExternalFundingRequired request, expires it past TTL, and enters
        // WatchtowerSigningStatus::Backoff before SDK-0 observes Backoff and recovers!
        let exercise_backoff = seed.is_multiple_of(4);
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;
        if exercise_backoff {
            self.runtime.wallets.borrow()[0].signing_paused.set(true);
        }
        for &(_, _, preimage, payment_hash) in &held_tlcs {
            self.wallet_call(0, |reply| WalletCommand::CreateWatchPreimage {
                payment_hash,
                preimage,
                reply,
            })?;
        }
        if exercise_backoff {
            // Scan 1 creates the unsigned ExternalFundingRequired request while SDK-0 is paused;
            // Scan 2 after WATCHTOWER_HOLD_TTL_MS expires it and arms Backoff!
            self.scan_onchain([lsp_node])?;
            for _ in 0..6 {
                self.step()?;
            }
            self.advance_clock_past(
                self.clock_ms()
                    .saturating_add(WATCHTOWER_HOLD_TTL_MS + 5_000),
            );
            self.scan_onchain([lsp_node])?;
            for _ in 0..6 {
                self.step()?;
            }
            self.runtime.wallets.borrow()[0].signing_paused.set(false);
            self.until(
                "SDK-0 observes WatchtowerSigningStatus::Backoff after TTL expiry",
                |s| Ok((s.runtime.wallets.borrow()[0].watchtower_backoff_hits >= 1).then_some(())),
            )?;
            self.runtime.note(format_args!(
                "lsp-random SDK-0 observed Watchtower Backoff hits={}",
                self.runtime.wallets.borrow()[0].watchtower_backoff_hits
            ));
            // Advance past 5-minute initial backoff so fresh ExternalFundingRequired requests resume.
            self.advance_clock_past(self.clock_ms().saturating_add(6 * 60 * 1000));
        }
        let prev_dups = self.wallet_rpc(0).duplicates_sent();
        self.wallet_rpc(0)
            .set_fault(Some(LspRpcFault::DuplicateRequest {
                method: Some("submit_watchtower_signature"),
                copies: 2,
                once: true,
            }));
        // Inject a transient Watchtower send_transaction Timeout fault so watchtower exercises
        // handle_external_settlement_send_error_on_chain before succeeding on retry!
        self.runtime.chain_access[lsp_node].set_for(
            ChainEndpoint::Watchtower,
            "send_transaction",
            ChainFault::Timeout(Duration::from_millis(20)),
        );
        let mut cleared_wt_fault = false;
        self.until(
            "random SDK-0 self-funds and settles multi-TLC hosted commitment on-chain via watchtower",
            |s| {
                if !cleared_wt_fault {
                    let hits = s.runtime.chain_access[lsp_node].take_hits();
                    if hits.iter().any(|h| {
                        h.endpoint == ChainEndpoint::Watchtower && h.request == "send_transaction"
                    }) {
                        s.runtime.chain_access[lsp_node].set(ChainFault::Healthy);
                        cleared_wt_fault = true;
                        s.runtime.note(
                            "lsp-random cleared Watchtower send_transaction Timeout after hit",
                        );
                    }
                }
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                }
                let settled = s.runtime.chain.commitment_settled(commitment);
                let all_payers_ok = held_tlcs.iter().all(|&(payer, _, _, hash)| {
                    s.runtime.payment_status(payer, hash) == Some(PaymentStatus::Success)
                });
                let signed = s.runtime.wallets.borrow()[0].watchtower_sigs_applied
                    >= held_tlcs.len() as u64;
                Ok((settled && all_payers_ok && signed).then_some(()))
            },
        )?;
        self.runtime.chain_access[lsp_node].set(ChainFault::Healthy);
        ensure!(
            self.wallet_rpc(0).duplicates_sent() > prev_dups,
            "duplicate submit_watchtower_signature did not fire in random LSP probe"
        );
        self.wait_payment_actor(lsp_node)?;
        let _ = simulation::take_tlc_settlements();
        self.runtime.note(format_args!(
            "lsp-random self-funded watchtower settled commitment={commitment} tlcs={} backoff={} watchtower_sigs={}",
            held_tlcs.len(),
            exercise_backoff,
            self.runtime.wallets.borrow()[0].watchtower_sigs_applied
        ));
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_tenant_to_tenant_payment(
        &mut self,
        seq: usize,
        from_wallet: usize,
        to_wallet: usize,
        tenant_ids: &[TenantId],
        tenant_channels: &[Hash256],
        amount: u128,
        payment_hash: Hash256,
        preimage: Hash256,
        invoice_address: &str,
        chaos: bool,
        rng: &mut StdRng,
    ) -> Result<()> {
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
                invoice: invoice_address.to_owned(),
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
            let payer_ok = s
                .wallet_payment_status_from_store(&tenant_ids[from_wallet], payment_hash)
                == Some(PaymentStatus::Success);
            Ok((paid && payer_ok).then_some(()))
        })?;
        self.runtime.note(format_args!(
            "lsp-random #{seq} {}->{} succeeded {payment_hash} amount={amount}",
            WALLET_HOSTS[from_wallet], WALLET_HOSTS[to_wallet]
        ));
        Ok(())
    }

    fn pick_public_payer(&mut self, candidates: &[usize], invoice: &str) -> Result<Option<usize>> {
        for &payer in candidates {
            if !self.runtime.node_available(payer) {
                continue;
            }
            let mut reply = self.runtime.request(payer, |reply| {
                FiberActorCommand::SendPayment(
                    SendPaymentCommand {
                        invoice: Some(invoice.to_owned()),
                        dry_run: true,
                        ..Default::default()
                    },
                    reply,
                )
            })?;
            let result = self.until("dry-run public route to LSP", |_| receive(&mut reply))?;
            if result.is_ok() {
                return Ok(Some(payer));
            }
        }
        Ok(None)
    }

    fn sync_lsp_trampoline_gossip(&mut self, public_nodes: usize) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        let lsp_pubkey = self.runtime.peers[lsp_node];
        let ann = self.until("LSP node announcement available", |s| {
            let store = s.runtime.store(lsp_node)?;
            Ok(store
                .get_latest_node_announcement(&lsp_pubkey)
                .filter(|node| node.features.supports_trampoline_routing()))
        })?;
        for node in 0..public_nodes {
            if node == lsp_node || !self.runtime.node_available(node) {
                continue;
            }
            let has_ann = self
                .runtime
                .store(node)
                .ok()
                .and_then(|s| s.get_latest_node_announcement(&lsp_pubkey))
                .is_some_and(|n| n.features.supports_trampoline_routing());
            if !has_ann {
                let slots = self.runtime.nodes.borrow();
                if let Some(live) = &slots[node].node {
                    send(
                        &live.actor,
                        NetworkActorMessage::new_event(PublicNetworkEvent::GossipMessageUpdates(
                            GossipMessageUpdates::new(vec![
                                BroadcastMessageWithTimestamp::NodeAnnouncement(ann.clone()),
                            ]),
                        )),
                    )?;
                }
            }
            self.wait_trampoline_ready(node, lsp_node)?;
        }
        Ok(())
    }

    fn lsp_node(&self) -> Result<usize> {
        Ok(self.runtime.lsp_node.unwrap_or(FIXTURE_LSP_NODE))
    }

    fn start_lsp_watchtower(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        if let Some(towers) = &self.runtime.watchtowers {
            towers[lsp_node].enabled.send_replace(true);
            let tower = towers[lsp_node].clone();
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
        self.lsp_call_raw(command)?.map_err(|err| anyhow!(err))
    }

    fn lsp_call_raw<T>(
        &mut self,
        command: impl FnOnce(ractor::RpcReplyPort<Result<T, String>>) -> LspServiceMessage,
    ) -> Result<Result<T, String>> {
        let actor = self.runtime.lsp_actor()?;
        let (tx, mut rx) = oneshot::channel();
        actor
            .send_message(command(tx.into()))
            .map_err(|err| anyhow!("send LSP service command: {err}"))?;
        self.until("LSP service command", |_| receive(&mut rx))
    }

    fn lsp_delivery_now(&self, payment_hash: Hash256) -> Option<LspPaymentDelivery> {
        let lsp_node = self.lsp_node().ok()?;
        let store = self.runtime.store(lsp_node).ok()?;
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
        let lsp_node = self.lsp_node()?;
        self.until("hosted tenant channels idle", |s| {
            let store = s.runtime.store(lsp_node)?;
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
        let lsp_node = self.lsp_node()?;
        self.until("hosted tenant channel online on LSP", |s| {
            let lsp_ready = s
                .runtime
                .channel(lsp_node, &channel_id)
                .is_some_and(|state| {
                    state.state == ChannelState::ChannelReady
                        && state.connectivity_state == fnn::fiber::ChannelConnectivityState::Online
                });
            let tenant_ready = s.runtime.store(lsp_node).ok().is_some_and(|store| {
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

    fn tenant_any_channel_awaiting_signature(&self, tenant_id: &TenantId) -> bool {
        let Ok(lsp_node) = self.lsp_node() else {
            return false;
        };
        let Ok(store) = self.runtime.store(lsp_node) else {
            return false;
        };
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        let lsp_pubkey = self.runtime.peers[lsp_node];
        fnn::fiber::channel::ChannelActorStateStore::get_channel_ids_by_pubkey(
            &tenant_store,
            &lsp_pubkey,
        )
        .into_iter()
        .any(|ch| {
            fnn::fiber::channel::ChannelActorStateStore::get_channel_actor_state(&tenant_store, &ch)
                .is_some_and(|st| st.signing_context.is_awaiting_signature())
        })
    }

    fn tenant_channel_awaiting_signature(&self, tenant_id: &TenantId, channel_id: Hash256) -> bool {
        let Ok(lsp_node) = self.lsp_node() else {
            return false;
        };
        let Ok(store) = self.runtime.store(lsp_node) else {
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
        let lsp_node = self.lsp_node()?;
        let store = self.runtime.store(lsp_node)?;
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        Ok(tenant_store.get_invoice_status(&payment_hash))
    }

    fn wallet_payment_status_from_store(
        &self,
        tenant_id: &TenantId,
        payment_hash: Hash256,
    ) -> Option<PaymentStatus> {
        use fnn::fiber::payment::PaymentSessionExt;
        let lsp_node = self.lsp_node().ok()?;
        let store = self.runtime.store(lsp_node).ok()?;
        let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()));
        fnn::fiber::graph::NetworkGraphStateStore::get_payment_session(&tenant_store, payment_hash)
            .map(|session| session.effective_status(&tenant_store))
    }

    pub(super) fn verify_lsp_indexes(
        &self,
        public_nodes: &[usize],
        tenants: &[TenantId],
    ) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        for &node in public_nodes {
            if !self.runtime.node_available(node) {
                continue;
            }
            let store = self.runtime.store(node)?;
            simulation::verify_payment_hash_index(&store)
                .map_err(|err| anyhow!("{} payment-hash index: {err}", NAMES[node]))?;
        }
        let lsp_store = self.runtime.store(lsp_node)?;
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
