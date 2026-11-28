use anyhow::{anyhow, ensure, Context, Result};
use ckb_types::core::EpochNumberWithFraction;
use fiber_json_types::ListChannelsParams;
use fnn::fiber::channel::{
    DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE, MAX_TLC_NUMBER_IN_FLIGHT,
    MIN_COMMITMENT_DELAY_EPOCHS,
};
use fnn::fiber::network::get_chain_hash;
use fnn::fiber::payment::SendPaymentCommand;
use fnn::fiber::types::{
    AddTlc, BroadcastMessageWithTimestamp, ChannelReadyV2, CommitmentSignedV2, FiberMessage,
    OpenChannelV2, RevokeAndAckV2,
};
use fnn::fiber::{ChannelConnectivityState, ChannelState, FiberActorCommand, PaymentStatus};
use fnn::fiber_types::{
    AnnouncedNodeName, ChannelFlags, FeatureVector, Hash256, HashAlgorithm, NodeAnnouncement,
    Privkey, UdtCfgInfos,
};
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use fnn::lsp::{
    LspPaymentDelivery, LspPaymentDeliveryStatus, LspPaymentDeliveryStore, LspServiceMessage,
    TenantId, TenantRuntimeStatus,
};
use fnn::store::NodeNamespace;
use secp256k1::SECP256K1;
use serde_json::json;
use tokio::sync::oneshot;

use crate::host::{FrameKindFilter, FrameMatcher, InjectedWireAction, NAMES};
use crate::lsp::{RpcFloodKind, RpcFloodReport, RpcFloodSpec, WalletCommand, WALLET_HOSTS};
use crate::oracle::{ConsistencyOracle, Endpoint, LivenessOracle, ResourceLimits};

use super::faults::Fault;
use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const TENANT_FUNDING_AMOUNT: u128 = 50_000 * 100_000_000;
const P2P_OVERSIZED_FRAME_LEN: u32 = (1 << 20) + 128;
const LSP_OVERSIZED_RPC_LEN: u32 = (2 << 20) + 256;
const SIBLING_PAYMENT_STEP_BOUND: u64 = 1_500;
const OPEN_CHANNEL_FLOOD_COUNT: usize = 25;
const OPEN_CHANNEL_PER_PEER_CAP: usize = 20;
const GOSSIP_BURST_COUNT: usize = 64;
const BUFFER_FLOOD_INVOICES: usize = 8;

impl Driver {
    /// Exercise P2P frame/DoS floods (oversized/malformed raw frames, duplicate & invalid
    /// semantic frames, `OpenChannelV2` cap flood, and gossip burst) while verifying victim
    /// hub liveness, sibling non-starvation, and deterministic resource bounds.
    pub(super) fn peer_protocol_dos_and_flood(&mut self) -> Result<()> {
        if let Some(towers) = &self.runtime.watchtowers {
            for tower in towers {
                tower.enabled.send_replace(true);
            }
        }

        // Victim hub (1) initiates both channels so its `Slot::pending_channels` starts at 0
        // before Attack 3 floods inbound `OpenChannelV2` frames from attacker (0).
        let att_edge = self.public_edge(1, 0, FUNDING_AMOUNT, 0)?;
        let sib_edge = self.public_edge(1, 2, FUNDING_AMOUNT, 0)?;
        let ch_att = att_edge.channel;
        let ch_sib = sib_edge.channel;
        self.step()?;

        let res_before = self.runtime.sample_resources()?;
        let mut tracked_payments: Vec<(Endpoint, Hash256)> = Vec::new();
        let mut audited_payments: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();

        // Attack 1: Oversized + malformed raw frames from attacker 0 -> victim 1.
        // Deliver oversized first, then malformed after reconnect, so both wire guards fire
        // deterministically and disconnect attacker 0 without disturbing victim 1 or sibling 1-2.
        let prev_malformed = self.runtime.links[&(1, 0)].malformed_disconnects();
        let oversized = Fault::FloodWire {
            from: 0,
            to: 1,
            actions: vec![InjectedWireAction::SendRawFrame {
                declared_len: P2P_OVERSIZED_FRAME_LEN,
                payload: vec![0xab; 64],
            }],
            allow_malformed_disconnect: true,
        }
        .inject(&mut self.runtime)?;
        self.until("victim disconnects attacker on oversized raw frame", |s| {
            Ok((s.runtime.links[&(1, 0)].malformed_disconnects() > prev_malformed).then_some(()))
        })?;
        oversized.heal(&mut self.runtime);
        self.coverage.hit("dos_p2p_oversized_disconnect");

        self.until("attacker reconnects before malformed raw frame", |s| {
            Ok(s.runtime.link(0, 1).then_some(()))
        })?;
        let malformed = Fault::FloodWire {
            from: 0,
            to: 1,
            actions: vec![InjectedWireAction::SendRawFrame {
                declared_len: 16,
                payload: vec![0xff; 16],
            }],
            allow_malformed_disconnect: true,
        }
        .inject(&mut self.runtime)?;
        self.until("victim disconnects attacker on malformed raw frame", |s| {
            Ok(
                (s.runtime.links[&(1, 0)].malformed_disconnects() >= prev_malformed + 2)
                    .then_some(()),
            )
        })?;
        malformed.heal(&mut self.runtime);
        self.coverage.hit("dos_p2p_malformed_disconnect");

        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after raw frame DoS",
            NAMES[1]
        );
        let probe1 = self.assert_sibling_payment(&ch_sib, 500 * 100_000_000, 0x11)?;
        tracked_payments.push((Endpoint::Public(1), probe1));
        audited_payments.push((Endpoint::Public(1), Endpoint::Public(2), probe1));

        // Attack 2: Valid-channel duplicate frames + 40 invalid semantic frames on ch_att.
        // Arm DuplicatePeerFrame before reestablishment so ReestablishChannelV2 is duplicated,
        // then blast 40 invalid AddTlc / CommitmentSignedV2 / RevokeAndAckV2 frames.
        let dup_fault = Fault::DuplicatePeerFrame {
            from: 0,
            to: 1,
            matcher: FrameMatcher::channel(FrameKindFilter::Any, ch_att),
            copies: 3,
            max_triggers: Some(6),
        }
        .inject(&mut self.runtime)?;
        self.wait_online(0, 1, &ch_att)?;

        let att_before_0 = self
            .runtime
            .channel(0, &ch_att)
            .context("missing ch_att on attacker 0")?;
        let att_before_1 = self
            .runtime
            .channel(1, &ch_att)
            .context("missing ch_att on victim 1")?;
        let att_balances_before = (
            att_before_0.get_local_balance(),
            att_before_0.get_remote_balance(),
            att_before_1.get_local_balance(),
            att_before_1.get_remote_balance(),
        );
        let valid_nonce = att_before_0
            .session
            .remote_nonce
            .clone()
            .or_else(|| att_before_0.session.bootstrap_remote_nonce.clone())
            .context("missing valid PubNonce from ch_att")?;
        let shutdown_script = att_before_0.get_local_shutdown_script();
        let reserved_ckb_amount = att_before_0.local_reserved_ckb_amount;
        let nonce_json = serde_json::to_value(ChannelReadyV2 {
            channel_id: ch_att,
            next_commitment_number: 0,
            next_commitment_nonce: valid_nonce.clone(),
        })?["next_commitment_nonce"]
            .clone();

        let mut semantic_actions = Vec::with_capacity(40);
        for i in 0..15u64 {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0] = 0xa0;
            hash_bytes[1..9].copy_from_slice(&i.to_le_bytes());
            semantic_actions.push(InjectedWireAction::SendFrame(FiberMessage::add_tlc(
                AddTlc {
                    channel_id: ch_att,
                    tlc_id: 1_000 + i,
                    amount: 0,
                    payment_hash: Hash256::from(hash_bytes),
                    expiry: self.clock_ms().saturating_add(3_600_000),
                    hash_algorithm: HashAlgorithm::CkbHash,
                    onion_packet: None,
                },
            )));
        }
        for i in 0..15u64 {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0] = 0xa1;
            hash_bytes[1..9].copy_from_slice(&i.to_le_bytes());
            semantic_actions.push(InjectedWireAction::SendFrame(FiberMessage::add_tlc(
                AddTlc {
                    channel_id: ch_att,
                    tlc_id: 2_000 + i,
                    amount: 100_000_000,
                    payment_hash: Hash256::from(hash_bytes),
                    expiry: 1,
                    hash_algorithm: HashAlgorithm::CkbHash,
                    onion_packet: None,
                },
            )));
        }
        let sig_bytes = [0x11u8; 32];
        for i in 0..5u64 {
            let forged_cs: CommitmentSignedV2 = serde_json::from_value(json!({
                "channel_id": ch_att,
                "commitment_number": 9_999u64 + i,
                "funding_tx_partial_signature": sig_bytes,
                "funding_nonce": nonce_json.clone(),
                "revocation_nonce": nonce_json.clone(),
            }))?;
            semantic_actions.push(InjectedWireAction::SendFrame(
                FiberMessage::commitment_signed_v2(forged_cs),
            ));
        }
        for i in 0..5u64 {
            let forged_ack: RevokeAndAckV2 = serde_json::from_value(json!({
                "channel_id": ch_att,
                "commitment_number": 9_999u64 + i,
                "revocation_partial_signature": sig_bytes,
                "revocation_nonce": nonce_json.clone(),
                "next_per_commitment_point": Privkey::from([0x22u8; 32]).pubkey(),
                "next_commitment_nonce": nonce_json.clone(),
            }))?;
            semantic_actions.push(InjectedWireAction::SendFrame(
                FiberMessage::revoke_and_ack_v2(forged_ack),
            ));
        }
        ensure!(semantic_actions.len() == 40);

        let sent_before_semantic = self.runtime.links[&(0, 1)].injected_sent();
        let semantic_flood = Fault::FloodWire {
            from: 0,
            to: 1,
            actions: semantic_actions,
            allow_malformed_disconnect: false,
        }
        .inject(&mut self.runtime)?;
        self.until("40 invalid semantic frames transmitted to victim", |s| {
            Ok(
                (s.runtime.links[&(0, 1)].injected_sent() >= sent_before_semantic + 40)
                    .then_some(()),
            )
        })?;
        for _ in 0..20 {
            self.step()?;
        }
        semantic_flood.heal(&mut self.runtime);
        dup_fault.heal(&mut self.runtime);
        self.coverage.hit("dos_p2p_semantic_flood");

        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after semantic frame flood",
            NAMES[1]
        );
        let att_after_0 = self
            .runtime
            .channel(0, &ch_att)
            .context("missing ch_att on attacker 0 after semantic flood")?;
        let att_after_1 = self
            .runtime
            .channel(1, &ch_att)
            .context("missing ch_att on victim 1 after semantic flood")?;
        let att_balances_after = (
            att_after_0.get_local_balance(),
            att_after_0.get_remote_balance(),
            att_after_1.get_local_balance(),
            att_after_1.get_remote_balance(),
        );
        ensure!(
            att_balances_before == att_balances_after,
            "ch_att balances mutated under invalid semantic flood: before={att_balances_before:?} after={att_balances_after:?}"
        );
        ensure!(
            att_after_1.tlc_state.all_tlcs().next().is_none(),
            "victim ch_att accepted invalid TLCs from semantic flood"
        );
        let probe2 = self.assert_sibling_payment(&ch_sib, 400 * 100_000_000, 0x12)?;
        tracked_payments.push((Endpoint::Public(1), probe2));
        audited_payments.push((Endpoint::Public(1), Endpoint::Public(2), probe2));

        // Attack 3: OpenChannelV2 flood (25 distinct temp channels > per-peer cap of 20).
        let chain_hash = get_chain_hash();
        let commitment_delay_epoch =
            EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1).full_value();
        let mut open_actions = Vec::with_capacity(OPEN_CHANNEL_FLOOD_COUNT);
        for i in 0..OPEN_CHANNEL_FLOOD_COUNT {
            let tag = (i + 1) as u8;
            let mut cid_bytes = [0u8; 32];
            cid_bytes[0] = 0xd0;
            cid_bytes[1] = tag;
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
                funding_pubkey: Privkey::from([0x30u8.wrapping_add(tag); 32]).pubkey(),
                tlc_basepoint: Privkey::from([0x50u8.wrapping_add(tag); 32]).pubkey(),
                first_per_commitment_point: Privkey::from([0x70u8.wrapping_add(tag); 32]).pubkey(),
                second_per_commitment_point: Privkey::from([0x90u8.wrapping_add(tag); 32]).pubkey(),
                channel_announcement_nonce: None,
                initial_commitment_nonce: valid_nonce.clone(),
                channel_flags: ChannelFlags::empty(),
                channel_features: 1,
            };
            open_actions.push(InjectedWireAction::SendFrame(
                FiberMessage::open_channel_v2(open_msg),
            ));
        }
        let sent_before_open = self.runtime.links[&(0, 1)].injected_sent();
        let open_flood = Fault::FloodWire {
            from: 0,
            to: 1,
            actions: open_actions,
            allow_malformed_disconnect: false,
        }
        .inject(&mut self.runtime)?;
        self.until(
            "25 OpenChannelV2 flood frames sent and victim pending cap reached",
            |s| {
                let sent_done = s.runtime.links[&(0, 1)].injected_sent()
                    >= sent_before_open + OPEN_CHANNEL_FLOOD_COUNT as u64;
                let cap_reached =
                    s.runtime.nodes.borrow()[1].pending_channels.len() >= OPEN_CHANNEL_PER_PEER_CAP;
                Ok((sent_done && cap_reached).then_some(()))
            },
        )?;
        for _ in 0..20 {
            self.step()?;
        }
        open_flood.heal(&mut self.runtime);
        let victim_pending = self.runtime.nodes.borrow()[1].pending_channels.len();
        ensure!(
            victim_pending > 0 && victim_pending <= OPEN_CHANNEL_PER_PEER_CAP,
            "victim pending_channels={victim_pending} violated per-peer cap {OPEN_CHANNEL_PER_PEER_CAP}"
        );
        self.coverage.hit("dos_p2p_open_channel_cap");
        self.runtime.note(format_args!(
            "victim {} enforced to_be_accepted_channels cap: pending={victim_pending}/{OPEN_CHANNEL_FLOOD_COUNT}",
            NAMES[1]
        ));

        // Disconnect attacker 0-1 and verify honest sibling 2 can still open a fresh channel with 1.
        self.runtime.reset_link(0, 1);
        let sib_edge_2 = self.public_edge(2, 1, FUNDING_AMOUNT, 0)?;
        self.wait_online(2, 1, &sib_edge_2.channel)?;
        self.wait_online(0, 1, &ch_att)?;
        self.runtime.note(format_args!(
            "honest {} opened fresh channel {} with victim {} despite attacker OpenChannelV2 flood",
            NAMES[2], sib_edge_2.channel, NAMES[1]
        ));

        // Attack 4: Gossip burst (64 synthetic signed NodeAnnouncements) + Resource Bound Audit.
        let now_ms = self.clock_ms();
        let mut gossip_burst = Vec::with_capacity(GOSSIP_BURST_COUNT);
        for i in 0..GOSSIP_BURST_COUNT {
            let mut key_bytes = [0u8; 32];
            key_bytes[0] = 0xe0;
            key_bytes[1] = (i % 16) as u8 + 1;
            let key = Privkey::from(key_bytes);
            let mut name_bytes = [0u8; 32];
            name_bytes[..4].copy_from_slice(b"dos-");
            name_bytes[4] = b'0' + (i % 10) as u8;
            let ann = NodeAnnouncement::new_signed(
                AnnouncedNodeName(name_bytes),
                FeatureVector::default(),
                Vec::new(),
                &key,
                chain_hash,
                now_ms.saturating_add(i as u64 + 1),
                0,
                UdtCfgInfos(Vec::new()),
                "0.0.0-dos".to_owned(),
            );
            gossip_burst.push(BroadcastMessageWithTimestamp::NodeAnnouncement(ann));
        }
        self.runtime.inject_gossip(&[1], gossip_burst)?;
        for _ in 0..20 {
            self.step()?;
        }
        self.coverage.hit("dos_p2p_gossip_burst");

        let probe3 = self.assert_sibling_payment(&ch_sib, 300 * 100_000_000, 0x13)?;
        tracked_payments.push((Endpoint::Public(1), probe3));
        audited_payments.push((Endpoint::Public(1), Endpoint::Public(2), probe3));

        let res_after = self.runtime.sample_resources()?;
        ResourceLimits {
            max_steps: Some(15_000),
            max_open_channels: Some(4),
            max_pending_channels: Some(OPEN_CHANNEL_PER_PEER_CAP + 4),
            max_actor_children: Some(32),
            max_store_row_growth: Some(2_000),
            max_store_byte_growth: Some(16 * 1024 * 1024),
            ..Default::default()
        }
        .check(&res_before, &res_after)?;

        LivenessOracle::assert_converged(&self.runtime, &tracked_payments, &[])?;
        ConsistencyOracle::audit(&self.runtime, &audited_payments, &[])?;
        self.runtime.note("peer_protocol_dos_and_flood passed");
        Ok(())
    }

    /// Exercise Hosted LSP JSON-RPC floods (malformed JSON, oversized frames, revoked Biscuit
    /// token storm, replayed registration `request_id` storm) and offline deferred delivery
    /// buffer flooding while proving honest tenant non-starvation and bounded resources.
    pub(super) fn lsp_rpc_and_buffer_dos(&mut self) -> Result<()> {
        let lsp_node = self.lsp_node()?;
        self.start_lsp_watchtower()?;
        let _public_edge =
            self.open_between(0, lsp_node, FUNDING_AMOUNT, 0, true, OpeningFault::None)?;
        self.wait_trampoline_ready(0, lsp_node)?;
        for wallet in 0..3 {
            self.wait_wallet_online(wallet)?;
        }

        let tenant_0 = self.wallet_call(0, |reply| WalletCommand::Register { reply })?;
        let tenant_1 = self.wallet_call(1, |reply| WalletCommand::Register { reply })?;
        let tenant_2 = self.wallet_call(2, |reply| WalletCommand::Register { reply })?;

        let ch_0 = self.wallet_call(0, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_0, ch_0)?;

        let ch_1 = self.wallet_call(1, |reply| WalletCommand::OpenChannel {
            funding_amount: TENANT_FUNDING_AMOUNT,
            reply,
        })?;
        self.wait_tenant_channel_online(&tenant_1, ch_1)?;

        // Confirm max_active_tenants=2 is actively enforced while SDK-2 stays cold.
        let slot_limit_res = self
            .dos_lsp_call_raw(|reply| LspServiceMessage::EnsureTenant(tenant_2.clone(), reply))?;
        ensure!(
            slot_limit_res.is_err(),
            "max_active_tenants=2 unexpectedly allowed activating cold tenant SDK-2"
        );

        let res_before = self.runtime.sample_resources_exact()?;
        let mut tracked_payments: Vec<(Endpoint, Hash256)> = Vec::new();
        let mut audited_payments: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();
        let mut tracked_deliveries: Vec<Hash256> = Vec::new();

        // Attack 1: Malformed JSON + oversized RPC frame flood from SDK-0.
        let malformed_report: RpcFloodReport =
            self.wallet_call(0, |reply| WalletCommand::RpcFlood {
                spec: RpcFloodSpec {
                    concurrency: 8,
                    total_requests: 40,
                    kind: RpcFloodKind::MalformedJson {
                        raw_payload: "{malformed-json-rpc-dos".to_owned(),
                    },
                },
                reply,
            })?;
        ensure!(
            malformed_report.sent == 40
                && malformed_report.ok == 0
                && (malformed_report.rpc_errors + malformed_report.transport_errors)
                    == malformed_report.sent,
            "malformed JSON flood unexpected report: {malformed_report:?}"
        );
        self.coverage.hit("dos_lsp_malformed_json");

        let oversized_report: RpcFloodReport =
            self.wallet_call(0, |reply| WalletCommand::RpcFlood {
                spec: RpcFloodSpec {
                    concurrency: 4,
                    total_requests: 16,
                    kind: RpcFloodKind::OversizedFrame {
                        declared_len: LSP_OVERSIZED_RPC_LEN,
                        payload_len: 128,
                    },
                },
                reply,
            })?;
        ensure!(
            oversized_report.sent == 16
                && oversized_report.ok == 0
                && (oversized_report.rpc_errors + oversized_report.transport_errors)
                    == oversized_report.sent,
            "oversized RPC frame flood unexpected report: {oversized_report:?}"
        );
        self.coverage.hit("dos_lsp_oversized_frame");

        ensure!(
            self.runtime.node_available(lsp_node) && self.runtime.lsp_slot.borrow().is_some(),
            "LSP went offline under malformed/oversized RPC flood"
        );
        let p_honest_1 =
            self.settle_inbound_tenant_payment(1, &tenant_1, 500 * 100_000_000, 0x21)?;
        tracked_payments.push((Endpoint::Public(0), p_honest_1));
        audited_payments.push((
            Endpoint::Public(0),
            Endpoint::Tenant(tenant_1.clone()),
            p_honest_1,
        ));
        tracked_deliveries.push(p_honest_1);

        // Attack 2: Revoked Biscuit token storm + replayed registration request_id storm.
        self.wait_tenant_idle(&tenant_0)?;
        let revoked_token = self
            .wallet_call(0, |reply| WalletCommand::CurrentToken { reply })?
            .context("SDK-0 missing current Biscuit token")?;
        let prev_refreshes = self.runtime.wallets.borrow()[0].token_refreshes;
        // Pause SDK-0 background polling during the storm so we cleanly observe the
        // automatic post-storm token refresh once polling resumes.
        self.runtime.wallets.borrow()[0].signing_paused.set(true);

        let new_epoch = self
            .dos_lsp_call(|reply| LspServiceMessage::RevokeTenantTokens(tenant_0.clone(), reply))?;
        self.runtime.note(format_args!(
            "revoked {} Biscuit tokens -> epoch={new_epoch}",
            WALLET_HOSTS[0]
        ));

        let list_params = ListChannelsParams {
            pubkey: None,
            include_closed: Some(true),
            only_pending: None,
        };
        let revoked_report: RpcFloodReport =
            self.wallet_call(0, |reply| WalletCommand::RpcFlood {
                spec: RpcFloodSpec {
                    concurrency: 8,
                    total_requests: 40,
                    kind: RpcFloodKind::RevokedToken {
                        token: revoked_token,
                        method: "list_channels",
                        params: json!([list_params]),
                    },
                },
                reply,
            })?;
        ensure!(
            revoked_report.sent == 40
                && revoked_report.ok == 0
                && (revoked_report.rpc_errors + revoked_report.transport_errors)
                    == revoked_report.sent,
            "revoked Biscuit token storm unexpectedly succeeded: {revoked_report:?}"
        );
        self.coverage.hit("dos_lsp_revoked_token_storm");

        let replay_report: RpcFloodReport =
            self.wallet_call(0, |reply| WalletCommand::RpcFlood {
                spec: RpcFloodSpec {
                    concurrency: 8,
                    total_requests: 24,
                    kind: RpcFloodKind::ReplayedLastRegistration,
                },
                reply,
            })?;
        ensure!(
            replay_report.sent == 24
                && replay_report.ok == 0
                && (replay_report.rpc_errors + replay_report.transport_errors)
                    == replay_report.sent,
            "replayed registration request_id storm unexpectedly succeeded: {replay_report:?}"
        );
        self.coverage.hit("dos_lsp_replayed_registration_storm");

        self.runtime.wallets.borrow()[0].signing_paused.set(false);
        self.until("SDK-0 auto-refreshes revoked Biscuit token", |s| {
            Ok((s.runtime.wallets.borrow()[0].token_refreshes > prev_refreshes).then_some(()))
        })?;
        let p_sdk0_recovered =
            self.settle_inbound_tenant_payment(0, &tenant_0, 400 * 100_000_000, 0x22)?;
        tracked_payments.push((Endpoint::Public(0), p_sdk0_recovered));
        audited_payments.push((
            Endpoint::Public(0),
            Endpoint::Tenant(tenant_0.clone()),
            p_sdk0_recovered,
        ));
        tracked_deliveries.push(p_sdk0_recovered);

        // Attack 3: Offline deferred delivery buffer flood + honest tenant non-starvation.
        self.wait_tenant_idle(&tenant_0)?;
        let mut buffered_invoices = Vec::with_capacity(BUFFER_FLOOD_INVOICES);
        for i in 0..BUFFER_FLOOD_INVOICES {
            let mut pre_bytes = [0u8; 32];
            pre_bytes[0] = 0xb0;
            pre_bytes[1] = (i + 1) as u8;
            let preimage = Hash256::from(pre_bytes);
            let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
            let inv = self.wallet_call(0, |reply| WalletCommand::CreateHoldInvoice {
                amount: 200 * 100_000_000,
                payment_hash,
                preimage,
                reply,
            })?;
            buffered_invoices.push((payment_hash, inv.invoice_address));
        }

        self.runtime.crash_wallet(0);
        self.dos_evict_tenant_when_idle(&tenant_0)?;

        for (payment_hash, invoice) in &buffered_invoices {
            let sent = self.pay(
                0,
                SendPaymentCommand {
                    invoice: Some(invoice.clone()),
                    ..Default::default()
                },
            )?;
            ensure!(sent == *payment_hash);
        }

        self.until(
            "all 8 offline SDK-0 deliveries buffered as Deferred on LSP",
            |s| {
                let all_deferred = buffered_invoices.iter().all(|(hash, _)| {
                    s.dos_lsp_delivery_now(*hash)
                        .is_some_and(|d| matches!(d.status, LspPaymentDeliveryStatus::Deferred))
                });
                Ok(all_deferred.then_some(()))
            },
        )?;
        let res_buffered = self.runtime.sample_resources()?;
        ensure!(
            res_buffered.lsp_pending_deliveries > 0 && res_buffered.lsp_pending_deliveries <= 64,
            "unexpected lsp_pending_deliveries={} during offline buffer flood",
            res_buffered.lsp_pending_deliveries
        );
        self.coverage.hit("dos_lsp_offline_buffer_flood");

        let max_deadline = buffered_invoices
            .iter()
            .filter_map(|(hash, _)| self.dos_lsp_delivery_now(*hash).map(|d| d.buffer_deadline))
            .max()
            .context("missing buffer_deadline on Deferred deliveries")?;

        // While SDK-0's 8 deliveries sit buffered, honest online SDK-1 must settle promptly.
        let p_honest_2 =
            self.settle_inbound_tenant_payment(1, &tenant_1, 350 * 100_000_000, 0x23)?;
        tracked_payments.push((Endpoint::Public(0), p_honest_2));
        audited_payments.push((
            Endpoint::Public(0),
            Endpoint::Tenant(tenant_1.clone()),
            p_honest_2,
        ));
        tracked_deliveries.push(p_honest_2);

        // Advance virtual clock past buffer_deadline and assert all 8 expire cleanly.
        self.advance_clock_past(max_deadline);
        self.until(
            "all 8 buffered SDK-0 deliveries expire cleanly on payer 0 and LSP",
            |s| {
                let all_failed = buffered_invoices.iter().all(|(hash, _)| {
                    let payer_failed =
                        s.runtime.payment_status(0, *hash) == Some(PaymentStatus::Failed);
                    let delivery_failed = s.dos_lsp_delivery_now(*hash).is_some_and(|d| {
                        matches!(d.status, LspPaymentDeliveryStatus::Failed { .. })
                    });
                    payer_failed && delivery_failed
                });
                Ok(all_failed.then_some(()))
            },
        )?;
        self.wait_payment_actor(0)?;
        for &(hash, _) in &buffered_invoices {
            tracked_payments.push((Endpoint::Public(0), hash));
            audited_payments.push((
                Endpoint::Public(0),
                Endpoint::Tenant(tenant_0.clone()),
                hash,
            ));
            tracked_deliveries.push(hash);
        }

        // Bring SDK-0 back online, ensure active, and run full resource/liveness/consistency audits.
        self.runtime.restart_wallet(0);
        self.wait_wallet_online(0)?;
        let _ =
            self.dos_lsp_call(|reply| LspServiceMessage::EnsureTenant(tenant_0.clone(), reply))?;
        self.wait_tenant_channel_online(&tenant_0, ch_0)?;
        self.wait_tenant_idle(&tenant_0)?;
        self.wait_tenant_idle(&tenant_1)?;

        let res_after = self.runtime.sample_resources_exact()?;
        ResourceLimits {
            max_steps: Some(15_000),
            max_lsp_pending_deliveries: Some(0),
            max_lsp_registered_tenants: Some(3),
            max_lsp_active_tenants: Some(2),
            max_store_row_growth: Some(2_000),
            max_store_byte_growth: Some(16 * 1024 * 1024),
            ..Default::default()
        }
        .check(&res_before, &res_after)?;

        LivenessOracle::assert_converged(&self.runtime, &tracked_payments, &tracked_deliveries)?;
        ConsistencyOracle::audit(&self.runtime, &audited_payments, &[])?;
        self.verify_lsp_indexes(&[0, lsp_node], &[tenant_0, tenant_1, tenant_2])?;
        self.runtime.note("lsp_rpc_and_buffer_dos passed");
        Ok(())
    }

    fn assert_sibling_payment(
        &mut self,
        sib_channel: &Hash256,
        amount: u128,
        tag: u8,
    ) -> Result<Hash256> {
        let start_steps = self.runtime.step_count;
        let preimage = Hash256::from([tag; 32]);
        let payee_key = Privkey::from([3; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_preimage(preimage)
            .payment_secret(Hash256::from([tag.wrapping_add(0x40); 32]))
            .payee_pub_key(self.runtime.peers[2].into())
            .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0))?;
        let payment_hash = *invoice.payment_hash();
        let invoice_str = invoice.to_string();
        let mut add_reply = self.runtime.request(2, |reply| {
            FiberActorCommand::AddInvoice(invoice, Some(preimage), reply)
        })?;
        self.until("sibling probe invoice added on honest node 2", |_| {
            receive(&mut add_reply)?.transpose().map_err(Into::into)
        })?;

        let sent = self.pay(
            1,
            SendPaymentCommand {
                invoice: Some(invoice_str),
                ..Default::default()
            },
        )?;
        ensure!(sent == payment_hash);

        self.until("sibling 1-2 probe payment settles cleanly", |s| {
            let status_ok =
                s.runtime.payment_status(1, payment_hash) == Some(PaymentStatus::Success);
            let paid_ok = s.runtime.store(2)?.get_invoice_status(&payment_hash)
                == Some(CkbInvoiceStatus::Paid);
            let drained = s.runtime.link(1, 2)
                && [1, 2].into_iter().all(|node| {
                    s.runtime.channel(node, sib_channel).is_some_and(|state| {
                        state.state == ChannelState::ChannelReady
                            && state.connectivity_state == ChannelConnectivityState::Online
                            && !state.tlc_state.waiting_ack
                            && state.tlc_state.all_tlcs().next().is_none()
                    })
                });
            Ok((status_ok && paid_ok && drained).then_some(()))
        })?;
        self.wait_payment_actor(1)?;

        let elapsed_steps = self.runtime.step_count.saturating_sub(start_steps);
        ensure!(
            elapsed_steps <= SIBLING_PAYMENT_STEP_BOUND,
            "sibling probe payment {payment_hash} starved: took {elapsed_steps} steps (limit {SIBLING_PAYMENT_STEP_BOUND})"
        );
        self.runtime.note(format_args!(
            "sibling 1->2 probe payment {payment_hash} settled in {elapsed_steps} steps"
        ));
        Ok(payment_hash)
    }

    fn settle_inbound_tenant_payment(
        &mut self,
        wallet: usize,
        tenant_id: &TenantId,
        amount: u128,
        tag: u8,
    ) -> Result<Hash256> {
        let start_steps = self.runtime.step_count;
        let preimage = Hash256::from([tag; 32]);
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
        let inv = self.wallet_call(wallet, |reply| WalletCommand::CreateHoldInvoice {
            amount,
            payment_hash,
            preimage,
            reply,
        })?;
        let sent = self.pay(
            0,
            SendPaymentCommand {
                invoice: Some(inv.invoice_address),
                ..Default::default()
            },
        )?;
        ensure!(sent == payment_hash);
        self.until("inbound tenant hold invoice received", |s| {
            let status = s
                .wallet_invoice_status_from_store(tenant_id, payment_hash)
                .unwrap_or(None);
            Ok((status == Some(CkbInvoiceStatus::Received)).then_some(()))
        })?;
        self.wallet_call(wallet, |reply| WalletCommand::SettleInvoice {
            payment_hash,
            preimage,
            reply,
        })?;
        self.until("inbound tenant payment settled cleanly", |s| {
            let payer_ok =
                s.runtime.payment_status(0, payment_hash) == Some(PaymentStatus::Success);
            let delivery_ok = s
                .dos_lsp_delivery_now(payment_hash)
                .is_some_and(|d| matches!(d.status, LspPaymentDeliveryStatus::Succeeded));
            let paid_ok = s
                .wallet_invoice_status_from_store(tenant_id, payment_hash)
                .unwrap_or(None)
                == Some(CkbInvoiceStatus::Paid);
            Ok((payer_ok && delivery_ok && paid_ok).then_some(()))
        })?;
        self.wait_payment_actor(0)?;
        self.wait_tenant_idle(tenant_id)?;

        let elapsed_steps = self.runtime.step_count.saturating_sub(start_steps);
        ensure!(
            elapsed_steps <= SIBLING_PAYMENT_STEP_BOUND,
            "inbound payment {payment_hash} to {} starved: took {elapsed_steps} steps (limit {SIBLING_PAYMENT_STEP_BOUND})",
            WALLET_HOSTS[wallet]
        );
        self.runtime.note(format_args!(
            "inbound payment {payment_hash} to {} settled in {elapsed_steps} steps",
            WALLET_HOSTS[wallet]
        ));
        Ok(payment_hash)
    }

    fn dos_lsp_call<T>(
        &mut self,
        command: impl FnOnce(ractor::RpcReplyPort<Result<T, String>>) -> LspServiceMessage,
    ) -> Result<T> {
        self.dos_lsp_call_raw(command)?.map_err(|err| anyhow!(err))
    }

    fn dos_lsp_call_raw<T>(
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

    fn dos_lsp_delivery_now(&self, payment_hash: Hash256) -> Option<LspPaymentDelivery> {
        let lsp_node = self.lsp_node().ok()?;
        let store = self.runtime.store(lsp_node).ok()?;
        let lsp_store = store.namespaced(NodeNamespace::lsp_metadata());
        let mut list = lsp_store
            .list_lsp_payment_deliveries_by_payment_hash(&payment_hash)
            .ok()?;
        list.sort_by_key(|d| (!d.status.is_final(), d.updated_at));
        list.pop()
    }

    fn dos_evict_tenant_when_idle(&mut self, tenant_id: &TenantId) -> Result<()> {
        self.wait_tenant_idle(tenant_id)?;
        for _ in 0..50 {
            let status = self
                .dos_lsp_call(|reply| LspServiceMessage::EvictTenant(tenant_id.clone(), reply))?;
            if status.runtime_status == TenantRuntimeStatus::Cold {
                return Ok(());
            }
            self.step()?;
        }
        anyhow::bail!("timed out evicting hosted tenant {tenant_id}")
    }
}
