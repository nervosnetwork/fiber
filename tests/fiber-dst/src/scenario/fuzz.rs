use anyhow::{ensure, Context, Result};
use ckb_types::core::EpochNumberWithFraction;
use ckb_types::packed::{Byte, OutPoint, Script, Transaction};
use ckb_types::prelude::{Builder, Entity, Pack, Unpack};
use fnn::fiber::channel::{
    ChannelCommand, ChannelCommandWithId, ChannelEvent, ShutdownCommand,
    DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE, MAX_TLC_NUMBER_IN_FLIGHT,
    MIN_COMMITMENT_DELAY_EPOCHS,
};
use fnn::fiber::config::DEFAULT_FINAL_TLC_EXPIRY_DELTA;
use fnn::fiber::network::{get_chain_hash, AcceptChannelCommand};
use fnn::fiber::payment::{SendPaymentCommand, SendPaymentWithRouterCommand};
use fnn::fiber::types::{
    AcceptChannelV2, AddTlc, ChannelReadyV2, ClosingSignedV2, CommitmentSignedV2,
    FiberChannelMessage, FiberMessage, OpenChannelV2, ReestablishChannelV2, RemoveTlc,
    RevokeAndAckV2, ShutdownV2, TxAbort, TxCompleteV2, TxSignatures, TxUpdate, UpdateTlcInfo,
};
use fnn::fiber::{
    ChannelConnectivityState, ChannelState, FiberActorCommand, Hash256, NetworkActorMessage,
    PaymentStatus, Privkey, Pubkey,
};
use fnn::fiber_types::{
    ChannelFlags, ChannelUpdateChannelFlags, CloseFlags, HashAlgorithm, PaymentOnionPacket,
    RemoveTlcFulfill, RemoveTlcReason, RouterHop, TlcErrPacket,
};
use fnn::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore};
use rand::{rngs::StdRng, Rng, SeedableRng};
use secp256k1::SECP256K1;
use serde_json::{json, Value};

use crate::host::{send, FrameKindFilter, FrameMatcher, InjectedWireAction, NAMES};
use crate::oracle::{ConsistencyOracle, Endpoint, LivenessOracle};

use super::faults::Fault;
use super::funding::OpeningFault;
use super::{receive, Driver, FUNDING_AMOUNT};

const FUZZ_BURST_TOTAL: usize = 100;
const FUZZ_BURST_SPLIT: usize = 50;
const PEER_RESPONSE_TIMEOUT_ADVANCE_MS: u64 = 35_000;
const REESTABLISH_TIMEOUT_ADVANCE_MS: u64 = 310_000;
const RECOVERY_WAIT_STEPS: usize = 300;
const CANARY_STEP_BOUND: u64 = 2_000;

/// Largest valid secp256k1 scalar (`n - 1`), exercising high-`0xff` partial-signature bytes
/// that pass wire deserialization into cryptographic verification.
const MAX_VALID_SCALAR: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x40,
];
const ZERO_SCALAR: [u8; 32] = [0u8; 32];
const FALLBACK_MIN_SCALAR: [u8; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];

fn random_hash(rng: &mut StdRng) -> Hash256 {
    Hash256::from(rng.random::<[u8; 32]>())
}

fn random_pubkey(rng: &mut StdRng) -> Pubkey {
    let mut bytes = rng.random::<[u8; 32]>();
    // Ensure a valid non-zero secp256k1 secret scalar (< curve order).
    bytes[0] &= 0x7f;
    bytes[31] |= 0x01;
    Privkey::from(bytes).pubkey()
}

fn random_scalar_bytes(rng: &mut StdRng) -> [u8; 32] {
    let mut bytes = rng.random::<[u8; 32]>();
    bytes[0] &= 0x7f;
    bytes
}

fn random_bytes(rng: &mut StdRng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.random::<u8>()).collect()
}

fn garbage_script(rng: &mut StdRng) -> Script {
    let args_len = rng.random_range(16..=64);
    let args = random_bytes(rng, args_len);
    Script::new_builder()
        .hash_type(Byte::new(0xff))
        .args(args.pack())
        .build()
}

fn build_forged_commitment_signed(
    channel_id: Hash256,
    commitment_number: u64,
    sig_bytes: [u8; 32],
    funding_nonce: &Value,
    revocation_nonce: Option<&Value>,
) -> Result<CommitmentSignedV2> {
    let try_build = |sig: [u8; 32]| {
        serde_json::from_value::<CommitmentSignedV2>(json!({
            "channel_id": channel_id,
            "commitment_number": commitment_number,
            "funding_tx_partial_signature": sig,
            "funding_nonce": funding_nonce.clone(),
            "revocation_nonce": revocation_nonce.cloned(),
        }))
    };
    match try_build(sig_bytes) {
        Ok(msg) => Ok(msg),
        Err(_) => Ok(try_build(FALLBACK_MIN_SCALAR)?),
    }
}

fn build_forged_revoke_and_ack(
    channel_id: Hash256,
    commitment_number: u64,
    sig_bytes: [u8; 32],
    revocation_nonce: &Value,
    next_per_commitment_point: Pubkey,
    next_commitment_nonce: &Value,
) -> Result<RevokeAndAckV2> {
    let try_build = |sig: [u8; 32]| {
        serde_json::from_value::<RevokeAndAckV2>(json!({
            "channel_id": channel_id,
            "commitment_number": commitment_number,
            "revocation_partial_signature": sig,
            "revocation_nonce": revocation_nonce.clone(),
            "next_per_commitment_point": next_per_commitment_point,
            "next_commitment_nonce": next_commitment_nonce.clone(),
        }))
    };
    match try_build(sig_bytes) {
        Ok(msg) => Ok(msg),
        Err(_) => Ok(try_build(FALLBACK_MIN_SCALAR)?),
    }
}

fn build_forged_closing_signed(
    channel_id: Hash256,
    sig_bytes: [u8; 32],
) -> Result<ClosingSignedV2> {
    let try_build = |sig: [u8; 32]| {
        serde_json::from_value::<ClosingSignedV2>(json!({
            "channel_id": channel_id,
            "partial_signature": sig,
        }))
    };
    match try_build(sig_bytes) {
        Ok(msg) => Ok(msg),
        Err(_) => Ok(try_build(FALLBACK_MIN_SCALAR)?),
    }
}

impl Driver {
    /// Stateful malicious-peer protocol fuzzer exercising all 5 channel lifecycle states
    /// (`NegotiatingFunding`/`CollaboratingFundingTx`, `AwaitingChannelReady`/post-funding,
    /// `ChannelReady` 100-frame semantic burst, `Reestablishing`, and `ShuttingDown`)
    /// while verifying victim hub liveness, state integrity, sibling canary non-starvation,
    /// and clean convergence/settlement.
    pub(super) fn protocol_state_machine_fuzz(&mut self, seed: u64) -> Result<()> {
        self.start_watchtowers([0, 1])?;
        self.start_watchtowers([1, 2])?;

        // Open healthy sibling channel 1-2 first as a continuous canary throughout all 5 states.
        let sib_edge = self.public_edge(1, 2, FUNDING_AMOUNT, 0)?;
        let ch_sib = sib_edge.channel;
        self.step()?;

        let mut rng = StdRng::seed_from_u64(seed ^ 0x4655_5a5a_0001);
        let mut tracked_payments: Vec<(Endpoint, Hash256)> = Vec::new();
        let mut audited_payments: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();
        let mut closed_channels: Vec<(Endpoint, Endpoint, Hash256)> = Vec::new();

        // Harvest two distinct valid PubNonces and valid funding parameters from ch_sib.
        let sib_state_1 = self
            .runtime
            .channel(1, &ch_sib)
            .context("missing ch_sib on victim 1")?;
        let sib_state_2 = self
            .runtime
            .channel(2, &ch_sib)
            .context("missing ch_sib on sibling 2")?;
        let nonce_a = sib_state_1.session.own.public_nonce.clone();
        let nonce_b = sib_state_2.session.own.public_nonce.clone();
        ensure!(
            nonce_a != nonce_b,
            "expected distinct PubNonces from sibling channel endpoints"
        );
        let valid_shutdown_script = sib_state_1.get_local_shutdown_script();
        let reserved_ckb_amount = sib_state_1.local_reserved_ckb_amount;
        let nonce_a_json = serde_json::to_value(ChannelReadyV2 {
            channel_id: ch_sib,
            next_commitment_number: 0,
            next_commitment_nonce: nonce_a.clone(),
        })?["next_commitment_nonce"]
            .clone();
        let nonce_b_json = serde_json::to_value(ChannelReadyV2 {
            channel_id: ch_sib,
            next_commitment_number: 0,
            next_commitment_nonce: nonce_b.clone(),
        })?["next_commitment_nonce"]
            .clone();

        let chain_hash = get_chain_hash();
        let commitment_delay_epoch =
            EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1).full_value();

        // ---------------------------------------------------------------------
        // State 1: NegotiatingFunding / CollaboratingFundingTx
        // ---------------------------------------------------------------------
        self.until("malicious peer 0 connected to victim 1", |s| {
            Ok(s.runtime.link(0, 1).then_some(()))
        })?;
        let temp_cid = random_hash(&mut rng);
        let synthetic_open = OpenChannelV2 {
            chain_hash,
            channel_id: temp_cid,
            funding_udt_type_script: None,
            funding_amount: FUNDING_AMOUNT,
            shutdown_script: valid_shutdown_script.clone(),
            reserved_ckb_amount,
            funding_fee_rate: DEFAULT_FEE_RATE,
            commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
            commitment_delay_epoch,
            max_tlc_value_in_flight: FUNDING_AMOUNT,
            max_tlc_number_in_flight: MAX_TLC_NUMBER_IN_FLIGHT,
            funding_pubkey: random_pubkey(&mut rng),
            tlc_basepoint: random_pubkey(&mut rng),
            first_per_commitment_point: random_pubkey(&mut rng),
            second_per_commitment_point: random_pubkey(&mut rng),
            channel_announcement_nonce: None,
            initial_commitment_nonce: nonce_a.clone(),
            channel_flags: ChannelFlags::empty(),
            channel_features: 1,
        };
        self.fuzz_send_burst(
            vec![InjectedWireAction::SendFrame(
                FiberMessage::open_channel_v2(synthetic_open),
            )],
            "state1 synthetic OpenChannelV2",
        )?;
        self.until("victim 1 records pending synthetic OpenChannelV2", |s| {
            Ok(s.runtime.nodes.borrow()[1]
                .pending_channels
                .contains(&temp_cid)
                .then_some(()))
        })?;

        // Accept on victim 1 so a live ChannelActor enters NegotiatingFunding/CollaboratingFundingTx.
        let accept_reply = self.runtime.request(1, |reply| {
            FiberActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: temp_cid,
                    funding_amount: FUNDING_AMOUNT,
                    shutdown_script: None,
                    max_tlc_value_in_flight: None,
                    max_tlc_number_in_flight: None,
                    min_tlc_value: None,
                    tlc_fee_proportional_millionths: Some(0),
                    tlc_expiry_delta: None,
                },
                reply,
            )
        })?;
        let ch_open = self
            .reply(
                "victim 1 accepts synthetic channel for State 1 fuzz",
                accept_reply,
            )?
            .new_channel_id;
        self.runtime.note(format_args!(
            "State 1: victim {} accepted synthetic temp={temp_cid} -> ch_open={ch_open}",
            NAMES[1]
        ));

        let state1_actions = vec![
            // Exercise NegotiatingFunding with premature/conflicting frames first.
            InjectedWireAction::SendFrame(FiberMessage::tx_complete_v2(TxCompleteV2 {
                channel_id: ch_open,
                initial_commitment_nonce: nonce_b.clone(),
            })),
            InjectedWireAction::SendFrame(FiberMessage::commitment_signed_v2(
                build_forged_commitment_signed(
                    ch_open,
                    1,
                    random_scalar_bytes(&mut rng),
                    &nonce_a_json,
                    None,
                )?,
            )),
            InjectedWireAction::SendFrame(FiberMessage::channel_ready_v2(ChannelReadyV2 {
                channel_id: ch_open,
                next_commitment_number: 2,
                next_commitment_nonce: nonce_b.clone(),
            })),
            InjectedWireAction::SendFrame(FiberMessage::add_tlc(AddTlc {
                channel_id: ch_open,
                tlc_id: 0,
                amount: 100_000_000,
                payment_hash: random_hash(&mut rng),
                expiry: self.clock_ms().saturating_add(3_600_000),
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: Some(PaymentOnionPacket::new(random_bytes(&mut rng, 128))),
            })),
            InjectedWireAction::SendFrame(FiberMessage::shutdown_v2(ShutdownV2 {
                channel_id: ch_open,
                fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
                close_script: valid_shutdown_script.clone(),
                closing_nonce: nonce_b.clone(),
            })),
            InjectedWireAction::SendFrame(FiberMessage::accept_channel_v2(AcceptChannelV2 {
                channel_id: ch_open,
                funding_amount: FUNDING_AMOUNT,
                reserved_ckb_amount,
                max_tlc_value_in_flight: FUNDING_AMOUNT,
                max_tlc_number_in_flight: MAX_TLC_NUMBER_IN_FLIGHT,
                funding_pubkey: random_pubkey(&mut rng),
                shutdown_script: valid_shutdown_script.clone(),
                tlc_basepoint: random_pubkey(&mut rng),
                first_per_commitment_point: random_pubkey(&mut rng),
                second_per_commitment_point: random_pubkey(&mut rng),
                channel_announcement_nonce: None,
                initial_commitment_nonce: nonce_b.clone(),
                channel_features: 1,
            })),
            // Transition into CollaboratingFundingTx via bogus empty TxUpdate, then blast again.
            InjectedWireAction::SendFrame(FiberMessage::tx_update(TxUpdate {
                channel_id: ch_open,
                tx: Transaction::default(),
            })),
            InjectedWireAction::SendFrame(FiberMessage::tx_complete_v2(TxCompleteV2 {
                channel_id: ch_open,
                initial_commitment_nonce: nonce_a.clone(),
            })),
            InjectedWireAction::SendFrame(FiberMessage::commitment_signed_v2(
                build_forged_commitment_signed(
                    ch_open,
                    1,
                    MAX_VALID_SCALAR,
                    &nonce_b_json,
                    Some(&nonce_a_json),
                )?,
            )),
            InjectedWireAction::SendFrame(FiberMessage::add_tlc(AddTlc {
                channel_id: ch_open,
                tlc_id: 1,
                amount: 0,
                payment_hash: random_hash(&mut rng),
                expiry: 0,
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: None,
            })),
            // Finish State 1 with a huge 1024-byte TxAbort so victim 1 aborts and cleans up ch_open.
            InjectedWireAction::SendFrame(FiberMessage::tx_abort(TxAbort {
                channel_id: ch_open,
                message: vec![b'X'; 1024],
            })),
        ];
        self.fuzz_send_burst(state1_actions, "state1 negotiating/collaborating burst")?;

        self.until("victim 1 aborts and cleans up fuzzed ch_open", |s| {
            let aborted =
                s.runtime.funding_aborted(1, ch_open) || s.runtime.funding_aborted(1, temp_cid);
            let absent = s.runtime.funding_record_absent(1, ch_open)?;
            Ok((aborted && absent).then_some(()))
        })?;
        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after State 1 fuzz",
            NAMES[1]
        );
        self.coverage.hit("fuzz_state1_negotiating_collaborating");

        let canary1 = self.fuzz_sibling_canary(&ch_sib, 500 * 100_000_000, 0x51)?;
        tracked_payments.push((Endpoint::Public(1), canary1));
        audited_payments.push((Endpoint::Public(1), Endpoint::Public(2), canary1));

        // ---------------------------------------------------------------------
        // State 2: AwaitingChannelReady / Post-Funding-Confirmation Fuzz
        // ---------------------------------------------------------------------
        let ch_fuzz = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        self.wait_online(0, 1, &ch_fuzz)?;

        let fuzz_before_0 = self
            .runtime
            .channel(0, &ch_fuzz)
            .context("missing ch_fuzz on attacker 0")?;
        let fuzz_before_1 = self
            .runtime
            .channel(1, &ch_fuzz)
            .context("missing ch_fuzz on victim 1")?;
        let state2_balances_before = (
            fuzz_before_0.get_local_balance(),
            fuzz_before_0.get_remote_balance(),
            fuzz_before_1.get_local_balance(),
            fuzz_before_1.get_remote_balance(),
        );
        let real_witnesses: Vec<Vec<u8>> = fuzz_before_0
            .funding_tx
            .as_ref()
            .map(|tx| tx.witnesses().into_iter().map(|w| w.unpack()).collect())
            .unwrap_or_else(|| vec![vec![0xaa; 65]]);
        let conflicting_ready_nonce =
            if fuzz_before_1.session.remote_nonce.as_ref() == Some(&nonce_a) {
                nonce_b.clone()
            } else {
                nonce_a.clone()
            };

        let state2_actions = vec![
            // Conflicting TxSignatures with wrong witness bytes.
            InjectedWireAction::SendFrame(FiberMessage::tx_signatures(TxSignatures {
                channel_id: ch_fuzz,
                witnesses: vec![vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04]],
            })),
            // Duplicate TxSignatures carrying the funding witnesses after ready.
            InjectedWireAction::SendFrame(FiberMessage::tx_signatures(TxSignatures {
                channel_id: ch_fuzz,
                witnesses: real_witnesses,
            })),
            // Late TxAbort after channel is already funded and ready.
            InjectedWireAction::SendFrame(FiberMessage::tx_abort(TxAbort {
                channel_id: ch_fuzz,
                message: b"malicious late TxAbort after ChannelReady".to_vec(),
            })),
            // Duplicate ChannelReadyV2 with conflicting next_commitment_nonce.
            InjectedWireAction::SendFrame(FiberMessage::channel_ready_v2(ChannelReadyV2 {
                channel_id: ch_fuzz,
                next_commitment_number: 2,
                next_commitment_nonce: conflicting_ready_nonce,
            })),
            // Conflicting AcceptChannelV2 on an already-ready channel.
            InjectedWireAction::SendFrame(FiberMessage::accept_channel_v2(AcceptChannelV2 {
                channel_id: ch_fuzz,
                funding_amount: FUNDING_AMOUNT,
                reserved_ckb_amount,
                max_tlc_value_in_flight: FUNDING_AMOUNT,
                max_tlc_number_in_flight: MAX_TLC_NUMBER_IN_FLIGHT,
                funding_pubkey: random_pubkey(&mut rng),
                shutdown_script: valid_shutdown_script.clone(),
                tlc_basepoint: random_pubkey(&mut rng),
                first_per_commitment_point: random_pubkey(&mut rng),
                second_per_commitment_point: random_pubkey(&mut rng),
                channel_announcement_nonce: None,
                initial_commitment_nonce: nonce_b.clone(),
                channel_features: 1,
            })),
        ];
        self.fuzz_send_burst(state2_actions, "state2 post-funding burst")?;

        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after State 2 fuzz",
            NAMES[1]
        );
        let fuzz_after2_0 = self
            .runtime
            .channel(0, &ch_fuzz)
            .context("missing ch_fuzz on attacker 0 after State 2")?;
        let fuzz_after2_1 = self
            .runtime
            .channel(1, &ch_fuzz)
            .context("missing ch_fuzz on victim 1 after State 2")?;
        ensure!(
            fuzz_after2_1.state == ChannelState::ChannelReady
                && fuzz_after2_1.connectivity_state == ChannelConnectivityState::Online,
            "State 2 fuzz perturbed ch_fuzz state on victim 1: {:?} {:?}",
            fuzz_after2_1.state,
            fuzz_after2_1.connectivity_state
        );
        let state2_balances_after = (
            fuzz_after2_0.get_local_balance(),
            fuzz_after2_0.get_remote_balance(),
            fuzz_after2_1.get_local_balance(),
            fuzz_after2_1.get_remote_balance(),
        );
        ensure!(
            state2_balances_before == state2_balances_after,
            "State 2 fuzz mutated ch_fuzz balances: before={state2_balances_before:?} after={state2_balances_after:?}"
        );

        // Real payment 0 -> 1 must still succeed on ch_fuzz after State 2 fuzz.
        let p_state2 = self.fuzz_direct_payment(0, 1, &ch_fuzz, 250 * 100_000_000, 0x52)?;
        tracked_payments.push((Endpoint::Public(0), p_state2));
        audited_payments.push((Endpoint::Public(0), Endpoint::Public(1), p_state2));
        self.coverage.hit("fuzz_state2_post_funding_ready");

        // ---------------------------------------------------------------------
        // State 3: ChannelReady (Deep Seeded Semantic Fuzz Burst — 100 frames)
        // ---------------------------------------------------------------------
        let snap3_0 = self
            .runtime
            .channel(0, &ch_fuzz)
            .context("missing ch_fuzz on attacker 0 before State 3")?;
        let snap3_1 = self
            .runtime
            .channel(1, &ch_fuzz)
            .context("missing ch_fuzz on victim 1 before State 3")?;
        let state3_balances_before = (
            snap3_0.get_local_balance(),
            snap3_0.get_remote_balance(),
            snap3_1.get_local_balance(),
            snap3_1.get_remote_balance(),
        );
        let victim_commit_before = snap3_1.get_current_commitment_numbers();
        let victim_local_commit = victim_commit_before.local;
        let victim_next_received_tlc = snap3_1.tlc_state.received_tlcs.next_tlc_id;
        let now_ms = self.clock_ms();

        let mut fuzz_frames: Vec<InjectedWireAction> = Vec::with_capacity(FUZZ_BURST_TOTAL);

        // Deterministic edge-case matrix covering every required message type & boundary value.
        // 1. AddTlc edge cases
        let add_tlc_cases: Vec<(u64, u128, u64, Option<Vec<u8>>)> = vec![
            (
                victim_next_received_tlc,
                0,
                now_ms.saturating_add(3_600_000),
                None,
            ),
            (
                0,
                1,
                now_ms.saturating_add(3_600_000),
                Some(random_bytes(&mut rng, 64)),
            ),
            (
                victim_next_received_tlc,
                u128::MAX,
                now_ms.saturating_add(3_600_000),
                None,
            ),
            (victim_next_received_tlc, 1, 0, None),
            (
                victim_next_received_tlc,
                1,
                1,
                Some(random_bytes(&mut rng, 32)),
            ),
            (
                victim_next_received_tlc,
                1,
                u64::MAX,
                Some(random_bytes(&mut rng, 256)),
            ),
            (
                u64::MAX,
                1,
                now_ms.saturating_add(3_600_000),
                Some(random_bytes(&mut rng, 128)),
            ),
            (
                victim_next_received_tlc.saturating_add(77),
                1,
                now_ms.saturating_add(3_600_000),
                Some(random_bytes(&mut rng, 6500)),
            ),
        ];
        for (tlc_id, amount, expiry, onion) in add_tlc_cases {
            fuzz_frames.push(InjectedWireAction::SendFrame(FiberMessage::add_tlc(
                AddTlc {
                    channel_id: ch_fuzz,
                    tlc_id,
                    amount,
                    payment_hash: random_hash(&mut rng),
                    expiry,
                    hash_algorithm: HashAlgorithm::CkbHash,
                    onion_packet: onion.map(PaymentOnionPacket::new),
                },
            )));
        }

        // 2. RemoveTlc edge cases
        let remove_tlc_cases: Vec<(u64, RemoveTlcReason)> = vec![
            (
                999_999,
                RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                    payment_preimage: Hash256::from([0u8; 32]),
                }),
            ),
            (
                u64::MAX,
                RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                    payment_preimage: random_hash(&mut rng),
                }),
            ),
            (
                0,
                RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                    payment_preimage: Hash256::from([0u8; 32]),
                }),
            ),
            (
                999_999,
                RemoveTlcReason::RemoveTlcFail(TlcErrPacket {
                    onion_packet: Vec::new(),
                }),
            ),
            (
                u64::MAX,
                RemoveTlcReason::RemoveTlcFail(TlcErrPacket {
                    onion_packet: random_bytes(&mut rng, 2048),
                }),
            ),
            (
                1,
                RemoveTlcReason::RemoveTlcFail(TlcErrPacket {
                    // Leading 32 zero bytes trigger plaintext error rejection.
                    onion_packet: vec![0u8; 64],
                }),
            ),
        ];
        for (tlc_id, reason) in remove_tlc_cases {
            fuzz_frames.push(InjectedWireAction::SendFrame(FiberMessage::remove_tlc(
                RemoveTlc {
                    channel_id: ch_fuzz,
                    tlc_id,
                    reason,
                },
            )));
        }

        // 3. CommitmentSignedV2 edge cases
        let wrong_channel_id = random_hash(&mut rng);
        let cs_cases: Vec<(Hash256, u64, [u8; 32], Option<&Value>)> = vec![
            (ch_fuzz, 0, ZERO_SCALAR, Some(&nonce_a_json)),
            (
                ch_fuzz,
                victim_local_commit.saturating_sub(1),
                MAX_VALID_SCALAR,
                Some(&nonce_b_json),
            ),
            (
                ch_fuzz,
                victim_local_commit.saturating_add(1),
                random_scalar_bytes(&mut rng),
                Some(&nonce_a_json),
            ),
            (
                ch_fuzz,
                victim_local_commit.saturating_add(2),
                ZERO_SCALAR,
                None,
            ),
            (ch_fuzz, u64::MAX, MAX_VALID_SCALAR, Some(&nonce_b_json)),
            (
                wrong_channel_id,
                victim_local_commit.saturating_add(1),
                random_scalar_bytes(&mut rng),
                Some(&nonce_a_json),
            ),
        ];
        for (cid, commit_num, sig, rev_nonce) in cs_cases {
            fuzz_frames.push(InjectedWireAction::SendFrame(
                FiberMessage::commitment_signed_v2(build_forged_commitment_signed(
                    cid,
                    commit_num,
                    sig,
                    &nonce_a_json,
                    rev_nonce,
                )?),
            ));
        }

        // 4. RevokeAndAckV2 edge cases (unsolicited, before any CommitmentSignedV2)
        let ack_nums = [
            0u64,
            victim_local_commit.saturating_sub(1),
            victim_local_commit.saturating_add(5),
            u64::MAX,
        ];
        let ack_sigs = [
            ZERO_SCALAR,
            MAX_VALID_SCALAR,
            random_scalar_bytes(&mut rng),
            random_scalar_bytes(&mut rng),
        ];
        for (&commit_num, &sig) in ack_nums.iter().zip(ack_sigs.iter()) {
            fuzz_frames.push(InjectedWireAction::SendFrame(
                FiberMessage::revoke_and_ack_v2(build_forged_revoke_and_ack(
                    ch_fuzz,
                    commit_num,
                    sig,
                    &nonce_a_json,
                    random_pubkey(&mut rng),
                    &nonce_b_json,
                )?),
            ));
        }

        // 5. ShutdownV2 edge cases (all invalid fees or invalid scripts so victim must reject)
        let shutdown_cases = [
            (0u64, valid_shutdown_script.clone()),
            (u64::MAX, valid_shutdown_script.clone()),
            (0u64, Script::default()),
            (u64::MAX, Script::default()),
            (DEFAULT_COMMITMENT_FEE_RATE, garbage_script(&mut rng)),
        ];
        for (fee_rate, close_script) in shutdown_cases {
            fuzz_frames.push(InjectedWireAction::SendFrame(FiberMessage::shutdown_v2(
                ShutdownV2 {
                    channel_id: ch_fuzz,
                    fee_rate,
                    close_script,
                    closing_nonce: nonce_b.clone(),
                },
            )));
        }

        // 6. ClosingSignedV2 edge cases (unsolicited before ShutdownV2)
        for sig in [ZERO_SCALAR, MAX_VALID_SCALAR, random_scalar_bytes(&mut rng)] {
            fuzz_frames.push(InjectedWireAction::SendFrame(
                FiberMessage::closing_signed_v2(build_forged_closing_signed(ch_fuzz, sig)?),
            ));
        }

        // 7. ReestablishChannelV2 edge cases (unsolicited while Online & NOT reestablishing)
        for (next_c, next_a) in [
            (0u64, 0u64),
            (9_999u64, 9_999u64),
            (u64::MAX, u64::MAX),
            (victim_local_commit, u64::MAX),
        ] {
            fuzz_frames.push(InjectedWireAction::SendFrame(
                FiberMessage::reestablish_channel_v2(ReestablishChannelV2 {
                    channel_id: ch_fuzz,
                    next_commitment_number: next_c,
                    next_ack_number: next_a,
                    next_local_commitment_nonce: if next_c.is_multiple_of(2) {
                        nonce_a.clone()
                    } else {
                        nonce_b.clone()
                    },
                }),
            ));
        }

        // 8. UpdateTlcInfo edge cases (unsolicited while ChannelReady with extreme values)
        for (ts, flags, delta, min_val, fee_ppm) in [
            (
                0u64,
                ChannelUpdateChannelFlags::DISABLED,
                0u64,
                0u128,
                0u128,
            ),
            (
                u64::MAX,
                ChannelUpdateChannelFlags::empty(),
                u64::MAX,
                u128::MAX,
                u128::MAX,
            ),
            (
                now_ms.saturating_add(1),
                ChannelUpdateChannelFlags::empty(),
                1u64,
                u128::MAX,
                999_999_999u128,
            ),
        ] {
            fuzz_frames.push(InjectedWireAction::SendFrame(
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::UpdateTlcInfo(
                    UpdateTlcInfo {
                        channel_id: ch_fuzz,
                        timestamp: ts,
                        channel_flags: flags,
                        tlc_expiry_delta: delta,
                        tlc_minimum_value: min_val,
                        tlc_fee_proportional_millionths: fee_ppm,
                    },
                )),
            ));
        }

        // Fill the remaining slots up to exactly 100 seeded adversarial frames across all 8 types.
        while fuzz_frames.len() < FUZZ_BURST_TOTAL {
            let frame = match rng.random_range(0u8..8u8) {
                0 => {
                    // AddTlc with seeded invalid combinations.
                    let variant = rng.random_range(0u8..4u8);
                    let (tlc_id, amount, expiry) = match variant {
                        0 => (
                            victim_next_received_tlc,
                            0,
                            now_ms.saturating_add(3_600_000),
                        ),
                        1 => (
                            victim_next_received_tlc,
                            u128::MAX,
                            now_ms.saturating_add(3_600_000),
                        ),
                        2 => (
                            victim_next_received_tlc,
                            1,
                            if rng.random::<bool>() { 0 } else { u64::MAX },
                        ),
                        _ => (
                            if rng.random::<bool>() {
                                0
                            } else {
                                u64::MAX.saturating_sub(rng.random_range(0..1000))
                            },
                            1,
                            now_ms.saturating_add(3_600_000),
                        ),
                    };
                    let onion_len = rng.random_range(0usize..256);
                    FiberMessage::add_tlc(AddTlc {
                        channel_id: ch_fuzz,
                        tlc_id,
                        amount,
                        payment_hash: random_hash(&mut rng),
                        expiry,
                        hash_algorithm: HashAlgorithm::CkbHash,
                        onion_packet: (onion_len > 0)
                            .then(|| PaymentOnionPacket::new(random_bytes(&mut rng, onion_len))),
                    })
                }
                1 => {
                    // RemoveTlc with nonexistent/wrong reason.
                    let tlc_id = if rng.random::<bool>() {
                        rng.random_range(100_000u64..1_000_000u64)
                    } else {
                        u64::MAX
                    };
                    let reason = if rng.random::<bool>() {
                        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: random_hash(&mut rng),
                        })
                    } else {
                        let len = rng.random_range(0usize..1024);
                        RemoveTlcReason::RemoveTlcFail(TlcErrPacket {
                            onion_packet: random_bytes(&mut rng, len),
                        })
                    };
                    FiberMessage::remove_tlc(RemoveTlc {
                        channel_id: ch_fuzz,
                        tlc_id,
                        reason,
                    })
                }
                2 => {
                    // CommitmentSignedV2 with out-of-range numbers or bogus sig.
                    let cid = if rng.random_range(0u8..5u8) == 0 {
                        random_hash(&mut rng)
                    } else {
                        ch_fuzz
                    };
                    let cnum = match rng.random_range(0u8..5u8) {
                        0 => 0,
                        1 => victim_local_commit.saturating_sub(1),
                        2 => victim_local_commit.saturating_add(1),
                        3 => victim_local_commit.saturating_add(2),
                        _ => u64::MAX,
                    };
                    let sig = match rng.random_range(0u8..3u8) {
                        0 => ZERO_SCALAR,
                        1 => MAX_VALID_SCALAR,
                        _ => random_scalar_bytes(&mut rng),
                    };
                    let nonce_ref = if rng.random::<bool>() {
                        &nonce_a_json
                    } else {
                        &nonce_b_json
                    };
                    let rev_ref = rng.random::<bool>().then_some(&nonce_b_json);
                    FiberMessage::commitment_signed_v2(build_forged_commitment_signed(
                        cid, cnum, sig, nonce_ref, rev_ref,
                    )?)
                }
                3 => {
                    // RevokeAndAckV2 unsolicited.
                    let cnum = match rng.random_range(0u8..4u8) {
                        0 => 0,
                        1 => victim_local_commit.saturating_sub(1),
                        2 => victim_local_commit.saturating_add(5),
                        _ => u64::MAX,
                    };
                    let sig = match rng.random_range(0u8..3u8) {
                        0 => ZERO_SCALAR,
                        1 => MAX_VALID_SCALAR,
                        _ => random_scalar_bytes(&mut rng),
                    };
                    FiberMessage::revoke_and_ack_v2(build_forged_revoke_and_ack(
                        ch_fuzz,
                        cnum,
                        sig,
                        &nonce_a_json,
                        random_pubkey(&mut rng),
                        &nonce_b_json,
                    )?)
                }
                4 => {
                    // ShutdownV2 with invalid fee or invalid script.
                    let (fee_rate, close_script) = match rng.random_range(0u8..3u8) {
                        0 => (0, valid_shutdown_script.clone()),
                        1 => (u64::MAX, Script::default()),
                        _ => (DEFAULT_COMMITMENT_FEE_RATE, garbage_script(&mut rng)),
                    };
                    FiberMessage::shutdown_v2(ShutdownV2 {
                        channel_id: ch_fuzz,
                        fee_rate,
                        close_script,
                        closing_nonce: if rng.random::<bool>() {
                            nonce_a.clone()
                        } else {
                            nonce_b.clone()
                        },
                    })
                }
                5 => {
                    // ClosingSignedV2 unsolicited.
                    let sig = if rng.random::<bool>() {
                        MAX_VALID_SCALAR
                    } else {
                        random_scalar_bytes(&mut rng)
                    };
                    FiberMessage::closing_signed_v2(build_forged_closing_signed(ch_fuzz, sig)?)
                }
                6 => {
                    // ReestablishChannelV2 unsolicited while Online.
                    let next_c = if rng.random::<bool>() {
                        rng.random_range(0u64..10_000u64)
                    } else {
                        u64::MAX
                    };
                    let next_a = if rng.random::<bool>() { 0 } else { u64::MAX };
                    FiberMessage::reestablish_channel_v2(ReestablishChannelV2 {
                        channel_id: ch_fuzz,
                        next_commitment_number: next_c,
                        next_ack_number: next_a,
                        next_local_commitment_nonce: if rng.random::<bool>() {
                            nonce_a.clone()
                        } else {
                            nonce_b.clone()
                        },
                    })
                }
                _ => {
                    // UpdateTlcInfo with extreme edge values.
                    FiberMessage::ChannelNormalOperation(FiberChannelMessage::UpdateTlcInfo(
                        UpdateTlcInfo {
                            channel_id: ch_fuzz,
                            timestamp: rng.random::<u64>(),
                            channel_flags: if rng.random::<bool>() {
                                ChannelUpdateChannelFlags::DISABLED
                            } else {
                                ChannelUpdateChannelFlags::empty()
                            },
                            tlc_expiry_delta: rng.random::<u64>(),
                            tlc_minimum_value: rng.random::<u128>(),
                            tlc_fee_proportional_millionths: rng.random::<u128>(),
                        },
                    ))
                }
            };
            fuzz_frames.push(InjectedWireAction::SendFrame(frame));
        }
        ensure!(fuzz_frames.len() == FUZZ_BURST_TOTAL);

        // Send the 100 semantic frames in 2 bursts of 50.
        let burst2 = fuzz_frames.split_off(FUZZ_BURST_SPLIT);
        let burst1 = fuzz_frames;
        self.fuzz_send_burst(burst1, "state3 ChannelReady semantic burst 1/2 (50 frames)")?;
        self.fuzz_send_burst(burst2, "state3 ChannelReady semantic burst 2/2 (50 frames)")?;

        // Also inject a raw wire CommitmentSignedV2 whose 32-byte partial signature is patched to
        // all 0xff (exceeding secp256k1 order n) to exercise wire-level scalar rejection.
        let marker_sig = [0x42u8; 32];
        let template_cs = FiberMessage::commitment_signed_v2(build_forged_commitment_signed(
            ch_fuzz,
            victim_local_commit.saturating_add(1),
            marker_sig,
            &nonce_a_json,
            Some(&nonce_b_json),
        )?);
        let mut raw_cs = template_cs.to_molecule_bytes().to_vec();
        let sig_offset = raw_cs
            .windows(32)
            .position(|w| w == marker_sig)
            .context("locate partial signature marker inside CommitmentSignedV2 molecule bytes")?;
        raw_cs[sig_offset..sig_offset + 32].fill(0xff);
        let prev_malformed = self.runtime.links[&(1, 0)].malformed_disconnects();
        let raw_sig_fault = Fault::FloodWire {
            from: 0,
            to: 1,
            actions: vec![InjectedWireAction::SendRawFrame {
                declared_len: raw_cs.len() as u32,
                payload: raw_cs,
            }],
            allow_malformed_disconnect: true,
        }
        .inject(&mut self.runtime)?;
        self.until(
            "victim 1 rejects all-0xff partial signature wire frame via malformed disconnect",
            |s| {
                Ok(
                    (s.runtime.links[&(1, 0)].malformed_disconnects() > prev_malformed)
                        .then_some(()),
                )
            },
        )?;
        raw_sig_fault.heal(&mut self.runtime);
        self.until(
            "link 0-1 reconnects after all-0xff raw frame disconnect",
            |s| Ok(s.runtime.link(0, 1).then_some(())),
        )?;

        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after State 3 100-frame fuzz burst",
            NAMES[1]
        );
        let after3_0 = self
            .runtime
            .channel(0, &ch_fuzz)
            .context("missing ch_fuzz on attacker 0 after State 3")?;
        let after3_1 = self
            .runtime
            .channel(1, &ch_fuzz)
            .context("missing ch_fuzz on victim 1 after State 3")?;
        let state3_balances_after = (
            after3_0.get_local_balance(),
            after3_0.get_remote_balance(),
            after3_1.get_local_balance(),
            after3_1.get_remote_balance(),
        );
        ensure!(
            state3_balances_before == state3_balances_after,
            "State 3 fuzz mutated ch_fuzz balances: before={state3_balances_before:?} after={state3_balances_after:?}"
        );
        ensure!(
            after3_1.get_current_commitment_numbers() == victim_commit_before,
            "State 3 fuzz advanced victim ch_fuzz commitment numbers: before={victim_commit_before:?} after={:?}",
            after3_1.get_current_commitment_numbers()
        );
        self.runtime.note(format_args!(
            "State 3 outcome on ch_fuzz={ch_fuzz}: state={:?} connectivity={:?} quarantined={}",
            after3_1.state,
            after3_1.connectivity_state,
            after3_1.state == ChannelState::Stale
        ));
        if after3_1.state == ChannelState::ChannelReady {
            self.wait_online(0, 1, &ch_fuzz)?;
        } else {
            self.fuzz_converge_channel_close(ch_fuzz, &mut closed_channels)?;
        }
        self.coverage.hit("fuzz_state3_channel_ready_burst");
        self.coverage
            .add("fuzz_state3_frames", FUZZ_BURST_TOTAL as u64);

        let canary2 = self.fuzz_sibling_canary(&ch_sib, 400 * 100_000_000, 0x53)?;
        tracked_payments.push((Endpoint::Public(1), canary2));
        audited_payments.push((Endpoint::Public(1), Endpoint::Public(2), canary2));

        // ---------------------------------------------------------------------
        // State 4: Reestablishing (Malicious Reestablish Fuzz)
        // ---------------------------------------------------------------------
        let ch_reest = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        self.wait_online(0, 1, &ch_reest)?;

        // Do 1 real payment first so commitment_number > 0 on both endpoints.
        let p_reest = self.fuzz_direct_payment(0, 1, &ch_reest, 300 * 100_000_000, 0x54)?;
        tracked_payments.push((Endpoint::Public(0), p_reest));
        audited_payments.push((Endpoint::Public(0), Endpoint::Public(1), p_reest));

        let reest_before_1 = self
            .runtime
            .channel(1, &ch_reest)
            .context("missing ch_reest on victim 1 before disconnect")?;
        let reest_commit_before = reest_before_1.get_current_commitment_numbers();
        ensure!(
            reest_commit_before.local > 0 && reest_commit_before.remote > 0,
            "expected ch_reest commitment numbers > 0 before State 4 fuzz, got {reest_commit_before:?}"
        );
        let reest_balances_before = (
            reest_before_1.get_local_balance(),
            reest_before_1.get_remote_balance(),
        );

        // Disconnect 0-1 and wait until victim 1 enters reestablishing == true.
        self.runtime.reset_link(0, 1);
        self.until("victim 1 enters reestablishing on ch_reest", |s| {
            Ok(s.runtime
                .channel(1, &ch_reest)
                .is_some_and(|st| st.reestablishing)
                .then_some(()))
        })?;

        // Queue adversarial ReestablishChannelV2 + premature CommitmentSignedV2 / AddTlc frames
        // while disconnected: host.rs drains injected actions first upon reconnect, so these
        // arrive while victim 1 is in reestablishing == true BEFORE 0's honest ReestablishChannelV2.
        let reest_actions = vec![
            InjectedWireAction::SendFrame(FiberMessage::add_tlc(AddTlc {
                channel_id: ch_reest,
                tlc_id: 0,
                amount: 100_000_000,
                payment_hash: random_hash(&mut rng),
                expiry: self.clock_ms().saturating_add(3_600_000),
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: Some(PaymentOnionPacket::new(random_bytes(&mut rng, 64))),
            })),
            InjectedWireAction::SendFrame(FiberMessage::commitment_signed_v2(
                build_forged_commitment_signed(
                    ch_reest,
                    reest_commit_before.local.saturating_add(1),
                    MAX_VALID_SCALAR,
                    &nonce_a_json,
                    Some(&nonce_b_json),
                )?,
            )),
            InjectedWireAction::SendFrame(FiberMessage::reestablish_channel_v2(
                ReestablishChannelV2 {
                    channel_id: ch_reest,
                    next_commitment_number: 0,
                    next_ack_number: 0,
                    next_local_commitment_nonce: nonce_a.clone(),
                },
            )),
            InjectedWireAction::SendFrame(FiberMessage::reestablish_channel_v2(
                ReestablishChannelV2 {
                    channel_id: ch_reest,
                    next_commitment_number: 9_999,
                    next_ack_number: reest_commit_before.remote,
                    next_local_commitment_nonce: nonce_b.clone(),
                },
            )),
            InjectedWireAction::SendFrame(FiberMessage::reestablish_channel_v2(
                ReestablishChannelV2 {
                    channel_id: ch_reest,
                    next_commitment_number: reest_commit_before.local,
                    next_ack_number: u64::MAX,
                    next_local_commitment_nonce: nonce_a.clone(),
                },
            )),
            InjectedWireAction::SendFrame(FiberMessage::commitment_signed_v2(
                build_forged_commitment_signed(
                    ch_reest,
                    u64::MAX,
                    ZERO_SCALAR,
                    &nonce_b_json,
                    None,
                )?,
            )),
            InjectedWireAction::SendFrame(FiberMessage::add_tlc(AddTlc {
                channel_id: ch_reest,
                tlc_id: u64::MAX,
                amount: 1,
                payment_hash: random_hash(&mut rng),
                expiry: 1,
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: None,
            })),
        ];
        self.fuzz_send_burst(reest_actions, "state4 reestablishing fuzz burst")?;

        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after State 4 reestablish fuzz",
            NAMES[1]
        );

        let mut reest_recovered = false;
        for _ in 0..RECOVERY_WAIT_STEPS {
            if self.runtime.channel_online(0, 1, &ch_reest)
                && self
                    .runtime
                    .channel(1, &ch_reest)
                    .is_some_and(|st| !st.reestablishing)
            {
                reest_recovered = true;
                break;
            }
            self.step()?;
        }
        if reest_recovered {
            let reest_after_1 = self
                .runtime
                .channel(1, &ch_reest)
                .context("missing recovered ch_reest on victim 1")?;
            let reest_balances_after = (
                reest_after_1.get_local_balance(),
                reest_after_1.get_remote_balance(),
            );
            ensure!(
                reest_balances_before == reest_balances_after,
                "State 4 fuzz mutated ch_reest balances: before={reest_balances_before:?} after={reest_balances_after:?}"
            );
            self.runtime.note(format_args!(
                "State 4 outcome: ch_reest={ch_reest} rejected adversarial reestablish frames and recovered cleanly to Online"
            ));
        } else {
            self.runtime.note(format_args!(
                "State 4 outcome: ch_reest={ch_reest} did not recover cleanly; verifying autonomous force-close + watchtower settlement"
            ));
            self.advance_clock_by(REESTABLISH_TIMEOUT_ADVANCE_MS);
            self.trigger_channel_maintenance([0, 1])?;
            self.fuzz_converge_channel_close(ch_reest, &mut closed_channels)?;
        }
        self.coverage.hit("fuzz_state4_reestablishing");

        // ---------------------------------------------------------------------
        // State 5: ShuttingDown (Malicious Close-Phase Fuzz)
        // ---------------------------------------------------------------------
        let ch_close = self
            .open_between(0, 1, FUNDING_AMOUNT, 0, false, OpeningFault::None)?
            .channel;
        self.wait_online(0, 1, &ch_close)?;

        // Blackhole ClosingSignedV2 in both directions while victim 1 initiates cooperative close
        // so both peers park in ShuttingDown before either can aggregate/broadcast the close tx.
        let bh_0_to_1 = Fault::BlackholePeer {
            from: 0,
            to: 1,
            matcher: FrameMatcher::channel(FrameKindFilter::ClosingSignedV2, ch_close),
        }
        .inject(&mut self.runtime)?;
        let bh_1_to_0 = Fault::BlackholePeer {
            from: 1,
            to: 0,
            matcher: FrameMatcher::channel(FrameKindFilter::ClosingSignedV2, ch_close),
        }
        .inject(&mut self.runtime)?;

        let close_reply = self.runtime.request(1, |reply| {
            FiberActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: ch_close,
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
            "victim 1 initiates cooperative close on ch_close",
            close_reply,
        )?;

        self.until(
            "victim 1 pauses in ShuttingDown on ch_close after blackholing 0->1 ClosingSignedV2",
            |s| {
                let paused =
                    s.runtime.channel(1, &ch_close).is_some_and(|st| {
                        matches!(st.state, ChannelState::ShuttingDown(_))
                            && st.session.closing.as_ref().is_some_and(|c| {
                                c.response.is_some() && c.remote_response.is_none()
                            })
                    });
                Ok((paused && bh_0_to_1.hit(&s.runtime)).then_some(()))
            },
        )?;

        // 0's real ClosingSignedV2 has already been dropped; heal bh_0_to_1 (while keeping
        // bh_1_to_0 active) so our injected bogus ClosingSignedV2 actually reaches victim 1.
        bh_0_to_1.heal(&mut self.runtime);

        let state5_actions = vec![
            // AddTlc during ShuttingDown.
            InjectedWireAction::SendFrame(FiberMessage::add_tlc(AddTlc {
                channel_id: ch_close,
                tlc_id: 0,
                amount: 100_000_000,
                payment_hash: random_hash(&mut rng),
                expiry: self.clock_ms().saturating_add(3_600_000),
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: Some(PaymentOnionPacket::new(random_bytes(&mut rng, 96))),
            })),
            // Conflicting ShutdownV2 with different fee_rate & close_script.
            InjectedWireAction::SendFrame(FiberMessage::shutdown_v2(ShutdownV2 {
                channel_id: ch_close,
                fee_rate: DEFAULT_COMMITMENT_FEE_RATE.saturating_mul(10),
                close_script: garbage_script(&mut rng),
                closing_nonce: nonce_b.clone(),
            })),
            // Bogus ClosingSignedV2 with invalid partial signatures.
            InjectedWireAction::SendFrame(FiberMessage::closing_signed_v2(
                build_forged_closing_signed(ch_close, ZERO_SCALAR)?,
            )),
            InjectedWireAction::SendFrame(FiberMessage::closing_signed_v2(
                build_forged_closing_signed(ch_close, MAX_VALID_SCALAR)?,
            )),
            InjectedWireAction::SendFrame(FiberMessage::closing_signed_v2(
                build_forged_closing_signed(ch_close, random_scalar_bytes(&mut rng))?,
            )),
            // TxAbort during ShuttingDown.
            InjectedWireAction::SendFrame(FiberMessage::tx_abort(TxAbort {
                channel_id: ch_close,
                message: b"malicious TxAbort during ShuttingDown".to_vec(),
            })),
        ];
        self.fuzz_send_burst(state5_actions, "state5 ShuttingDown fuzz burst")?;

        ensure!(
            self.runtime.node_available(1),
            "victim {} became unavailable after State 5 ShuttingDown fuzz",
            NAMES[1]
        );
        let close_after_fuzz = self
            .runtime
            .channel(1, &ch_close)
            .context("missing ch_close on victim 1 after State 5 fuzz")?;
        ensure!(
            matches!(close_after_fuzz.state, ChannelState::ShuttingDown(_))
                && close_after_fuzz
                    .session
                    .closing
                    .as_ref()
                    .is_some_and(|c| c.remote_response.is_none()),
            "victim 1 accepted bogus ClosingSignedV2 or exited ShuttingDown unexpectedly: {:?}",
            close_after_fuzz.state
        );

        // Escalate hung cooperative close on victim 1 to autonomous force-close & settle on-chain.
        self.advance_clock_by(PEER_RESPONSE_TIMEOUT_ADVANCE_MS);
        self.trigger_channel_maintenance([0, 1])?;
        self.fuzz_notify_channel_event(1, ch_close, ChannelEvent::MaintainChannelTlcs)?;

        let commit_close = self.until(
            "victim 1 autonomously escalates fuzzed ShuttingDown ch_close to force-close",
            |s| {
                Ok(s.runtime.channel(1, &ch_close).and_then(|st| {
                    matches!(
                        st.state,
                        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    )
                    .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                    .flatten()
                }))
            },
        )?;
        bh_1_to_0.heal(&mut self.runtime);
        self.advance_clock_past(self.clock_ms());
        let _ = self.finish_onchain_close([0, 1], ch_close, commit_close)?;
        closed_channels.push((Endpoint::Public(0), Endpoint::Public(1), ch_close));
        self.coverage.hit("fuzz_state5_shutting_down");

        // Final canary payment on sibling 1-2 + full Liveness & Consistency audits.
        let canary_final = self.fuzz_sibling_canary(&ch_sib, 350 * 100_000_000, 0x55)?;
        tracked_payments.push((Endpoint::Public(1), canary_final));
        audited_payments.push((Endpoint::Public(1), Endpoint::Public(2), canary_final));

        if self
            .runtime
            .channel(1, &ch_fuzz)
            .is_some_and(|st| st.state == ChannelState::ChannelReady)
        {
            self.wait_online(0, 1, &ch_fuzz)?;
        }

        LivenessOracle::assert_converged(&self.runtime, &tracked_payments, &[])?;
        ConsistencyOracle::audit(&self.runtime, &audited_payments, &closed_channels)?;
        self.runtime.note(format_args!(
            "protocol_state_machine_fuzz passed: {}",
            self.coverage.format_summary()
        ));
        Ok(())
    }

    fn fuzz_send_burst(&mut self, actions: Vec<InjectedWireAction>, label: &str) -> Result<()> {
        let count = actions.len() as u64;
        let sent_before = self.runtime.links[&(0, 1)].injected_sent();
        let flood = Fault::FloodWire {
            from: 0,
            to: 1,
            actions,
            allow_malformed_disconnect: true,
        }
        .inject(&mut self.runtime)?;
        self.until(&format!("{label} transmitted ({count} frames)"), |s| {
            Ok((s.runtime.links[&(0, 1)].injected_sent() >= sent_before + count).then_some(()))
        })?;
        for _ in 0..20 {
            self.step()?;
        }
        flood.heal(&mut self.runtime);
        Ok(())
    }

    fn fuzz_notify_channel_event(
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

    fn fuzz_converge_channel_close(
        &mut self,
        channel: Hash256,
        closed_channels: &mut Vec<(Endpoint, Endpoint, Hash256)>,
    ) -> Result<()> {
        let already_closed = self.runtime.channel(1, &channel).and_then(|st| {
            st.is_closed()
                .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                .flatten()
        });
        let commit = if let Some(tx) = already_closed {
            tx
        } else {
            let mut auto_commit = None;
            for _ in 0..200 {
                if let Some(tx) = self.runtime.channel(1, &channel).and_then(|st| {
                    st.is_closed()
                        .then(|| st.shutdown_transaction_hash.clone().map(Hash256::from))
                        .flatten()
                }) {
                    auto_commit = Some(tx);
                    break;
                }
                self.step()?;
            }
            match auto_commit {
                Some(tx) => tx,
                None => self.confirm_force_close(channel, 1, 0)?,
            }
        };
        self.advance_clock_past(self.clock_ms());
        let _ = self.finish_onchain_close([0, 1], channel, commit)?;
        closed_channels.push((Endpoint::Public(0), Endpoint::Public(1), channel));
        Ok(())
    }

    fn fuzz_sibling_canary(
        &mut self,
        sib_channel: &Hash256,
        amount: u128,
        tag: u8,
    ) -> Result<Hash256> {
        let start_steps = self.runtime.step_count;
        let preimage = Hash256::from([tag; 32]);
        let payee_key = Privkey::from([3u8; 32]);
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
        self.until("sibling canary invoice added on honest node 2", |_| {
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

        self.until("sibling 1-2 canary payment settles cleanly", |s| {
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
            elapsed_steps <= CANARY_STEP_BOUND,
            "sibling canary payment {payment_hash} starved: took {elapsed_steps} steps (limit {CANARY_STEP_BOUND})"
        );
        self.runtime.note(format_args!(
            "sibling 1->2 canary payment {payment_hash} settled in {elapsed_steps} steps"
        ));
        Ok(payment_hash)
    }

    fn fuzz_direct_payment(
        &mut self,
        payer: usize,
        payee: usize,
        channel: &Hash256,
        amount: u128,
        tag: u8,
    ) -> Result<Hash256> {
        let funding: OutPoint = self
            .runtime
            .channel(payer, channel)
            .with_context(|| format!("missing channel {channel} on {}", NAMES[payer]))?
            .get_funding_transaction_outpoint()
            .context("missing funding outpoint for direct payment")?;
        let preimage = Hash256::from([tag; 32]);
        let payee_key = Privkey::from([(payee + 1) as u8; 32]);
        let invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_preimage(preimage)
            .payment_secret(Hash256::from([tag.wrapping_add(0x60); 32]))
            .payee_pub_key(self.runtime.peers[payee].into())
            .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &payee_key.0))?;
        let payment_hash = *invoice.payment_hash();
        let invoice_str = invoice.to_string();
        let mut add_reply = self.runtime.request(payee, |reply| {
            FiberActorCommand::AddInvoice(invoice, Some(preimage), reply)
        })?;
        self.until("direct fuzz payment invoice added", |_| {
            receive(&mut add_reply)?.transpose().map_err(Into::into)
        })?;

        let reply = self.runtime.request(payer, |reply| {
            FiberActorCommand::SendPaymentWithRouter(
                SendPaymentWithRouterCommand {
                    invoice: Some(invoice_str),
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
            self.reply("submit direct payment on fuzzed channel", reply)?
                .payment_hash
                == payment_hash
        );

        self.until("direct payment settles cleanly on fuzzed channel", |s| {
            let status_ok =
                s.runtime.payment_status(payer, payment_hash) == Some(PaymentStatus::Success);
            let paid_ok = s.runtime.store(payee)?.get_invoice_status(&payment_hash)
                == Some(CkbInvoiceStatus::Paid);
            let drained = s.runtime.link(payer, payee)
                && [payer, payee].into_iter().all(|node| {
                    s.runtime.channel(node, channel).is_some_and(|state| {
                        state.state == ChannelState::ChannelReady
                            && state.connectivity_state == ChannelConnectivityState::Online
                            && !state.tlc_state.waiting_ack
                            && state.tlc_state.all_tlcs().next().is_none()
                    })
                });
            Ok((status_ok && paid_ok && drained).then_some(()))
        })?;
        self.wait_payment_actor(payer)?;
        self.runtime.note(format_args!(
            "direct payment {}->{} {payment_hash} settled cleanly on channel {channel}",
            NAMES[payer], NAMES[payee]
        ));
        Ok(payment_hash)
    }
}
