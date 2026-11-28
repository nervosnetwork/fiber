//! Signing policies that sit between [`crate::ChannelSigner::prepare`]
//! and [`crate::ChannelSigner::sign`].

use std::collections::HashSet;

use fiber_types::{
    settlement_matches_commitment_tx, Hash256, Pubkey, SettlementData, SigningIntent, TLCId,
};
use serde::{Deserialize, Serialize};

use crate::{ChannelSigningContent, Musig2SignableContent, SigningReview};

/// How a client decides whether to apply a prepared signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigningPolicy {
    /// Approve every prepared request. Compiled only for in-tree tests.
    #[cfg(any(test, feature = "test-apis"))]
    Always,
    /// Auto-approve verified inbound updates after opening. Opening, outbound
    /// changes, revocation and non-commitment operations require confirmation.
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

/// Legacy application bookkeeping used by [`SigningPolicy::decide`].
/// This hash-only registry does not authorize commitments in the hosted session;
/// use the channel signer's persisted payment authorizations instead.
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Decide from an immutable SDK preparation. Commitment automation requires the
    /// mandatory verifier's result, not a node-supplied balance or a hash-only registry.
    pub fn decide_prepared(self, prepared: &crate::PreparedSigning) -> SigningDecision {
        match self {
            #[cfg(any(test, feature = "test-apis"))]
            Self::Always => SigningDecision::Allow,
            Self::Manual => SigningDecision::RequireConfirmation,
            Self::Auto => match &prepared.validation {
                crate::protocol::PreparedValidation::Commitment { review, .. }
                    if review.version > 1 && !review.has_outbound_changes =>
                {
                    SigningDecision::Allow
                }
                _ => SigningDecision::RequireConfirmation,
            },
        }
    }

    /// Legacy additional policy check. Does not perform mandatory commitment validation.
    /// Prefer [`Self::decide_prepared`]; a payment-hash registry is not a spending authorization.
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
            crate::SigningWarning::NoncePreviouslyUsedForDifferentMessage { .. }
        )
    }) {
        return SigningDecision::Deny;
    }
    match input.review.intent {
        SigningIntent::Revocation => SigningDecision::RequireConfirmation,
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
mod tests;
