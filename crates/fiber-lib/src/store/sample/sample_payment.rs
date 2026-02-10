/// StoreSample implementations for v0.6.1 payment types:
/// - `PaymentSession` (prefix 192)
/// - `Attempt` (prefix 195)
use std::collections::HashMap;

use crate::fiber::graph::RouterHop;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::payment::{
    Attempt, AttemptStatus, HopHint, PaymentCustomRecords, PaymentSession, PaymentStatus,
    SendPaymentData, SessionRoute, SessionRouteNode,
};
use crate::fiber::types::PaymentHopData;
use crate::store::schema::{ATTEMPT_PREFIX, PAYMENT_SESSION_PREFIX};

use super::{
    deterministic_hash, deterministic_hash256, deterministic_outpoint, deterministic_pubkey,
    StoreSample,
};

// ─── PaymentSession ─────────────────────────────────────────────────

impl StoreSample for PaymentSession {
    const STORE_PREFIX: u8 = PAYMENT_SESSION_PREFIX;
    const TYPE_NAME: &'static str = "PaymentSession";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_session_minimal(seed), sample_session_full(seed)]
    }
}

/// Helper: build a minimal `SendPaymentData` with no Options set.
/// v0.6.1 SendPaymentData does NOT have trampoline_hops, trampoline_context, channel_stats
fn minimal_send_payment_data(seed: u64) -> SendPaymentData {
    SendPaymentData {
        target_pubkey: deterministic_pubkey(seed, 0),
        amount: 100_000_000,
        payment_hash: deterministic_hash256(seed, 1),
        invoice: None,
        final_tlc_expiry_delta: 9_600_000,
        tlc_expiry_limit: 576_000_000,
        timeout: None,
        max_fee_amount: None,
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        custom_records: None,
        allow_self_payment: false,
        hop_hints: vec![],
        router: vec![],
        allow_mpp: false,
        dry_run: false,
        // NOTE: v0.6.1 does NOT have trampoline_hops, trampoline_context, channel_stats
        channel_stats: Default::default(),
    }
}

/// Helper: build a fully-populated `SendPaymentData`.
fn full_send_payment_data(seed: u64) -> SendPaymentData {
    // Use single entry to guarantee deterministic HashMap serialization.
    let mut custom_records_data = HashMap::new();
    custom_records_data.insert(1u32, vec![0xab, 0xcd]);

    SendPaymentData {
        target_pubkey: deterministic_pubkey(seed, 100),
        amount: 500_000_000,
        payment_hash: deterministic_hash256(seed, 101),
        invoice: Some("fibd50000000001p...example".to_string()),
        final_tlc_expiry_delta: 9_600_000,
        tlc_expiry_limit: 576_000_000,
        timeout: Some(60_000),
        max_fee_amount: Some(5_000_000),
        max_parts: Some(4),
        keysend: false,
        udt_type_script: Some(ckb_types::packed::Script::default()),
        preimage: Some(deterministic_hash256(seed, 102)),
        custom_records: Some(PaymentCustomRecords {
            data: custom_records_data,
        }),
        allow_self_payment: true,
        hop_hints: vec![HopHint {
            pubkey: deterministic_pubkey(seed, 103),
            channel_outpoint: deterministic_outpoint(seed, 104),
            fee_rate: 500,
            tlc_expiry_delta: 40,
        }],
        router: vec![RouterHop {
            target: deterministic_pubkey(seed, 105),
            channel_outpoint: deterministic_outpoint(seed, 106),
            amount_received: 500_000_000,
            incoming_tlc_expiry: 1_704_070_800,
        }],
        allow_mpp: true,
        dry_run: false,
        // NOTE: v0.6.1 does NOT have trampoline_hops, trampoline_context, channel_stats
        channel_stats: Default::default(),
    }
}

fn sample_session_minimal(seed: u64) -> PaymentSession {
    PaymentSession {
        request: minimal_send_payment_data(seed),
        last_error: None,
        // NOTE: v0.6.1 does NOT have last_error_code
        try_limit: 10,
        status: PaymentStatus::Created,
        created_at: 1_704_067_200_000,
        last_updated_at: 1_704_067_200_000,
        cached_attempts: vec![], // #[serde(skip)]
    }
}

fn sample_session_full(seed: u64) -> PaymentSession {
    PaymentSession {
        request: full_send_payment_data(seed),
        last_error: Some("TemporaryChannelFailure".to_string()),
        // NOTE: v0.6.1 does NOT have last_error_code
        try_limit: 30,
        status: PaymentStatus::Inflight,
        created_at: 1_704_067_200_000,
        last_updated_at: 1_704_070_800_000,
        cached_attempts: vec![], // #[serde(skip)]
    }
}

// ─── Attempt ────────────────────────────────────────────────────────

impl StoreSample for Attempt {
    const STORE_PREFIX: u8 = ATTEMPT_PREFIX;
    const TYPE_NAME: &'static str = "Attempt";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_attempt_minimal(seed), sample_attempt_full(seed)]
    }
}

fn sample_attempt_minimal(seed: u64) -> Attempt {
    Attempt {
        id: 0,
        try_limit: 3,
        tried_times: 1,
        hash: deterministic_hash256(seed, 200),
        status: AttemptStatus::Created,
        payment_hash: deterministic_hash256(seed, 201),
        route: SessionRoute::default(),
        route_hops: vec![],
        session_key: deterministic_hash(seed, 202),
        preimage: None,
        created_at: 1_704_067_200_000,
        last_updated_at: 1_704_067_200_000,
        last_error: None,
    }
}

fn sample_attempt_full(seed: u64) -> Attempt {
    let hop1 = PaymentHopData {
        amount: 50_000_000,
        expiry: 1_704_070_800,
        payment_preimage: Some(deterministic_hash256(seed, 300)),
        hash_algorithm: HashAlgorithm::CkbHash,
        funding_tx_hash: deterministic_hash256(seed, 301),
        next_hop: Some(deterministic_pubkey(seed, 302)),
        custom_records: Some(PaymentCustomRecords {
            data: {
                let mut m = HashMap::new();
                m.insert(1u32, vec![0xab]);
                m
            },
        }),
    };

    let hop2 = PaymentHopData {
        amount: 45_000_000,
        expiry: 1_704_074_400,
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::Sha256,
        funding_tx_hash: deterministic_hash256(seed, 303),
        next_hop: None,
        custom_records: None,
    };

    let route = SessionRoute {
        nodes: vec![
            SessionRouteNode {
                pubkey: deterministic_pubkey(seed, 310),
                amount: 50_000_000,
                channel_outpoint: deterministic_outpoint(seed, 311),
            },
            SessionRouteNode {
                pubkey: deterministic_pubkey(seed, 312),
                amount: 45_000_000,
                channel_outpoint: deterministic_outpoint(seed, 313),
            },
        ],
    };

    Attempt {
        id: 1,
        try_limit: 10,
        tried_times: 3,
        hash: deterministic_hash256(seed, 210),
        status: AttemptStatus::Inflight,
        payment_hash: deterministic_hash256(seed, 211),
        route,
        route_hops: vec![hop1, hop2],
        session_key: deterministic_hash(seed, 212),
        preimage: Some(deterministic_hash256(seed, 213)),
        created_at: 1_704_067_200_000,
        last_updated_at: 1_704_070_800_000,
        last_error: Some("temporary failure".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payment_session_samples_roundtrip() {
        PaymentSession::verify_samples_roundtrip(42);
    }

    #[test]
    fn test_payment_session_samples_deterministic() {
        let bytes_a = PaymentSession::sample_bytes(42);
        let bytes_b = PaymentSession::sample_bytes(42);
        assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
    }

    #[test]
    fn test_attempt_samples_roundtrip() {
        Attempt::verify_samples_roundtrip(42);
    }

    #[test]
    fn test_attempt_samples_deterministic() {
        let bytes_a = Attempt::sample_bytes(42);
        let bytes_b = Attempt::sample_bytes(42);
        assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
    }
}
