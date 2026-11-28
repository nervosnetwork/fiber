use super::*;
use crate::{Musig2SignableContent, Musig2SigningContent, NoncePurpose, NonceSlot, SigningIntent};
use ckb_types::prelude::*;
use fiber_types::{settlement_witness_hash, Privkey, SettlementTlc};
use musig2::{AggNonce, KeyAggContext, SecNonce};

fn review(intent: SigningIntent) -> SigningReview {
    SigningReview {
        intent,
        commitment_counter: None,
        commitment_number: Some(1),
        signing_message: [1; 32],
        content_hash: [2; 32],
        canonical_content: Vec::new(),
        warnings: Vec::new(),
    }
}

fn settlement_keys() -> (Pubkey, Pubkey) {
    (
        Privkey::from(&[1; 32]).pubkey(),
        Privkey::from(&[2; 32]).pubkey(),
    )
}

fn commitment_tx_for(
    settlement: &SettlementData,
    for_remote: bool,
) -> ckb_types::packed::Transaction {
    let (local, remote) = settlement_keys();
    let hash = settlement_witness_hash(settlement, for_remote, local, remote);
    let mut args = vec![0u8; 36];
    args.extend_from_slice(&hash);
    args.push(0x00);
    args.push(fiber_types::ChannelFeatures::V2.bits());
    ckb_types::core::TransactionBuilder::default()
        .output(
            ckb_types::packed::CellOutput::new_builder()
                .lock(
                    ckb_types::packed::Script::new_builder()
                        .args(args.pack())
                        .build(),
                )
                .capacity(1000u64)
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build()
        .data()
}

fn content_for(transaction: ckb_types::packed::Transaction) -> ChannelSigningContent {
    let secret = secp256k1::SecretKey::from_byte_array(&[3; 32]).unwrap();
    let public = secret.public_key(secp256k1::SECP256K1);
    let nonce = SecNonce::build([4u8; 32]).build().public_nonce();
    ChannelSigningContent::Musig2(Musig2SigningContent {
        v2: Some(fiber_types::V2NonceBinding {
            channel_id: Hash256::from([5; 32]),
            owner: public.into(),
            public_nonce: nonce.clone(),
        }),
        slot: NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 1,
        },
        commitment_counter: None,
        key_agg_ctx: KeyAggContext::new([public]).unwrap(),
        agg_nonce: AggNonce::sum([nonce]),
        content: Musig2SignableContent::CommitmentTransaction(transaction),
    })
}

fn content() -> ChannelSigningContent {
    content_for(Default::default())
}

fn inbound_tlc(hash: Hash256) -> SettlementTlc {
    SettlementTlc {
        tlc_id: TLCId::Received(0),
        hash_algorithm: Default::default(),
        payment_amount: 5,
        payment_hash: hash,
        expiry: 1_000,
        local_key: None,
        local_key_pubkey: Some(Privkey::from(&[3; 32]).pubkey()),
        local_key_commitment_number: None,
        remote_key: Privkey::from(&[7; 32]).pubkey(),
    }
}

fn inbound_settlement(hash: Hash256, local_amount: u128) -> SettlementData {
    SettlementData {
        local_amount,
        remote_amount: 1,
        tlcs: vec![inbound_tlc(hash)],
    }
}

fn binding<'a>(data: &'a SettlementData) -> SettlementBinding<'a> {
    let (local_settlement_key, remote_settlement_key) = settlement_keys();
    SettlementBinding {
        data,
        local_settlement_key,
        remote_settlement_key,
        for_remote: Some(true),
    }
}

fn auto_commitment(
    settlement: &SettlementData,
    registry: &PaymentRegistry,
    transaction: ckb_types::packed::Transaction,
) -> SigningDecision {
    let review = review(SigningIntent::CommitmentTransaction);
    let content = content_for(transaction);
    SigningPolicy::Auto.decide(SigningPolicyInput {
        review: &review,
        content: &content,
        settlement: Some(binding(settlement)),
        registry,
    })
}

#[test]
fn always_approves_in_tests() {
    let review = review(SigningIntent::SettlementTransaction);
    let content = content();
    let registry = PaymentRegistry::default();
    assert_eq!(
        SigningPolicy::Always.decide(SigningPolicyInput {
            review: &review,
            content: &content,
            settlement: None,
            registry: &registry,
        }),
        SigningDecision::Allow
    );
}

#[test]
fn manual_always_asks() {
    let review = review(SigningIntent::Revocation);
    let content = content();
    let registry = PaymentRegistry::default();
    assert_eq!(
        SigningPolicy::Manual.decide(SigningPolicyInput {
            review: &review,
            content: &content,
            settlement: None,
            registry: &registry,
        }),
        SigningDecision::RequireConfirmation
    );
}

#[test]
fn auto_allows_inbound_for_issued_invoice() {
    let hash = Hash256::from([9; 32]);
    let mut registry = PaymentRegistry::default();
    registry.record_issued_invoice(hash);
    registry.note_signed_balance(10);
    let settlement = inbound_settlement(hash, 15);
    assert_eq!(
        auto_commitment(&settlement, &registry, commitment_tx_for(&settlement, true)),
        SigningDecision::Allow
    );
}

#[test]
fn auto_denies_inbound_for_unknown_invoice() {
    let settlement = inbound_settlement(Hash256::from([9; 32]), 15);
    let mut registry = PaymentRegistry::default();
    registry.note_signed_balance(10);
    assert_eq!(
        auto_commitment(&settlement, &registry, commitment_tx_for(&settlement, true)),
        SigningDecision::Deny
    );
}

#[test]
fn auto_asks_before_outbound_pay() {
    let hash = Hash256::from([8; 32]);
    let mut registry = PaymentRegistry::default();
    registry.record_outbound_payment(hash);
    registry.note_signed_balance(20);
    let settlement = SettlementData {
        local_amount: 12,
        remote_amount: 8,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: Default::default(),
            payment_amount: 8,
            payment_hash: hash,
            expiry: 1_000,
            local_key: None,
            local_key_pubkey: Some(Privkey::from(&[3; 32]).pubkey()),
            local_key_commitment_number: None,
            remote_key: Privkey::from(&[7; 32]).pubkey(),
        }],
    };
    assert_eq!(
        auto_commitment(&settlement, &registry, commitment_tx_for(&settlement, true)),
        SigningDecision::RequireConfirmation
    );
}

#[test]
fn auto_and_manual_require_confirmation_for_onchain_claims() {
    let content = content();
    let registry = PaymentRegistry::default();
    for intent in [
        SigningIntent::SettlementTransaction,
        SigningIntent::TlcTransaction,
    ] {
        let review = review(intent);
        for policy in [SigningPolicy::Auto, SigningPolicy::Manual] {
            assert_eq!(
                policy.decide(SigningPolicyInput {
                    review: &review,
                    content: &content,
                    settlement: None,
                    registry: &registry,
                }),
                SigningDecision::RequireConfirmation,
                "{policy:?}: {intent:?}"
            );
        }
    }
}

#[test]
fn auto_denies_when_local_amount_does_not_match_commitment_tx() {
    let hash = Hash256::from([9; 32]);
    let mut registry = PaymentRegistry::default();
    registry.record_issued_invoice(hash);
    registry.note_signed_balance(10);
    let claimed = inbound_settlement(hash, 15);
    let committed = inbound_settlement(hash, 1);
    assert_eq!(
        auto_commitment(&claimed, &registry, commitment_tx_for(&committed, true)),
        SigningDecision::Deny
    );
}

#[test]
fn auto_denies_when_tlc_set_does_not_match_commitment_tx() {
    let issued = Hash256::from([9; 32]);
    let other = Hash256::from([8; 32]);
    let mut registry = PaymentRegistry::default();
    registry.record_issued_invoice(issued);
    registry.note_signed_balance(10);
    let claimed = inbound_settlement(issued, 15);
    let committed = inbound_settlement(other, 15);
    assert_eq!(
        auto_commitment(&claimed, &registry, commitment_tx_for(&committed, true)),
        SigningDecision::Deny
    );
}

#[test]
fn auto_denies_commitment_without_a_bound_settlement() {
    let review = review(SigningIntent::CommitmentTransaction);
    let content = content();
    let registry = PaymentRegistry::default();
    assert_eq!(
        SigningPolicy::Auto.decide(SigningPolicyInput {
            review: &review,
            content: &content,
            settlement: None,
            registry: &registry,
        }),
        SigningDecision::Deny
    );
}

#[test]
fn auto_denies_nonce_reuse_across_signing_contexts() {
    let mut review = review(SigningIntent::CommitmentTransaction);
    review.warnings.push(
        crate::SigningWarning::NoncePreviouslyUsedForDifferentMessage {
            previous_message: [9u8; 32],
        },
    );
    let hash = Hash256::from([9; 32]);
    let mut registry = PaymentRegistry::default();
    registry.record_issued_invoice(hash);
    registry.note_signed_balance(10);
    let settlement = inbound_settlement(hash, 15);
    let content = content_for(commitment_tx_for(&settlement, true));
    assert_eq!(
        SigningPolicy::Auto.decide(SigningPolicyInput {
            review: &review,
            content: &content,
            settlement: Some(binding(&settlement)),
            registry: &registry,
        }),
        SigningDecision::Deny
    );
}
