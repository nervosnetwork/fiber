use anyhow::{ensure, Context, Result};
use ckb_types::prelude::Unpack;
use fnn::fiber::channel::{ChannelCommand, ChannelCommandWithId, ShutdownCommand};
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::{ChannelState, FiberActorCommand, Hash256, PaymentStatus, Privkey};
use fnn::fiber_types::{CloseFlags, HashAlgorithm, NodeId};
use fnn::invoice::CkbInvoiceStatus;
use fnn::lsp::{LspPaymentDeliveryStore, LspServiceMessage};
use fnn::store::NodeNamespace;
use fnn::watchtower::WatchtowerStore;

use crate::lsp::{LspTamperKind, WalletCommand};
use crate::oracle::{ConsistencyOracle, Endpoint, LivenessOracle};

use super::funding::OpeningFault;
use super::{Driver, FUNDING_AMOUNT};

const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;

impl Driver {
    /// Exercise Byzantine stale-commitment broadcast and full LSP/watchtower tamper resistance:
    /// - Probe 1: Broadcast a revoked local commitment transaction directly to chain and verify
    ///   that the counterparty's built-in watchtower autonomously executes a penalty sweep.
    /// - Probe 2: Exercise all 5 channel-side `LspTamperKind` variants against `SDK-0`, verifying
    ///   rejection with zero persistent store mutation and clean recovery on the next poll.
    /// - Probe 3: Force-close a hosted channel with a held TLC while `SDK-0` is offline, then
    ///   exercise all 3 watchtower external-funding `LspTamperKind` variants before verifying
    ///   clean self-funded on-chain settlement.
    pub(super) fn byzantine_stale_and_tamper(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        let mut tracked_payments: Vec<(Endpoint, Hash256)> = Vec::new();
        let mut tracked_deliveries: Vec<Hash256> = Vec::new();
        let mut audit_payments: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();

        // -------------------------------------------------------------------------
        // Probe 1: Revoked / Stale Commitment Broadcast -> Watchtower Penalty Sweep
        // -------------------------------------------------------------------------
        let victim = 2usize;
        self.start_watchtowers([0, victim])?;
        self.start_watchtowers([0, lsp_node])?;
        let ch_stale = self
            .open_between(0, victim, FUNDING_AMOUNT, 0, true, OpeningFault::None)?
            .channel;

        let pay1_amount = 2_000 * 100_000_000;
        let pay1_hash = self.pay(
            0,
            SendPaymentCommand {
                target_pubkey: Some(self.runtime.peers[victim]),
                amount: Some(pay1_amount),
                keysend: Some(true),
                max_parts: Some(1),
                ..Default::default()
            },
        )?;
        self.until("Probe 1 Pay #1 settles cleanly", |s| {
            let ok = s.runtime.payment_status(0, pay1_hash) == Some(PaymentStatus::Success);
            let idle = [0, victim].into_iter().all(|n| {
                s.runtime
                    .channel(n, &ch_stale)
                    .is_some_and(|st| !st.tlc_state.waiting_ack && !st.any_tlc_pending())
            });
            Ok((ok && idle).then_some(()))
        })?;
        self.wait_payment_actor(0)?;
        tracked_payments.push((Endpoint::Public(0), pay1_hash));

        // Snapshot Node 0's signed local commitment at this round before advancing state.
        let stale_tx = self.runtime.snapshot_commitment_tx(0, &ch_stale)?;
        let stale_commit_num = {
            let output = stale_tx
                .output(0)
                .context("stale commitment transaction missing output 0")?;
            let args = output.lock().args().raw_data();
            ensure!(
                args.len() >= 36,
                "unexpected stale commitment lock args length {}",
                args.len()
            );
            u64::from_be_bytes(args[28..36].try_into().expect("8-byte commitment number"))
        };

        // Advance the channel by another settled payment so `stale_tx` is revoked and Node 2's
        // watchtower receives the corresponding RevocationData.
        let pay2_amount = 3_000 * 100_000_000;
        let pay2_hash = self.pay(
            0,
            SendPaymentCommand {
                target_pubkey: Some(self.runtime.peers[victim]),
                amount: Some(pay2_amount),
                keysend: Some(true),
                max_parts: Some(1),
                ..Default::default()
            },
        )?;
        self.until("Probe 1 Pay #2 settles cleanly", |s| {
            let ok = s.runtime.payment_status(0, pay2_hash) == Some(PaymentStatus::Success);
            let idle = [0, victim].into_iter().all(|n| {
                s.runtime
                    .channel(n, &ch_stale)
                    .is_some_and(|st| !st.tlc_state.waiting_ack && !st.any_tlc_pending())
            });
            Ok((ok && idle).then_some(()))
        })?;
        self.wait_payment_actor(0)?;
        tracked_payments.push((Endpoint::Public(0), pay2_hash));

        self.until(
            "Node 2 watchtower persists revocation data for stale commitment",
            |s| {
                let towers = s
                    .runtime
                    .watchtowers
                    .as_ref()
                    .context("watchtower controls missing")?;
                let store = towers[victim].store.borrow();
                let has_revocation = store.as_ref().is_some_and(|store| {
                    store
                        .get_watch_channels_with_nodes()
                        .into_iter()
                        .find(|(owner, ch)| *owner == NodeId::local() && ch.channel_id == ch_stale)
                        .and_then(|(_, ch)| ch.revocation_data)
                        .is_some_and(|rev| rev.commitment_number >= stale_commit_num)
                });
                Ok(has_revocation.then_some(()))
            },
        )?;

        let node0_key = Privkey::from([1u8; 32]);
        let victim_key = Privkey::from([(victim + 1) as u8; 32]);
        let node0_wt_before = self
            .runtime
            .chain
            .watchtower_wallet_capacity(node0_key.clone());
        let victim_wt_before = self
            .runtime
            .chain
            .watchtower_wallet_capacity(victim_key.clone());
        let victim_shutdown_lock = self
            .runtime
            .channel(victim, &ch_stale)
            .context("missing victim ch_stale state")?
            .get_local_shutdown_script();

        let stale_hash = self.runtime.chain.submit_raw_transaction(stale_tx)?;
        self.runtime.note(format_args!(
            "Probe 1 broadcast revoked commitment channel={ch_stale} num={stale_commit_num} tx={stale_hash}"
        ));
        // Scan honest victim immediately so the revocation penalty sweep fires before any local delay.
        self.scan_onchain([victim])?;

        self.until(
            "watchtower penalty-sweeps revoked commitment and honest victim observes closed channel",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(25) {
                    s.scan_onchain([victim])?;
                }
                let settled = s.runtime.chain.commitment_settled(stale_hash);
                let victim_closed = s.runtime.channel(victim, &ch_stale).is_some_and(|state| {
                    matches!(
                        state.state,
                        ChannelState::Closed(flags)
                            if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
                    ) && state
                        .shutdown_transaction_hash
                        .as_ref()
                        .map(|h| Hash256::from(h.clone()))
                        == Some(stale_hash)
                        && !state.is_waiting_onchain_settlement()
                });
                Ok((settled && victim_closed).then_some(()))
            },
        )?;

        let spend_hash = self
            .runtime
            .chain
            .commitment_spend(stale_hash)
            .context("missing penalty sweep spend for revoked commitment")?;
        let spend_tx = self
            .runtime
            .chain
            .confirmed_transaction(spend_hash)
            .context("missing confirmed penalty sweep transaction")?;
        let penalty_output = spend_tx
            .output(0)
            .context("penalty sweep transaction missing output 0")?;
        ensure!(
            penalty_output.lock() == victim_shutdown_lock,
            "penalty sweep output 0 did not pay to honest victim shutdown lock"
        );
        let penalty_gain = u128::from(Unpack::<u64>::unpack(&penalty_output.capacity()));
        ensure!(
            penalty_gain > FUNDING_AMOUNT + FUNDING_AMOUNT / 2,
            "victim sweep output {penalty_gain} too small for full revoked-commitment penalty sweep"
        );
        let witness0 = spend_tx
            .witnesses()
            .get(0)
            .context("penalty sweep missing commitment witness")?;
        let witness_bytes = witness0.raw_data();
        ensure!(
            witness_bytes.get(16) == Some(&0x00),
            "commitment spend witness did not use revocation unlock_count=0x00"
        );
        let node0_wt_after = self.runtime.chain.watchtower_wallet_capacity(node0_key);
        let victim_wt_after = self.runtime.chain.watchtower_wallet_capacity(victim_key);
        ensure!(
            victim_wt_after < victim_wt_before,
            "honest victim watchtower did not fund the revocation tx: before={victim_wt_before} after={victim_wt_after}"
        );
        ensure!(
            node0_wt_after == node0_wt_before,
            "cheating Node 0 watchtower unexpectedly spent fee cells: before={node0_wt_before} after={node0_wt_after}"
        );
        self.coverage.hit("revoked_commitment_penalty_swept");
        self.runtime.note(format_args!(
            "Probe 1 passed: Node {victim} watchtower swept revoked commitment {stale_hash} gain={penalty_gain}"
        ));

        // -------------------------------------------------------------------------
        // Probe 2: Byzantine LSP Channel Signing Tampers + Zero-Mutation Store Oracle
        // -------------------------------------------------------------------------
        let ch_pub = self
            .open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?
            .channel;
        self.wait_online(0, lsp_node, &ch_pub)?;
        self.wait_trampoline_ready(0, lsp_node)?;
        self.wait_wallet_online(0)?;

        let tenant_id = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        let ch_hosted = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_id, ch_hosted)?;

        // Seed SDK-0 with 5_000 CKB of inbound liquidity.
        let seed_pre = Hash256::from([0x60; 32]);
        let seed_hash: Hash256 = HashAlgorithm::CkbHash.hash(seed_pre).into();
        let seed_inv = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            amount: 5_000 * 100_000_000,
            payment_hash: seed_hash,
            preimage: seed_pre,
            reply,
        })?;
        let sent_seed = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(seed_inv.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_seed == seed_hash);
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
        self.until("seed inbound payment settles cleanly", |s| {
            Ok(
                (s.runtime.payment_status(0, seed_hash) == Some(PaymentStatus::Success)
                    && s.wallet_invoice_status_from_store(&tenant_id, seed_hash)
                        .unwrap_or(None)
                        == Some(CkbInvoiceStatus::Paid))
                .then_some(()),
            )
        })?;
        self.wait_payment_actor(0)?;
        self.wait_tenant_idle(&tenant_id)?;
        tracked_payments.push((Endpoint::Public(0), seed_hash));
        tracked_deliveries.push(seed_hash);
        audit_payments.push((
            Endpoint::Public(0),
            Endpoint::Tenant(tenant_id.clone()),
            seed_hash,
        ));

        let channel_tampers = [
            LspTamperKind::CommitmentBalance { delta: 1 },
            LspTamperKind::SettlementTlcAmount { delta: 1 },
            LspTamperKind::SettlementTlcExpiry { delta: 1_000 },
            LspTamperKind::DropFirstTlc,
            LspTamperKind::BadCommitmentNumber { delta: 1 },
        ];

        for (idx, kind) in channel_tampers.into_iter().enumerate() {
            let preimage = Hash256::from([0x61 + idx as u8; 32]);
            let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            let inv = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
                amount: 500 * 100_000_000,
                payment_hash,
                preimage,
                reply,
            })?;

            let prev_rejections = self.runtime.wallets.borrow()[0].sdk_rejections_verified;
            self.runtime.wallets.borrow()[0]
                .sdk_policy
                .borrow_mut()
                .lsp_tamper = Some(kind);

            let sent = self.pay(
                0,
                SendPaymentCommand {
                    invoice: Some(inv.invoice_address),
                    ..Default::default()
                },
            )?;
            ensure!(sent == payment_hash);

            self.until(
                "SDK-0 rejects tampered channel signing payload with zero store mutation",
                |s| {
                    Ok(
                        (s.runtime.wallets.borrow()[0].sdk_rejections_verified > prev_rejections)
                            .then_some(()),
                    )
                },
            )?;
            ensure!(
                self.runtime.wallets.borrow()[0]
                    .sdk_policy
                    .borrow()
                    .lsp_tamper
                    .is_none(),
                "channel tamper {kind:?} was not consumed"
            );
            self.coverage.hit("lsp_channel_tamper_rejected");

            self.until(
                "tampered inbound payment recovers and commits on clean retry poll",
                |s| {
                    Ok((s
                        .wallet_invoice_status_from_store(&tenant_id, payment_hash)
                        .unwrap_or(None)
                        == Some(CkbInvoiceStatus::Received))
                    .then_some(()))
                },
            )?;
            self.wallet_call(0, |reply| WalletCommand::SettleInvoice {
                payment_hash,
                preimage,
                reply,
            })?;
            self.until(
                "inbound payment settles cleanly after channel tamper recovery",
                |s| {
                    Ok(
                        (s.runtime.payment_status(0, payment_hash) == Some(PaymentStatus::Success)
                            && s.wallet_invoice_status_from_store(&tenant_id, payment_hash)
                                .unwrap_or(None)
                                == Some(CkbInvoiceStatus::Paid))
                        .then_some(()),
                    )
                },
            )?;
            self.wait_payment_actor(0)?;
            self.wait_tenant_idle(&tenant_id)?;

            tracked_payments.push((Endpoint::Public(0), payment_hash));
            tracked_deliveries.push(payment_hash);
            audit_payments.push((
                Endpoint::Public(0),
                Endpoint::Tenant(tenant_id.clone()),
                payment_hash,
            ));
            self.runtime.note(format_args!(
                "Probe 2 channel tamper #{idx} ({kind:?}) rejected with zero store mutation and recovered cleanly"
            ));
        }

        // -------------------------------------------------------------------------
        // Probe 3: Byzantine LSP Watchtower External-Funding Tampers + Settlement
        // -------------------------------------------------------------------------
        let pre_wt = Hash256::from([0x70; 32]);
        let p_wt: Hash256 = HashAlgorithm::CkbHash.hash(pre_wt).into();
        let inv_wt = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
            amount: 1_000 * 100_000_000,
            payment_hash: p_wt,
            preimage: pre_wt,
            reply,
        })?;
        let sent_wt = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(inv_wt.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent_wt == p_wt);
        self.until("Probe 3 hold TLC committed on hosted channel", |s| {
            Ok((s
                .wallet_invoice_status_from_store(&tenant_id, p_wt)
                .unwrap_or(None)
                == Some(CkbInvoiceStatus::Received))
            .then_some(()))
        })?;

        // Crash SDK-0 and force-close the hosted channel from LSP.
        self.runtime.crash_wallet(0);
        let shutdown_reply = self.runtime.request(lsp_node, |reply| {
            FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: ch_hosted,
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
        self.reply("LSP force close hosted tenant channel", shutdown_reply)?;
        let commitment = self.until("LSP force close confirmed on chain", |s| {
            Ok(s.runtime.channel(lsp_node, &ch_hosted).and_then(|state| {
                matches!(
                    state.state,
                    ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                )
                .then(|| state.shutdown_transaction_hash.clone().map(Hash256::from))
                .flatten()
            }))
        })?;
        self.runtime.note(format_args!(
            "Probe 3 LSP force-closed hosted channel {ch_hosted} commitment={commitment} while SDK-0 offline"
        ));

        // Advance virtual clock by ~1s (past commitment delay while keeping p_wt unexpired).
        self.advance_clock_past(self.clock_ms());

        // Restart SDK-0, arm the first watchtower tamper before registering the preimage so
        // every watchtower tamper fires before any clean signature can be submitted.
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;

        let watchtower_tampers = [
            LspTamperKind::WatchtowerFeeDrain {
                extra_fee: 10_000_000,
            },
            LspTamperKind::WatchtowerWrongSince {
                since: 0x4000_0000_0000_0001,
            },
            LspTamperKind::WatchtowerWrongDestination,
        ];

        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .lsp_tamper = Some(watchtower_tampers[0]);
        self.wallet_call(0, |reply| WalletCommand::CreateWatchPreimage {
            payment_hash: p_wt,
            preimage: pre_wt,
            reply,
        })?;

        for (idx, kind) in watchtower_tampers.into_iter().enumerate() {
            let expected_count = idx as u64 + 1;
            if idx > 0 {
                self.runtime.wallets.borrow()[0]
                    .sdk_policy
                    .borrow_mut()
                    .lsp_tamper = Some(kind);
            }
            self.until(
                "SDK-0 rejects tampered watchtower external-funding template with zero store mutation",
                |s| {
                    if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                        s.scan_onchain([lsp_node])?;
                    }
                    Ok(
                        (s.runtime.wallets.borrow()[0].sdk_watchtower_rejections_verified
                            >= expected_count)
                            .then_some(()),
                    )
                },
            )?;
            ensure!(
                self.runtime.wallets.borrow()[0]
                    .sdk_policy
                    .borrow()
                    .lsp_tamper
                    .is_none(),
                "watchtower tamper {kind:?} was not consumed"
            );
            ensure!(
                self.runtime.wallets.borrow()[0].watchtower_sigs_applied == 0,
                "tampered watchtower template unexpectedly resulted in a submitted signature"
            );
            self.coverage.hit("lsp_watchtower_tamper_rejected");
            self.runtime.note(format_args!(
                "Probe 3 watchtower tamper #{idx} ({kind:?}) rejected with zero store mutation"
            ));
        }

        // Now let the clean watchtower poll self-fund, sign, and settle p_wt on-chain.
        let tenant_ctx = self
            .lsp_call(|reply| LspServiceMessage::GetTenantRpcContext(tenant_id.clone(), reply))?;
        self.until(
            "SDK-0 self-funds and settles p_wt TLC on-chain via watchtower after tamper rejections",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                    tenant_ctx.check_channels_shutdown();
                }
                let settled = s.runtime.chain.commitment_settled(commitment);
                let payer_ok = s.runtime.payment_status(0, p_wt) == Some(PaymentStatus::Success);
                let signed = s.runtime.wallets.borrow()[0].watchtower_sigs_applied >= 1;
                let lsp_closed = s
                    .runtime
                    .channel(lsp_node, &ch_hosted)
                    .is_some_and(|st| st.is_closed() && !st.is_waiting_onchain_settlement());
                let tenant_closed = s
                    .tenant_channel_state(&tenant_id, ch_hosted)?
                    .is_some_and(|st| st.is_closed() && !st.is_waiting_onchain_settlement());
                let pub_drained = [0, lsp_node].into_iter().all(|node| {
                    s.runtime.channel(node, &ch_pub).is_some_and(|st| {
                        !st.tlc_state.waiting_ack && st.tlc_state.all_tlcs().next().is_none()
                    })
                });
                let delivery_final = s.runtime.store(lsp_node).ok().is_some_and(|store| {
                    let lsp_store = store.namespaced(NodeNamespace::lsp_metadata());
                    lsp_store
                        .list_lsp_payment_deliveries_by_payment_hash(&p_wt)
                        .map(|list| !list.is_empty() && list.iter().all(|d| d.status.is_final()))
                        .unwrap_or(false)
                });
                Ok((settled
                    && payer_ok
                    && signed
                    && lsp_closed
                    && tenant_closed
                    && pub_drained
                    && delivery_final)
                    .then_some(()))
            },
        )?;
        self.wait_payment_actor(0)?;
        self.wait_payment_actor(lsp_node)?;
        tracked_payments.push((Endpoint::Public(0), p_wt));
        tracked_deliveries.push(p_wt);

        // Final strict liveness & consistency audits.
        self.verify_lsp_indexes(&[0, lsp_node], std::slice::from_ref(&tenant_id))?;
        LivenessOracle::assert_converged(&self.runtime, &tracked_payments, &tracked_deliveries)?;
        ConsistencyOracle::audit(
            &self.runtime,
            &audit_payments,
            &[(
                Endpoint::Public(lsp_node),
                Endpoint::Tenant(tenant_id),
                ch_hosted,
            )],
        )?;
        self.runtime.note(format_args!(
            "byzantine_stale_and_tamper passed: {}",
            self.coverage.format_summary()
        ));
        Ok(())
    }
}
