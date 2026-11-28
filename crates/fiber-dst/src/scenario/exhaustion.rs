use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use ckb_types::core::EpochNumberWithFraction;
use fiber_json_types::ChannelSigningTransition;
use fnn::fiber::channel::{
    ChannelActorMessage, ChannelActorStateStore, ChannelCommand, ChannelCommandWithId,
    ChannelEvent, ShutdownCommand, DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE,
    MAX_TLC_NUMBER_IN_FLIGHT, MIN_COMMITMENT_DELAY_EPOCHS,
};
use fnn::fiber::config::MILLI_SECONDS_PER_EPOCH;
use fnn::fiber::network::get_chain_hash;
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::types::{AddTlc, ChannelReadyV2, CommitmentSignedV2, FiberMessage, OpenChannelV2};
use fnn::fiber::{
    ChannelConnectivityState, ChannelState, FiberActorCommand, Hash256, NetworkActorMessage,
    PaymentStatus, Privkey, Pubkey,
};
use fnn::fiber_types::{ChannelFlags, CloseFlags, HashAlgorithm, ShuttingDownFlags};
use fnn::invoice::CkbInvoiceStatus;
use fnn::lsp::{LspPaymentDeliveryStore, LspServiceMessage, TenantId, TenantRuntimeStatus};
use fnn::store::NodeNamespace;
use serde_json::json;
use tokio::sync::oneshot;

use crate::host::{send, FrameKindFilter, FrameMatcher, InjectedWireAction, NAMES};
use crate::lsp::{LspTamperKind, RefusalTarget, WalletCommand, WALLET_HOSTS};
use crate::oracle::{ConsistencyOracle, Endpoint, LivenessOracle};

use super::faults::Fault;
use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;
const PEER_RESPONSE_TIMEOUT_ADVANCE_MS: u64 = 35_000;
const RECONNECT_TIMEOUT_STEP_MS: u64 = 20_000;
const RECONNECT_TIMEOUT_ROUNDS: usize = 4;
const EXTERNAL_FUNDING_TIMEOUT_ADVANCE_MS: u64 = 310_000;
const DEFAULT_FUNDING_TIMEOUT_ADVANCE_MS: u64 = 86_500_000;
const SEMANTIC_FLOOD_ADD_TLC_COUNT: u64 = 60;
const SEMANTIC_FLOOD_COMMIT_COUNT: u64 = 60;
const SEMANTIC_FLOOD_TOTAL: u64 = SEMANTIC_FLOOD_ADD_TLC_COUNT + SEMANTIC_FLOOD_COMMIT_COUNT;
const OPEN_CHANNEL_PER_ATTACKER: usize = 20;
const COOP_RACE_CONVERGENCE_TICKS: usize = 600;

fn tlc_expiry_delay_ms(commitment_delay_epoch: u64) -> u64 {
    let delay_epoch = EpochNumberWithFraction::from_full_value(commitment_delay_epoch);
    ((delay_epoch.number() as f64 + delay_epoch.index() as f64 / delay_epoch.length() as f64)
        * MILLI_SECONDS_PER_EPOCH as f64
        * 2.0
        / 3.0) as u64
}

fn synth_pubkey(domain: u8, attacker: usize, idx: usize) -> Pubkey {
    let mut bytes = [0x42u8; 32];
    bytes[0] = domain;
    bytes[1] = attacker as u8;
    bytes[2] = idx as u8;
    Privkey::from(bytes).pubkey()
}

impl Driver {
    /// Exercise aggressive P2P exhaustion and close-race invariants designed to surface real
    /// production defects:
    /// - Probe 1 (`CoopCloseVsForceCloseRace`): one-way `ClosingSignedV2` blackhole so node 1
    ///   broadcasts & confirms the 2-output cooperative close tx while node 0 escalates to a
    ///   conflicting force-close commitment tx.
    /// - Probe 2 (`PeriodicReconnectResetsTimeout`): periodic reconnects every 20s across 80s
    ///   while `ClosingSignedV2` is blackholed must NOT infinitely reset
    ///   `PEER_CHANNEL_RESPONSE_TIMEOUT`.
    /// - Probe 3 (`SemanticFloodWriteAmplification`): 120 rejected semantic frames must NOT cause
    ///   1-to-1 SQLite batch commit write amplification.
    /// - Probe 4 (`GlobalPendingOpenChannelExhaustion`): 5 connected attackers sending 20 valid
    ///   `OpenChannelV2` frames each (100 = global limit) must NOT permanently block honest peer 6
    ///   past funding timeout.
    pub(super) fn adversarial_exhaustion_and_races(&mut self) -> Result<()> {
        self.start_watchtowers([0, 1])?;
        self.start_watchtowers([6, 1])?;

        let mut closed_channels: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();

        // -------------------------------------------------------------------------
        // Probe 1: CoopCloseVsForceCloseRace
        // -------------------------------------------------------------------------
        let edge1 = self.open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?;
        let ch1 = edge1.channel;
        let funding_tx1 = edge1.funding_tx;

        // Blackhole ClosingSignedV2 ONLY from 1 -> 0 so 1 receives 0's ClosingSignedV2, combines
        // both partial signatures, and broadcasts/confirms the 2-output cooperative close tx
        // on-chain while 0 never receives 1's ClosingSignedV2.
        let bh_close_1_to_0 = Fault::BlackholePeer {
            from: 1,
            to: 0,
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

        let coop_tx1 = self.until(
            "Node 1 confirms 2-output coop close tx while 1->0 ClosingSignedV2 is blackholed",
            |s| {
                let blackholed = bh_close_1_to_0.hit(&s.runtime);
                let onchain_spend = s.runtime.chain.funding_spend(funding_tx1);
                let node1_closed = s.runtime.channel(1, &ch1).and_then(|st| {
                    matches!(st.state, ChannelState::Closed(_))
                        .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                        .flatten()
                });
                let tx = match (onchain_spend, node1_closed) {
                    (Some(spend), Some(n1_tx)) if spend == n1_tx => Some(spend),
                    _ => None,
                };
                Ok((blackholed && tx.is_some()).then_some(tx).flatten())
            },
        )?;
        self.runtime.note(format_args!(
            "Probe 1: Node 1 confirmed coop close tx={coop_tx1} on ch1={ch1} while Node 0 state={:?}",
            self.runtime.channel(0, &ch1).map(|st| st.state)
        ));

        // Advance past PEER_CHANNEL_RESPONSE_TIMEOUT without scanning on-chain yet so Node 0
        // escalates to force-close and races its conflicting commitment tx against coop_tx1.
        self.advance_clock_by(PEER_RESPONSE_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([0, 1])?;
        self.exhaustion_notify_channel_event(0, ch1, ChannelEvent::MaintainChannelTlcs)?;

        for _ in 0..40 {
            let raced = self.runtime.channel(0, &ch1).is_some_and(|st| {
                st.is_closed()
                    || matches!(
                        st.state,
                        ChannelState::ShuttingDown(flags)
                            if flags.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
                    )
                    || st
                        .session
                        .closing
                        .as_ref()
                        .is_some_and(|c| c.superseded_by_force_close)
            });
            if raced {
                break;
            }
            self.step()?;
        }
        bh_close_1_to_0.heal(&mut self.runtime);

        let mut probe1_converged = false;
        for tick in 0..COOP_RACE_CONVERGENCE_TICKS {
            if tick.is_multiple_of(25) {
                self.scan_onchain([0, 1])?;
            }
            if tick.is_multiple_of(100) {
                self.trigger_channel_maintenance([0, 1])?;
            }
            let converged = [0usize, 1usize].into_iter().all(|node| {
                self.runtime.channel(node, &ch1).is_some_and(|st| {
                    matches!(st.state, ChannelState::Closed(_))
                        && st
                            .shutdown_transaction_hash
                            .as_ref()
                            .map(|h| Hash256::from(h.clone()))
                            == Some(coop_tx1)
                        && !st.is_waiting_onchain_settlement()
                })
            });
            if converged {
                probe1_converged = true;
                break;
            }
            self.step()?;
        }
        let mut exposed_bugs: Vec<String> = Vec::new();
        if !probe1_converged {
            let st0 = self.runtime.channel(0, &ch1);
            let st1 = self.runtime.channel(1, &ch1);
            let msg = format!(
                "BUG EXPOSED (Probe 1 CoopCloseVsForceCloseRace): Node 0 failed to converge to confirmed 2-output coop close tx {coop_tx1} on ch1={ch1} after force-close race: node0_state={:?} node0_tx={:?} node0_superseded={:?} node1_state={:?} node1_tx={:?} onchain_spend={:?}",
                st0.as_ref().map(|s| s.state),
                st0.as_ref().and_then(|s| s.shutdown_transaction_hash.clone().map(Hash256::from)),
                st0.as_ref().and_then(|s| s.session.closing.as_ref().map(|c| c.superseded_by_force_close)),
                st1.as_ref().map(|s| s.state),
                st1.as_ref().and_then(|s| s.shutdown_transaction_hash.clone().map(Hash256::from)),
                self.runtime.chain.funding_spend(funding_tx1),
            );
            self.runtime.note(&msg);
            exposed_bugs.push(msg);
        } else {
            closed_channels.push((Endpoint::Public(0), Endpoint::Public(1), ch1));
            self.coverage.hit("exhaustion_coop_vs_force_close_race");
            self.runtime.note(format_args!(
                "Probe 1 passed: both endpoints converged to confirmed coop close tx={coop_tx1} on ch1={ch1}"
            ));
        }

        // -------------------------------------------------------------------------
        // Probe 2: PeriodicReconnectResetsTimeout
        // -------------------------------------------------------------------------
        let ch2 = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        let bh2_0_to_1 = Fault::BlackholePeer {
            from: 0,
            to: 1,
            matcher: FrameMatcher::channel(FrameKindFilter::ClosingSignedV2, ch2),
        }
        .inject(&mut self.runtime)?;
        let bh2_1_to_0 = Fault::BlackholePeer {
            from: 1,
            to: 0,
            matcher: FrameMatcher::channel(FrameKindFilter::ClosingSignedV2, ch2),
        }
        .inject(&mut self.runtime)?;

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
        self.reply("Node 0 initiates cooperative close on ch2", close_ch2)?;

        self.until(
            "Node 0 enters ShuttingDown on ch2 with local ClosingSignedV2 response",
            |s| {
                let waiting =
                    s.runtime.channel(0, &ch2).is_some_and(|st| {
                        matches!(st.state, ChannelState::ShuttingDown(_))
                            && st.session.closing.as_ref().is_some_and(|c| {
                                c.response.is_some() && c.remote_response.is_none()
                            })
                    });
                Ok((waiting && bh2_1_to_0.hit(&s.runtime)).then_some(()))
            },
        )?;

        let is_force_close_escalated = |s: &Self| {
            s.runtime.channel(0, &ch2).is_some_and(|st| {
                matches!(
                    st.state,
                    ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                ) || matches!(
                    st.state,
                    ChannelState::ShuttingDown(flags)
                        if flags.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
                )
            })
        };

        for round in 0..RECONNECT_TIMEOUT_ROUNDS {
            if is_force_close_escalated(self) {
                break;
            }
            self.advance_clock_by(RECONNECT_TIMEOUT_STEP_MS);
            self.runtime.reset_link(0, 1);
            self.until("ch2 finishes ReestablishChannelV2 after reconnect", |s| {
                if is_force_close_escalated(s) {
                    return Ok(Some(()));
                }
                let linked = s.runtime.link(0, 1);
                let synced = [0usize, 1usize].into_iter().all(|node| {
                    s.runtime.channel(node, &ch2).is_some_and(|st| {
                        st.is_closed()
                            || (!st.reestablishing
                                && st.connectivity_state == ChannelConnectivityState::Online)
                    })
                });
                Ok((linked && synced).then_some(()))
            })?;
            self.trigger_channel_maintenance([0, 1])?;
            let _ = self.exhaustion_notify_channel_event(0, ch2, ChannelEvent::MaintainChannelTlcs);
            for _ in 0..10 {
                if is_force_close_escalated(self) {
                    break;
                }
                self.step()?;
            }
            self.runtime.note(format_args!(
                "Probe 2 reconnect round {}/{RECONNECT_TIMEOUT_ROUNDS}: ch2 node0 state={:?} peer_wait_started_at={:?} now={}",
                round + 1,
                self.runtime.channel(0, &ch2).map(|st| st.state),
                self.runtime
                    .channel(0, &ch2)
                    .and_then(|st| st.session.closing.as_ref().map(|c| c.peer_wait_started_at)),
                self.clock_ms()
            ));
        }

        for _ in 0..50 {
            if is_force_close_escalated(self) {
                break;
            }
            self.trigger_channel_maintenance([0, 1])?;
            let _ = self.exhaustion_notify_channel_event(0, ch2, ChannelEvent::MaintainChannelTlcs);
            self.step()?;
        }

        let escalated = is_force_close_escalated(self);
        bh2_0_to_1.heal(&mut self.runtime);
        bh2_1_to_0.heal(&mut self.runtime);

        if !escalated {
            let st0 = self.runtime.channel(0, &ch2);
            let msg = format!(
                "BUG EXPOSED (Probe 2 PeriodicReconnectResetsTimeout): periodic reconnect infinitely reset PEER_CHANNEL_RESPONSE_TIMEOUT on coop close (ch2={ch2} after {}ms across {RECONNECT_TIMEOUT_ROUNDS} reconnects: node0_state={:?} peer_wait_started_at={:?} now={})",
                RECONNECT_TIMEOUT_STEP_MS * RECONNECT_TIMEOUT_ROUNDS as u64,
                st0.as_ref().map(|s| s.state),
                st0.as_ref()
                    .and_then(|s| s.session.closing.as_ref().map(|c| c.peer_wait_started_at)),
                self.clock_ms()
            );
            self.runtime.note(&msg);
            exposed_bugs.push(msg);
        } else {
            let commit2 = self.until("Node 0 confirms escalated force-close on ch2", |s| {
                Ok(s.runtime.channel(0, &ch2).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                }))
            })?;
            self.advance_clock_past(self.clock_ms());
            let _ = self.finish_onchain_close([0, 1], ch2, commit2)?;
            closed_channels.push((Endpoint::Public(0), Endpoint::Public(1), ch2));
            self.coverage
                .hit("exhaustion_periodic_reconnect_timeout_enforced");
        }

        // -------------------------------------------------------------------------
        // Probe 3: SemanticFloodWriteAmplification
        // -------------------------------------------------------------------------
        let ch3 = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        self.wait_online(0, 1, &ch3)?;
        for _ in 0..10 {
            self.step()?;
        }

        let ch3_0 = self
            .runtime
            .channel(0, &ch3)
            .context("missing ch3 state on Node 0")?;
        let valid_nonce = ch3_0
            .session
            .remote_nonce
            .clone()
            .or_else(|| ch3_0.session.bootstrap_remote_nonce.clone())
            .context("missing valid PubNonce from ch3")?;
        let shutdown_script = ch3_0.get_local_shutdown_script();
        let reserved_ckb_amount = ch3_0.local_reserved_ckb_amount;
        let nonce_json = serde_json::to_value(ChannelReadyV2 {
            channel_id: ch3,
            next_commitment_number: 0,
            next_commitment_nonce: valid_nonce.clone(),
        })?["next_commitment_nonce"]
            .clone();

        let mut semantic_actions = Vec::with_capacity(SEMANTIC_FLOOD_TOTAL as usize);
        for i in 0..SEMANTIC_FLOOD_ADD_TLC_COUNT {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0] = 0xc0;
            hash_bytes[1..9].copy_from_slice(&i.to_le_bytes());
            semantic_actions.push(InjectedWireAction::SendFrame(FiberMessage::add_tlc(
                AddTlc {
                    channel_id: ch3,
                    tlc_id: 10_000 + i,
                    amount: 0,
                    payment_hash: Hash256::from(hash_bytes),
                    expiry: self.clock_ms().saturating_add(3_600_000),
                    hash_algorithm: HashAlgorithm::CkbHash,
                    onion_packet: None,
                },
            )));
        }
        let sig_bytes = [0x11u8; 32];
        for i in 0..SEMANTIC_FLOOD_COMMIT_COUNT {
            let forged_cs: CommitmentSignedV2 = serde_json::from_value(json!({
                "channel_id": ch3,
                "commitment_number": 50_000u64 + i,
                "funding_tx_partial_signature": sig_bytes,
                "funding_nonce": nonce_json.clone(),
                "revocation_nonce": nonce_json.clone(),
            }))?;
            semantic_actions.push(InjectedWireAction::SendFrame(
                FiberMessage::commitment_signed_v2(forged_cs),
            ));
        }
        ensure!(semantic_actions.len() == SEMANTIC_FLOOD_TOTAL as usize);

        let res_before = self.runtime.sample_resources()?;
        let sent_before_semantic = self.runtime.links[&(0, 1)].injected_sent();
        let semantic_flood = Fault::FloodWire {
            from: 0,
            to: 1,
            actions: semantic_actions,
            allow_malformed_disconnect: false,
        }
        .inject(&mut self.runtime)?;

        self.until("120 invalid semantic frames transmitted to victim 1", |s| {
            Ok((s.runtime.links[&(0, 1)].injected_sent()
                >= sent_before_semantic + SEMANTIC_FLOOD_TOTAL)
                .then_some(()))
        })?;
        // Flush ch3's mailbox on victim 1 so every rejected frame has finished processing.
        self.exhaustion_flush_channel_mailbox(1, ch3)?;
        for _ in 0..10 {
            self.step()?;
        }
        semantic_flood.heal(&mut self.runtime);

        let res_after = self.runtime.sample_resources()?;
        let commits_before = res_before
            .sqlite_batch_commits
            .get(&1)
            .copied()
            .unwrap_or(0);
        let commits_after = res_after.sqlite_batch_commits.get(&1).copied().unwrap_or(0);
        let commits_delta = commits_after.saturating_sub(commits_before);
        let ops_before = res_before.sqlite_write_ops.get(&1).copied().unwrap_or(0);
        let ops_after = res_after.sqlite_write_ops.get(&1).copied().unwrap_or(0);
        let ops_delta = ops_after.saturating_sub(ops_before);
        self.runtime.note(format_args!(
            "Probe 3 semantic flood ({SEMANTIC_FLOOD_TOTAL} invalid frames): victim {} sqlite_batch_commits_delta={commits_delta} sqlite_write_ops_delta={ops_delta}",
            NAMES[1]
        ));
        if commits_delta >= 30 {
            let msg = format!(
                "BUG EXPOSED (Probe 3 SemanticFloodWriteAmplification): semantic invalid frame flood caused {commits_delta} SQLite batch commits (write_ops_delta={ops_delta}, limit < 30) for {SEMANTIC_FLOOD_TOTAL} rejected frames"
            );
            self.runtime.note(&msg);
            exposed_bugs.push(msg);
        } else {
            self.coverage
                .hit("exhaustion_semantic_flood_write_amplification_bounded");
        }

        // -------------------------------------------------------------------------
        // Probe 4: GlobalPendingOpenChannelExhaustion
        // -------------------------------------------------------------------------
        let attackers = [0usize, 2, 3, 4, 5];
        self.until(
            "all 5 attacker links and honest peer 6 link connected to victim 1",
            |s| {
                let connected = attackers
                    .into_iter()
                    .chain(std::iter::once(6usize))
                    .all(|peer| s.runtime.link(peer, 1));
                Ok(connected.then_some(()))
            },
        )?;

        let chain_hash = get_chain_hash();
        let commitment_delay_epoch =
            EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1).full_value();
        let mut flood_handles = Vec::with_capacity(attackers.len());
        let mut sent_baselines = Vec::with_capacity(attackers.len());

        for &attacker in &attackers {
            let mut open_actions = Vec::with_capacity(OPEN_CHANNEL_PER_ATTACKER);
            for i in 0..OPEN_CHANNEL_PER_ATTACKER {
                let mut cid_bytes = [0u8; 32];
                cid_bytes[0] = 0xe0;
                cid_bytes[1] = attacker as u8;
                cid_bytes[2] = i as u8;
                let open_msg = OpenChannelV2 {
                    chain_hash,
                    channel_id: Hash256::from(cid_bytes),
                    funding_udt_type_script: None,
                    funding_amount: FUNDING_AMOUNT,
                    shutdown_script: shutdown_script.clone(),
                    reserved_ckb_amount,
                    funding_fee_rate: DEFAULT_FEE_RATE,
                    commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
                    commitment_delay_epoch,
                    max_tlc_value_in_flight: FUNDING_AMOUNT,
                    max_tlc_number_in_flight: MAX_TLC_NUMBER_IN_FLIGHT,
                    funding_pubkey: synth_pubkey(0x11, attacker, i),
                    tlc_basepoint: synth_pubkey(0x22, attacker, i),
                    first_per_commitment_point: synth_pubkey(0x33, attacker, i),
                    second_per_commitment_point: synth_pubkey(0x44, attacker, i),
                    channel_announcement_nonce: None,
                    initial_commitment_nonce: valid_nonce.clone(),
                    channel_flags: ChannelFlags::empty(),
                    channel_features: 1,
                };
                open_actions.push(InjectedWireAction::SendFrame(
                    FiberMessage::open_channel_v2(open_msg),
                ));
            }
            let sent_before = self.runtime.links[&(attacker, 1)].injected_sent();
            let flood = Fault::FloodWire {
                from: attacker,
                to: 1,
                actions: open_actions,
                allow_malformed_disconnect: false,
            }
            .inject(&mut self.runtime)?;
            sent_baselines.push((attacker, sent_before));
            flood_handles.push(flood);
        }

        self.until(
            "all 100 OpenChannelV2 frames from 5 connected attackers delivered to victim 1",
            |s| {
                let all_sent = sent_baselines.iter().all(|&(attacker, baseline)| {
                    s.runtime.links[&(attacker, 1)].injected_sent()
                        >= baseline + OPEN_CHANNEL_PER_ATTACKER as u64
                });
                Ok(all_sent.then_some(()))
            },
        )?;
        for _ in 0..30 {
            self.step()?;
        }
        for flood in flood_handles {
            flood.heal(&mut self.runtime);
        }

        let pending_after_flood = self.runtime.nodes.borrow()[1].pending_channels.len();
        ensure!(
            attackers.iter().all(|&a| self.runtime.link(a, 1)),
            "attacker TCP links did not stay connected during Probe 4"
        );
        self.runtime.note(format_args!(
            "Probe 4: 5 connected attackers populated victim {} pending_channels={pending_after_flood}",
            NAMES[1]
        ));

        // Advance virtual clock past funding timeout and run maintenance while attackers stay
        // connected — stale unaccepted entries MUST NOT permanently DoS honest peer 6.
        self.advance_clock_by(EXTERNAL_FUNDING_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([1])?;
        for _ in 0..30 {
            self.step()?;
        }

        match self.open_between(6, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None) {
            Ok(honest_edge) => {
                self.wait_online(6, 1, &honest_edge.channel)?;
                self.coverage
                    .hit("exhaustion_global_pending_open_honest_peer_succeeded");
            }
            Err(err) => {
                let pending_now = self.runtime.nodes.borrow()[1].pending_channels.len();
                let msg = format!(
                    "BUG EXPOSED (Probe 4 GlobalPendingOpenChannelExhaustion): 5 connected peers exhausted global to_be_accepted_channels=100 (pending={pending_now}) and permanently blocked honest peer 6: {err}"
                );
                self.runtime.note(&msg);
                exposed_bugs.push(msg);
            }
        }

        if !exposed_bugs.is_empty() {
            bail!(
                "AdversarialExhaustionAndRaces exposed {} REAL BUG(s):\n- {}",
                exposed_bugs.len(),
                exposed_bugs.join("\n- ")
            );
        }

        LivenessOracle::assert_converged(&self.runtime, &[], &[])?;
        ConsistencyOracle::audit(&self.runtime, &[], &closed_channels)?;
        self.runtime.note(format_args!(
            "adversarial_exhaustion_and_races passed: {}",
            self.coverage.format_summary()
        ));
        Ok(())
    }

    /// Exercise aggressive Hosted LSP + external SDK tamper, slow-signer deadline race, eviction
    /// deadlock / `max_active_tenants` starvation, and watchtower extra-input rejection probes:
    /// - Probe A (`OpeningFundingOutputTamper`): tampered `unsigned_funding_tx` output rejected
    ///   with zero store mutation, followed by clean retry open.
    /// - Probe B (`SlowSignerCrossingSafetyDeadline`): `sign_delay = 200ms` racing across
    ///   `signature_tlc_deadline_reached(now)` against `force_close_expired_signature`.
    /// - Probe C (`ClosingOutputTamper` + `SendClosingSigned` Refusal Eviction Starvation +
    ///   `WatchtowerExtraInput`): closing output tamper rejected, refusing tenant eviction &
    ///   `max_active_tenants` starvation audited, LSP force-close recovery + `SDK-2` activation,
    ///   and `WatchtowerExtraInput` zero-mutation rejection followed by clean settlement.
    pub(super) fn lsp_adversarial_exhaustion(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_watchtowers([0, lsp_node])?;

        let public_edge =
            self.open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?;
        self.wait_trampoline_ready(0, lsp_node)?;
        for wallet in 0..3 {
            self.wait_wallet_online(wallet)?;
        }

        let tenant_0 = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        let tenant_1 = self.wallet_call(1, |reply| WalletCommand::Register { reply })?;
        let tenant_2 = self.wallet_call(2, |reply| WalletCommand::Register { reply })?;

        let mut tracked_payments: Vec<(Endpoint, Hash256)> = Vec::new();
        let mut tracked_deliveries: Vec<Hash256> = Vec::new();
        let mut audit_payments: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();
        let mut closed_channels: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();

        // -------------------------------------------------------------------------
        // Probe A: OpeningFundingOutputTamper + Clean Retry
        // -------------------------------------------------------------------------
        let prev_rejections_a = self.runtime.wallets.borrow()[0].sdk_rejections_verified;
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .lsp_tamper = Some(LspTamperKind::OpeningFundingOutputTamper);

        let tampered_open = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        });
        let open_err = match tampered_open {
            Ok(ch) => bail!(
                "ORACLE VIOLATION: SDK-0 unexpectedly accepted tampered OpeningFundingOutputTamper channel={ch}"
            ),
            Err(err) => err,
        };
        ensure!(
            open_err
                .to_string()
                .contains("SDK rejected tampered OpeningFundingOutputTamper"),
            "unexpected OpeningFundingOutputTamper error: {open_err}"
        );
        ensure!(
            self.runtime.wallets.borrow()[0].sdk_rejections_verified > prev_rejections_a,
            "SDK-0 did not increment sdk_rejections_verified for OpeningFundingOutputTamper"
        );
        ensure!(
            self.runtime.wallets.borrow()[0]
                .sdk_policy
                .borrow()
                .lsp_tamper
                .is_none(),
            "OpeningFundingOutputTamper was not consumed"
        );
        self.coverage
            .hit("exhaustion_opening_output_tamper_rejected");
        self.runtime.note(format_args!(
            "Probe A: SDK-0 rejected tampered OpeningFundingOutputTamper with zero store mutation ({open_err})"
        ));

        // Expire any orphaned external-funding draft left on tenant_0/LSP from the tampered RPC so
        // it does not linger in AwaitingSignature across subsequent probes and LivenessOracle.
        let orphaned_drafts: Vec<Hash256> = {
            let store = self.runtime.store(lsp_node)?;
            let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant_0.as_str()));
            tenant_store
                .get_all_channel_states()
                .into_iter()
                .filter(|st| !st.is_closed())
                .map(|st| st.get_id())
                .collect()
        };
        if !orphaned_drafts.is_empty() {
            self.advance_clock_by(EXTERNAL_FUNDING_TIMEOUT_ADVANCE_MS);
            self.runtime.crash(lsp_node);
            for _ in 0..4 {
                self.step()?;
            }
            self.runtime.restart(lsp_node);
            self.until(
                "LSP service online after cleaning orphaned tampered draft",
                |s| Ok(s.runtime.lsp_slot.borrow().is_some().then_some(())),
            )?;
            self.start_lsp_watchtower()?;
            self.until("Public A reconnects to restarted LSP", |s| {
                Ok(s.runtime.link(0, lsp_node).then_some(()))
            })?;
            let _ =
                self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_0.clone(), reply))?;
            for draft in orphaned_drafts {
                self.until(
                    "orphaned tampered opening draft cleaned up in tenant_0 store",
                    |s| {
                        Ok(s.tenant_channel_state(&tenant_0, draft)?
                            .is_none()
                            .then_some(()))
                    },
                )?;
                let mut lsp_cleaned = false;
                for _ in 0..200 {
                    if self.runtime.funding_record_absent(lsp_node, draft)? {
                        lsp_cleaned = true;
                        break;
                    }
                    self.step()?;
                }
                if !lsp_cleaned {
                    self.advance_clock_by(DEFAULT_FUNDING_TIMEOUT_ADVANCE_MS);
                    let _ = self.exhaustion_notify_channel_event(
                        lsp_node,
                        draft,
                        ChannelEvent::CheckFundingTimeout,
                    );
                    self.until(
                        "orphaned tampered opening draft cleaned up on LSP after funding timeout",
                        |s| {
                            Ok(s.runtime
                                .funding_record_absent(lsp_node, draft)?
                                .then_some(()))
                        },
                    )?;
                }
            }
            self.wait_online(0, lsp_node, &public_edge.channel)?;
        }

        let ch_0 = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_0, ch_0)?;
        self.runtime.note(format_args!(
            "Probe A passed: clean retry opened ch_0={ch_0} on SDK-0"
        ));

        // -------------------------------------------------------------------------
        // Probe B: SlowSignerCrossingSafetyDeadline
        // -------------------------------------------------------------------------
        let pre1 = Hash256::from([0xE1; 32]);
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

        self.until(
            "Probe B hold invoice p1 committed and Received on SDK-0",
            |s| {
                let received = s
                    .wallet_invoice_status_from_store(&tenant_0, p1)
                    .unwrap_or(None)
                    == Some(CkbInvoiceStatus::Received);
                let tenant_idle = s.tenant_channel_state(&tenant_0, ch_0)?.is_some_and(|st| {
                    !st.signing_context.is_awaiting_signature() && !st.tlc_state.waiting_ack
                });
                let lsp_idle = s
                    .runtime
                    .channel(lsp_node, &ch_0)
                    .is_some_and(|st| !st.tlc_state.waiting_ack);
                Ok((received && tenant_idle && lsp_idle).then_some(()))
            },
        )?;

        let (p1_expiry, ch0_delay_ms) = {
            let st = self
                .tenant_channel_state(&tenant_0, ch_0)?
                .context("missing tenant_0 ch_0 state")?;
            let expiry = st
                .tlc_state
                .all_tlcs()
                .find(|tlc| tlc.payment_hash == p1)
                .context("missing p1 TLC on tenant_0 ch_0")?
                .expiry;
            (expiry, tlc_expiry_delay_ms(st.commitment_delay_epoch))
        };

        // Arm slow signer delay (200ms) on SDK-0 and send a 2nd payment so SDK-0's delayed
        // submit_channel_signature races right across signature_tlc_deadline_reached(now).
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .sign_delay = Duration::from_millis(200);

        let pre2 = Hash256::from([0xE2; 32]);
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
            "Probe B ch_0 enters AwaitingSignature while SDK-0 has 200ms sign_delay",
            |s| {
                Ok(s.tenant_channel_state(&tenant_0, ch_0)?
                    .is_some_and(|st| st.signing_context.is_awaiting_signature())
                    .then_some(()))
            },
        )?;
        // Step ~50ms so SDK-0's 25ms poll picks up the request and enters its 200ms sign_delay.
        for _ in 0..10 {
            self.step()?;
        }

        // Advance virtual clock right across signature_tlc_deadline_reached(now) while SDK-0's
        // delayed submit_channel_signature is in flight, racing with force_close_expired_signature.
        let deadline_target = p1_expiry.saturating_sub(ch0_delay_ms / 2);
        let delta = deadline_target.saturating_sub(self.clock_ms()).max(1);
        self.advance_clock_by(delta);

        let tenant0_ctx =
            self.lsp_call(|reply| LspServiceMessage::GetTenantRpcContext(tenant_0.clone(), reply))?;
        // Let SDK-0's delayed submit_channel_signature fire into the crossed deadline, and trigger
        // periodic maintenance on both LSP and tenant so whichever path wins force-closes cleanly.
        self.trigger_channel_maintenance([0, lsp_node])?;
        tenant0_ctx.check_channels();

        let commit_0 = self.until(
            "Probe B ch_0 cleanly force-closes after slow-signer deadline race",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                    s.trigger_channel_maintenance([0, lsp_node])?;
                    tenant0_ctx.check_channels();
                }
                let lsp_closed = s.runtime.channel(lsp_node, &ch_0).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags)
                            if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                                || flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                });
                let tenant_closed = s.tenant_channel_state(&tenant_0, ch_0)?.is_some_and(|st| {
                    matches!(st.state, ChannelState::Closed(_))
                        && !st.signing_context.is_awaiting_signature()
                });
                Ok(tenant_closed.then_some(lsp_closed).flatten())
            },
        )?;
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .sign_delay = Duration::ZERO;
        self.coverage.hit("exhaustion_slow_signer_deadline_race");
        self.runtime.note(format_args!(
            "Probe B: ch_0={ch_0} force-closed commit={commit_0} during slow-signer deadline race"
        ));

        let max_expiry_0 = self
            .runtime
            .channel(lsp_node, &ch_0)
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
        self.advance_clock_past(max_expiry_0);

        self.until(
            "Probe B ch_0 settles on-chain and public payments p1/p2 reach Failed",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                    tenant0_ctx.check_channels_shutdown();
                }
                if s.runtime.elapsed().as_millis().is_multiple_of(250) {
                    s.trigger_channel_maintenance([0, lsp_node])?;
                    tenant0_ctx.check_channels();
                }
                let chain_settled = s.runtime.chain.commitment_settled(commit_0);
                let lsp_done = s
                    .runtime
                    .channel(lsp_node, &ch_0)
                    .is_some_and(|st| st.is_closed() && !st.is_waiting_onchain_settlement());
                let tenant_done = s.tenant_channel_state(&tenant_0, ch_0)?.is_some_and(|st| {
                    st.is_closed()
                        && !st.is_waiting_onchain_settlement()
                        && !st.signing_context.is_awaiting_signature()
                });
                let p1_failed = s.runtime.payment_status(0, p1) == Some(PaymentStatus::Failed);
                let p2_failed = s.runtime.payment_status(0, p2) == Some(PaymentStatus::Failed);
                let deliveries_final = s.exhaustion_lsp_deliveries_final(lsp_node, &[p1, p2])?;
                let pub_drained = [0, lsp_node].into_iter().all(|node| {
                    s.runtime
                        .channel(node, &public_edge.channel)
                        .is_some_and(|st| {
                            st.state == ChannelState::ChannelReady
                                && !st.tlc_state.waiting_ack
                                && !st.any_tlc_pending()
                        })
                });
                Ok((chain_settled
                    && lsp_done
                    && tenant_done
                    && p1_failed
                    && p2_failed
                    && deliveries_final
                    && pub_drained)
                    .then_some(()))
            },
        )?;
        self.wait_payment_actor(0)?;
        tracked_payments.push((Endpoint::Public(0), p1));
        tracked_payments.push((Endpoint::Public(0), p2));
        tracked_deliveries.push(p1);
        tracked_deliveries.push(p2);
        audit_payments.push((Endpoint::Public(0), Endpoint::Tenant(tenant_0.clone()), p1));
        audit_payments.push((Endpoint::Public(0), Endpoint::Tenant(tenant_0.clone()), p2));
        closed_channels.push((
            Endpoint::Public(lsp_node),
            Endpoint::Tenant(tenant_0.clone()),
            ch_0,
        ));
        self.runtime
            .note("Probe B passed: slow-signer deadline race converged cleanly");

        // -------------------------------------------------------------------------
        // Probe C: ClosingOutputTamper + SendClosingSigned Refusal Eviction Starvation
        //          + WatchtowerExtraInput Rejection & Clean Settlement
        // -------------------------------------------------------------------------
        let ch_0_fresh = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_0, ch_0_fresh)?;

        let ch_1 = self.wallet_call(1, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_1, ch_1)?;

        // Arm ClosingOutputTamper on SDK-1 and initiate cooperative close -> SDK-1 MUST reject
        // the tampered closing tx with zero store mutation.
        let prev_rejections_c = self.runtime.wallets.borrow()[1].sdk_rejections_verified;
        self.runtime.wallets.borrow()[1]
            .sdk_policy
            .borrow_mut()
            .lsp_tamper = Some(LspTamperKind::ClosingOutputTamper {
            drain_shannons: 100_000_000,
        });

        self.wallet_call(1, |reply| WalletCommand::CooperativeClose {
            channel_id: ch_1,
            fee_rate: 1_000,
            reply,
        })?;

        self.until(
            "SDK-1 rejects tampered ClosingOutputTamper with zero store mutation",
            |s| {
                Ok(
                    (s.runtime.wallets.borrow()[1].sdk_rejections_verified > prev_rejections_c)
                        .then_some(()),
                )
            },
        )?;
        ensure!(
            self.runtime.wallets.borrow()[1]
                .sdk_policy
                .borrow()
                .lsp_tamper
                .is_none(),
            "ClosingOutputTamper was not consumed on SDK-1"
        );
        self.coverage
            .hit("exhaustion_closing_output_tamper_rejected");

        // Immediately arm SendClosingSigned refusal before SDK-1's next poll tick so ch_1 stays
        // stuck in AwaitingSignature(SendClosingSigned).
        let prev_refusals_1 = self.runtime.wallets.borrow()[1].sdk_refusals_hit;
        self.runtime.wallets.borrow()[1]
            .sdk_policy
            .borrow_mut()
            .refusal = Some(RefusalTarget::Transition(
            ChannelSigningTransition::SendClosingSigned,
        ));

        self.until(
            "SDK-1 refuses SendClosingSigned and stays stuck in AwaitingSignature",
            |s| {
                let refused = s.runtime.wallets.borrow()[1].sdk_refusals_hit > prev_refusals_1;
                let tenant_awaiting = s
                    .tenant_channel_state(&tenant_1, ch_1)?
                    .is_some_and(|st| st.signing_context.is_awaiting_signature());
                let lsp_waiting =
                    s.runtime.channel(lsp_node, &ch_1).is_some_and(|st| {
                        matches!(st.state, ChannelState::ShuttingDown(_))
                            && st.session.closing.as_ref().is_some_and(|c| {
                                c.response.is_some() && c.remote_response.is_none()
                            })
                    });
                Ok((refused && tenant_awaiting && lsp_waiting).then_some(()))
            },
        )?;

        // With 2/2 max_active_tenants occupied (tenant_0 + refusing tenant_1), attempt EvictTenant
        // on refusing tenant_1 and EnsureTenant on honest SDK-2 to observe eviction/starvation.
        let evict_res = self.exhaustion_lsp_call_raw(|reply| {
            LspServiceMessage::EvictTenant(tenant_1.clone(), reply)
        })?;
        let ensure_sdk2_res = self.exhaustion_lsp_call_raw(|reply| {
            LspServiceMessage::EnsureTenant(tenant_2.clone(), reply)
        })?;
        match (&evict_res, &ensure_sdk2_res) {
            (Err(evict_err), Err(ensure_err)) => {
                self.coverage
                    .hit("exhaustion_lsp_refusal_blocks_eviction_and_starves_slot");
                self.runtime.note(format_args!(
                    "Probe C diagnostic: refusing tenant {} BLOCKED eviction ({evict_err}) and STARVED honest {} ({ensure_err})",
                    WALLET_HOSTS[1], WALLET_HOSTS[2]
                ));
            }
            (Ok(status), ensure_outcome) => {
                self.coverage
                    .hit("exhaustion_lsp_refusal_evicted_without_starvation");
                self.runtime.note(format_args!(
                    "Probe C diagnostic: refusing tenant {} evicted -> {:?}; honest {} EnsureTenant={ensure_outcome:?}",
                    WALLET_HOSTS[1], status.runtime_status, WALLET_HOSTS[2]
                ));
            }
            (Err(evict_err), Ok(_)) => {
                self.runtime.note(format_args!(
                    "Probe C diagnostic: refusing tenant {} blocked eviction ({evict_err}) yet {} activated unexpectedly",
                    WALLET_HOSTS[1], WALLET_HOSTS[2]
                ));
            }
        }

        // Advance past PEER_CHANNEL_RESPONSE_TIMEOUT so LSP autonomously escalates hung ch_1 to
        // force-close and frees tenant_1 once on-chain settlement completes.
        self.advance_clock_by(PEER_RESPONSE_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([lsp_node])?;

        let commit_1 = self.until(
            "LSP autonomously escalates refusing SDK-1 ch_1 to force-close",
            |s| {
                Ok(s.runtime.channel(lsp_node, &ch_1).and_then(|st| {
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
        self.runtime.wallets.borrow()[1]
            .sdk_policy
            .borrow_mut()
            .refusal = None;
        if ensure_sdk2_res.is_ok() {
            self.exhaustion_evict_tenant_when_idle(&tenant_2)?;
        }

        let tenant1_ctx =
            self.lsp_call(|reply| LspServiceMessage::GetTenantRpcContext(tenant_1.clone(), reply))?;
        self.advance_clock_past(self.clock_ms());
        self.until(
            "ch_1 force-close settles on-chain across both LSP and tenant_1",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                    tenant1_ctx.check_channels_shutdown();
                }
                let chain_settled = s.runtime.chain.commitment_settled(commit_1);
                let lsp_done = s
                    .runtime
                    .channel(lsp_node, &ch_1)
                    .is_some_and(|st| st.is_closed() && !st.is_waiting_onchain_settlement());
                let tenant_done = s.tenant_channel_state(&tenant_1, ch_1)?.is_some_and(|st| {
                    st.is_closed()
                        && !st.is_waiting_onchain_settlement()
                        && !st.signing_context.is_awaiting_signature()
                });
                Ok((chain_settled && lsp_done && tenant_done).then_some(()))
            },
        )?;
        closed_channels.push((
            Endpoint::Public(lsp_node),
            Endpoint::Tenant(tenant_1.clone()),
            ch_1,
        ));

        // Now evict idle tenant_1 and confirm honest SDK-2 can take the freed active slot.
        self.exhaustion_evict_tenant_when_idle(&tenant_1)?;
        let _ = self.lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_2.clone(), reply))?;
        self.runtime.note(format_args!(
            "Probe C: after ch_1 force-close & tenant_1 eviction, honest {} activated cleanly",
            WALLET_HOSTS[2]
        ));

        // Test WatchtowerExtraInput rejection + zero store mutation + clean watchtower settlement
        // on ch_0_fresh with an active hold TLC.
        let pre_wt = Hash256::from([0xE3; 32]);
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

        self.until("Probe C hold TLC p_wt committed on ch_0_fresh", |s| {
            let received = s
                .wallet_invoice_status_from_store(&tenant_0, p_wt)
                .unwrap_or(None)
                == Some(CkbInvoiceStatus::Received);
            let tenant_idle = s
                .tenant_channel_state(&tenant_0, ch_0_fresh)?
                .is_some_and(|st| {
                    !st.signing_context.is_awaiting_signature() && !st.tlc_state.waiting_ack
                });
            Ok((received && tenant_idle).then_some(()))
        })?;

        self.runtime.crash_wallet(0);
        let force_wt = self.runtime.request(lsp_node, |reply| {
            FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: ch_0_fresh,
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
        self.reply("LSP force-closes ch_0_fresh while SDK-0 offline", force_wt)?;

        let commit_wt = self.until("LSP force-close of ch_0_fresh confirmed on-chain", |s| {
            Ok(s.runtime.channel(lsp_node, &ch_0_fresh).and_then(|st| {
                matches!(
                    st.state,
                    ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                )
                .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                .flatten()
            }))
        })?;

        // Advance past 1-epoch commitment delay while keeping p_wt unexpired.
        self.advance_clock_past(self.clock_ms());
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;

        let prev_wt_rejections =
            self.runtime.wallets.borrow()[0].sdk_watchtower_rejections_verified;
        let prev_wt_sigs = self.runtime.wallets.borrow()[0].watchtower_sigs_applied;
        self.runtime.wallets.borrow()[0]
            .sdk_policy
            .borrow_mut()
            .lsp_tamper = Some(LspTamperKind::WatchtowerExtraInput);
        self.wallet_call(0, |reply| WalletCommand::CreateWatchPreimage {
            payment_hash: p_wt,
            preimage: pre_wt,
            reply,
        })?;

        self.until(
            "SDK-0 rejects WatchtowerExtraInput with zero store mutation",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                }
                Ok(
                    (s.runtime.wallets.borrow()[0].sdk_watchtower_rejections_verified
                        > prev_wt_rejections)
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
            "WatchtowerExtraInput was not consumed on SDK-0"
        );
        ensure!(
            self.runtime.wallets.borrow()[0].watchtower_sigs_applied == prev_wt_sigs,
            "tampered WatchtowerExtraInput unexpectedly resulted in a submitted watchtower signature"
        );
        self.coverage
            .hit("exhaustion_watchtower_extra_input_rejected");

        let tenant0_ctx_after =
            self.lsp_call(|reply| LspServiceMessage::GetTenantRpcContext(tenant_0.clone(), reply))?;
        self.until(
            "SDK-0 cleanly settles p_wt on-chain via watchtower after WatchtowerExtraInput rejection",
            |s| {
                if s.runtime.elapsed().as_millis().is_multiple_of(50) {
                    s.scan_onchain([lsp_node])?;
                    tenant0_ctx_after.check_channels_shutdown();
                }
                let settled = s.runtime.chain.commitment_settled(commit_wt);
                let payer_ok = s.runtime.payment_status(0, p_wt) == Some(PaymentStatus::Success);
                let signed = s.runtime.wallets.borrow()[0].watchtower_sigs_applied > prev_wt_sigs;
                let lsp_done = s
                    .runtime
                    .channel(lsp_node, &ch_0_fresh)
                    .is_some_and(|st| st.is_closed() && !st.is_waiting_onchain_settlement());
                let tenant_done = s
                    .tenant_channel_state(&tenant_0, ch_0_fresh)?
                    .is_some_and(|st| st.is_closed() && !st.is_waiting_onchain_settlement());
                let pub_drained = [0, lsp_node].into_iter().all(|node| {
                    s.runtime
                        .channel(node, &public_edge.channel)
                        .is_some_and(|st| {
                            !st.tlc_state.waiting_ack && st.tlc_state.all_tlcs().next().is_none()
                        })
                });
                let delivery_final = s.exhaustion_lsp_deliveries_final(lsp_node, &[p_wt])?;
                Ok((settled
                    && payer_ok
                    && signed
                    && lsp_done
                    && tenant_done
                    && pub_drained
                    && delivery_final)
                    .then_some(()))
            },
        )?;
        self.wait_payment_actor(0)?;
        self.wait_payment_actor(lsp_node)?;
        tracked_payments.push((Endpoint::Public(0), p_wt));
        tracked_deliveries.push(p_wt);
        closed_channels.push((
            Endpoint::Public(lsp_node),
            Endpoint::Tenant(tenant_0.clone()),
            ch_0_fresh,
        ));

        self.verify_lsp_indexes(&[0, lsp_node], &[tenant_0, tenant_1, tenant_2])?;
        LivenessOracle::assert_converged(&self.runtime, &tracked_payments, &tracked_deliveries)?;
        ConsistencyOracle::audit(&self.runtime, &audit_payments, &closed_channels)?;
        self.runtime.note(format_args!(
            "lsp_adversarial_exhaustion passed: {}",
            self.coverage.format_summary()
        ));
        Ok(())
    }

    fn exhaustion_notify_channel_event(
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

    fn exhaustion_flush_channel_mailbox(&mut self, node: usize, channel_id: Hash256) -> Result<()> {
        let mut actor_rx = self.runtime.request(node, |reply| {
            FiberActorCommand::GetChannelActor(channel_id, reply)
        })?;
        if let Some(actor) = self.until("lookup channel actor for barrier", |_| {
            receive(&mut actor_rx)
        })? {
            let (tx, mut rx) = oneshot::channel();
            actor
                .send_message(ChannelActorMessage::Command(ChannelCommand::TestBarrier(
                    tx.into(),
                )))
                .map_err(|err| anyhow!("send TestBarrier: {err}"))?;
            self.until("channel actor mailbox barrier", |_| receive(&mut rx))?;
        } else {
            for _ in 0..40 {
                self.step()?;
            }
        }
        Ok(())
    }

    fn exhaustion_lsp_call_raw<T>(
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

    fn exhaustion_evict_tenant_when_idle(&mut self, tenant_id: &TenantId) -> Result<()> {
        self.wait_tenant_idle(tenant_id)?;
        for _ in 0..50 {
            let status =
                self.lsp_call(|reply| LspServiceMessage::EvictTenant(tenant_id.clone(), reply))?;
            if status.runtime_status == TenantRuntimeStatus::Cold {
                return Ok(());
            }
            self.step()?;
        }
        bail!("timed out evicting hosted tenant {tenant_id}")
    }

    fn exhaustion_lsp_deliveries_final(&self, lsp_node: usize, hashes: &[Hash256]) -> Result<bool> {
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
