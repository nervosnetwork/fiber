//! Signing policies that sit between [`crate::ChannelSigner::prepare`]
//! and [`crate::ChannelSigner::sign`].

use std::collections::HashSet;

use fiber_types::{
    settlement_matches_commitment_tx, Hash256, Pubkey, SettlementData, SigningIntent, TLCId,
};
use serde::{Deserialize, Serialize};

use crate::{ChannelSigningContent, Musig2SignableContent, SigningReview, SigningWarning};

/// How a client decides whether to apply a prepared signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigningPolicy {
    /// Approve every prepared request. Compiled only for in-tree tests.
    #[cfg(any(test, feature = "test-apis"))]
    Always,
    /// Auto-approve inbound settlements for invoices this client issued.
    /// The snapshot must hash into the commitment lock args, and local
    /// balance must not fall. Outbound payments and on-chain claims
    /// require confirmation.
    Auto,
    /// Every signature requires an explicit user confirmation.
    Manual,
}

/// Outcome of evaluating a prepared request against a [`SigningPolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigningDecision {
    /// The policy authorizes signing without further UI.
    Allow,
    /// The user must confirm this request (outbound pay or on-chain claim).
    RequireConfirmation,
    /// The request must not be signed.
    Deny,
}

/// Client-owned invoice and payment memory used by [`SigningPolicy::Auto`].
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaymentRegistry {
    /// Payment hashes of invoices this client created and still considers live.
    pub issued_inbound: HashSet<Hash256>,
    /// Payment hashes of outbound payments this client has initiated.
    pub pending_outbound: HashSet<Hash256>,
    /// Local channel balance after the last approved commitment, if any.
    pub last_local_balance: Option<u128>,
}

impl PaymentRegistry {
    /// Record an invoice whose preimage this client holds.
    pub fn record_issued_invoice(&mut self, payment_hash: Hash256) {
        self.issued_inbound.insert(payment_hash);
    }

    /// Record an outbound payment this client started.
    pub fn record_outbound_payment(&mut self, payment_hash: Hash256) {
        self.pending_outbound.insert(payment_hash);
    }

    /// Remember the local balance from a settlement the client just approved.
    pub fn note_signed_balance(&mut self, local_amount: u128) {
        self.last_local_balance = Some(local_amount);
    }
}

/// Owned settlement snapshot plus the keys needed to hash it.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnedSettlementBinding {
    /// Balance and TLC set claimed for this commitment.
    pub data: SettlementData,
    /// Local TLC base key hashed into the settlement witness.
    pub local_settlement_key: Pubkey,
    /// Remote TLC base key hashed into the settlement witness.
    pub remote_settlement_key: Pubkey,
    /// `Some(true)` for `SendCommitmentSigned`, `Some(false)` for
    /// `CompleteReceivedCommitment`. `None` accepts either orientation.
    pub for_remote: Option<bool>,
}

impl OwnedSettlementBinding {
    /// Borrow as a [`SettlementBinding`].
    pub fn as_binding(&self) -> SettlementBinding<'_> {
        SettlementBinding {
            data: &self.data,
            local_settlement_key: self.local_settlement_key,
            remote_settlement_key: self.remote_settlement_key,
            for_remote: self.for_remote,
        }
    }
}

/// Settlement snapshot bound to the commitment transaction being signed.
#[derive(Clone, Copy, Debug)]
pub struct SettlementBinding<'a> {
    /// Balance and TLC set claimed for this commitment.
    pub data: &'a SettlementData,
    /// Local TLC base key hashed into the settlement witness.
    pub local_settlement_key: Pubkey,
    /// Remote TLC base key hashed into the settlement witness.
    pub remote_settlement_key: Pubkey,
    /// `Some(true)` for `SendCommitmentSigned`, `Some(false)` for
    /// `CompleteReceivedCommitment`. `None` accepts either orientation.
    pub for_remote: Option<bool>,
}

/// Inputs [`SigningPolicy::decide`] needs beyond the prepared review.
#[derive(Clone, Copy, Debug)]
pub struct SigningPolicyInput<'a> {
    /// Review produced by [`crate::ChannelSigner::prepare`].
    pub review: &'a SigningReview,
    /// Typed plaintext that would be signed.
    pub content: &'a ChannelSigningContent,
    /// Settlement snapshot plus the keys needed to bind it to the commitment tx.
    pub settlement: Option<SettlementBinding<'a>>,
    /// Invoices and outbound payments this client created.
    pub registry: &'a PaymentRegistry,
}

impl SigningPolicy {
    /// Evaluate one prepared request.
    pub fn decide(self, input: SigningPolicyInput<'_>) -> SigningDecision {
        match self {
            #[cfg(any(test, feature = "test-apis"))]
            Self::Always => SigningDecision::Allow,
            Self::Manual => SigningDecision::RequireConfirmation,
            Self::Auto => decide_auto(input),
        }
    }
}

fn decide_auto(input: SigningPolicyInput<'_>) -> SigningDecision {
    if input.review.warnings.iter().any(|warning| {
        matches!(
            warning,
            SigningWarning::NoncePreviouslyUsedForDifferentMessage { .. }
        )
    }) {
        return SigningDecision::Deny;
    }

    match input.review.intent {
        SigningIntent::Revocation => SigningDecision::Allow,
        SigningIntent::CommitmentTransaction => decide_auto_commitment(input),
        SigningIntent::CooperativeCloseTransaction
        | SigningIntent::ChannelAnnouncement
        | SigningIntent::SettlementTransaction
        | SigningIntent::TlcTransaction => SigningDecision::RequireConfirmation,
    }
}

fn decide_auto_commitment(input: SigningPolicyInput<'_>) -> SigningDecision {
    let Some(binding) = input.settlement else {
        return SigningDecision::Deny;
    };
    if !commitment_tx_commits_settlement(input.content, binding) {
        return SigningDecision::Deny;
    }

    let settlement = binding.data;
    let inbound_unknown = settlement.tlcs.iter().any(|tlc| {
        matches!(tlc.tlc_id, TLCId::Received(_))
            && !input.registry.issued_inbound.contains(&tlc.payment_hash)
    });
    if inbound_unknown {
        return SigningDecision::Deny;
    }

    let outbound_unknown = settlement.tlcs.iter().any(|tlc| {
        matches!(tlc.tlc_id, TLCId::Offered(_))
            && !input.registry.pending_outbound.contains(&tlc.payment_hash)
    });
    if outbound_unknown {
        return SigningDecision::Deny;
    }

    let has_outbound = settlement
        .tlcs
        .iter()
        .any(|tlc| matches!(tlc.tlc_id, TLCId::Offered(_)));

    match input.registry.last_local_balance {
        Some(previous) if settlement.local_amount < previous => {
            if has_outbound {
                SigningDecision::RequireConfirmation
            } else {
                SigningDecision::Deny
            }
        }
        _ if has_outbound => SigningDecision::RequireConfirmation,
        _ => SigningDecision::Allow,
    }
}

fn commitment_tx_commits_settlement(
    content: &ChannelSigningContent,
    binding: SettlementBinding<'_>,
) -> bool {
    let ChannelSigningContent::Musig2(musig2) = content else {
        return false;
    };
    let Musig2SignableContent::CommitmentTransaction(transaction) = &musig2.content else {
        return false;
    };
    settlement_matches_commitment_tx(
        transaction,
        binding.data,
        binding.local_settlement_key,
        binding.remote_settlement_key,
        binding.for_remote,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommitmentCounter, Musig2SignableContent, Musig2SigningContent, NoncePurpose, NonceSlot,
        SigningIntent,
    };
    use ckb_types::prelude::*;
    use fiber_types::{settlement_witness_hash, Privkey, SettlementTlc};
    use musig2::{AggNonce, KeyAggContext, SecNonce};

    fn review(intent: SigningIntent) -> SigningReview {
        SigningReview {
            intent,
            commitment_counter: Some(CommitmentCounter::Local),
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
            slot: NonceSlot {
                purpose: NoncePurpose::Commitment,
                commitment_number: 1,
            },
            commitment_counter: Some(CommitmentCounter::Local),
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
    fn auto_requires_confirmation_for_onchain_claims() {
        let review = review(SigningIntent::SettlementTransaction);
        let content = content();
        let registry = PaymentRegistry::default();
        assert_eq!(
            SigningPolicy::Auto.decide(SigningPolicyInput {
                review: &review,
                content: &content,
                settlement: None,
                registry: &registry,
            }),
            SigningDecision::RequireConfirmation
        );
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
}
