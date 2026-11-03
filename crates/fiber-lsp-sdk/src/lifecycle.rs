//! Signer-owned evidence for safe commitment replacement and payment resolution.
use ckb_types::packed::Transaction;
use fiber_types::{ChannelSigningTransition, Hash256, InMemorySigner};
use musig2::{
    aggregate_partial_signatures, verify_partial, AggNonce, CompactSignature, KeyAggContext,
    PartialSignature, PubNonce,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    commitment::invalid, ChannelSigningContent, CommitmentContext, CommitmentParameters,
    CommitmentReview, Musig2SignableContent, OwnedSettlementBinding, SignerError,
};

/// Public participant evidence, bound to the enclosing signing request.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningSessionEvidence {
    /// Counterparty nonce for this session, not an aggregate nonce.
    #[serde_as(as = "fiber_types::PubNonceAsBytes")]
    pub peer_public_nonce: PubNonce,
    /// Required when completing a message received from the counterparty.
    #[serde_as(as = "Option<fiber_types::PartialSignatureAsBytes>")]
    pub peer_partial_signature: Option<PartialSignature>,
}

/// Exact previously validated commitment identity. Versions are not nonce counters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentReference {
    /// True for the counterparty's transaction.
    pub for_remote: bool,
    /// Version encoded in CommitmentLock.
    pub version: u64,
    /// Hash of the raw commitment transaction.
    pub tx_hash: Hash256,
}

/// Context required before signing either revocation transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationContext {
    /// Actual protocol phase.
    pub transition: ChannelSigningTransition,
    /// Participant nonce and peer signature, if receiving a revocation.
    pub session: SigningSessionEvidence,
    /// Old commitment being revoked.
    pub revoked: CommitmentReference,
    /// Successor in the same commitment lane.
    pub replacement: CommitmentReference,
}

/// Durable transaction and witness snapshot; complete signature is present only after verification.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryRecord {
    /// Exact commitment identity.
    pub reference: CommitmentReference,
    /// Original unsigned transaction.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub transaction: Transaction,
    /// Witness reconstruction data, without secrets.
    pub settlement: OwnedSettlementBinding,
    /// Exact signing session.
    pub content: ChannelSigningContent,
    /// Peer nonce and signature validated at signing time.
    pub session: SigningSessionEvidence,
    /// Complete signature assembled and verified locally.
    #[serde_as(as = "Option<fiber_types::CompactSignatureAsBytes>")]
    pub complete_signature: Option<CompactSignature>,
}

/// Wallet-approved cooperative close fees, never inferred from an LSP proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseAuthorization {
    /// Total fee in shannons.
    pub fee: u64,
    /// Local party's fee contribution.
    pub local_fee_share: u64,
    /// Remote party's fee contribution.
    pub remote_fee_share: u64,
}

/// Verified revocation authorization and optional complete punishment signature.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevocationRecord {
    /// Old and replacement state identity and peer session evidence.
    pub context: RevocationContext,
    /// Exact revocation payload and signing session.
    pub content: ChannelSigningContent,
    /// Complete signature, present when processing the peer's revocation.
    #[serde_as(as = "Option<fiber_types::CompactSignatureAsBytes>")]
    pub signature: Option<CompactSignature>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct LifecycleState {
    pub commitments: Vec<RecoveryRecord>,
    pub revocations: Vec<RevocationRecord>,
    pub released: Vec<Hash256>,
    pub close: Option<CloseAuthorization>,
    pub announcement: Option<Vec<u8>>,
}

pub(crate) fn validate_session(
    content: &ChannelSigningContent,
    evidence: &SigningSessionEvidence,
    transition: ChannelSigningTransition,
    parameters: &CommitmentParameters,
    signer: &InMemorySigner,
) -> Result<(), SignerError> {
    use crate::{CommitmentCounter, NoncePurpose};
    let ChannelSigningContent::Musig2(m) = content else {
        return Err(invalid("expected MuSig2 session"));
    };
    let local = signer.funding_key.pubkey();
    let remote = parameters.remote_funding_key;
    let (keys, purpose, counter, require_peer) = match transition {
        ChannelSigningTransition::SendCommitmentSigned
        | ChannelSigningTransition::CompleteReceivedCommitment => {
            if !matches!(m.content, Musig2SignableContent::CommitmentTransaction(_)) {
                return Err(invalid("commitment phase/content mismatch"));
            }
            let mut keys = [local, remote];
            keys.sort();
            (
                keys,
                NoncePurpose::Commitment,
                Some(CommitmentCounter::Local),
                transition == ChannelSigningTransition::CompleteReceivedCommitment,
            )
        }
        ChannelSigningTransition::SendRevokeAndAck
        | ChannelSigningTransition::CompleteReceivedRevokeAndAck => {
            if !matches!(m.content, Musig2SignableContent::Revocation { .. }) {
                return Err(invalid("revocation phase/content mismatch"));
            }
            let receiving = transition == ChannelSigningTransition::CompleteReceivedRevokeAndAck;
            (
                if receiving {
                    [local, remote]
                } else {
                    [remote, local]
                },
                NoncePurpose::Revocation,
                Some(if receiving {
                    CommitmentCounter::Local
                } else {
                    CommitmentCounter::Remote
                }),
                receiving,
            )
        }
        ChannelSigningTransition::SendClosingSigned => {
            if !matches!(
                m.content,
                Musig2SignableContent::CooperativeCloseTransaction(_)
            ) {
                return Err(invalid("close phase/content mismatch"));
            }
            let mut keys = [local, remote];
            keys.sort();
            (
                keys,
                NoncePurpose::Commitment,
                Some(CommitmentCounter::Local),
                false,
            )
        }
        ChannelSigningTransition::SignChannelAnnouncement => {
            if !matches!(m.content, Musig2SignableContent::ChannelAnnouncement(_))
                || m.slot.commitment_number != 0
            {
                return Err(invalid("invalid announcement phase or nonce"));
            }
            let mut keys = [local, remote];
            keys.sort();
            (keys, NoncePurpose::ChannelAnnouncement, None, false)
        }
    };
    let expected = KeyAggContext::new(keys).map_err(|e| invalid(&e.to_string()))?;
    if expected.serialize() != m.key_agg_ctx.serialize()
        || m.slot.purpose != purpose
        || m.commitment_counter != counter
    {
        return Err(invalid("incorrect signing keys, nonce purpose or counter"));
    }
    let nonce = crate::signer::derive_nonce(signer, m.slot).public_nonce();
    if AggNonce::sum([nonce, evidence.peer_public_nonce.clone()]) != m.agg_nonce {
        return Err(invalid("aggregate nonce does not match participant nonces"));
    }
    if require_peer && evidence.peer_partial_signature.is_none() {
        return Err(invalid("missing peer partial signature"));
    }
    if let Some(partial) = evidence.peer_partial_signature {
        verify_partial(
            &expected,
            partial,
            &m.agg_nonce,
            remote,
            &evidence.peer_public_nonce,
            content.signing_message(),
        )
        .map_err(|_| invalid("invalid peer partial signature"))?;
    }
    Ok(())
}

fn aggregate(
    content: &ChannelSigningContent,
    local: PartialSignature,
    peer: PartialSignature,
) -> Result<CompactSignature, SignerError> {
    let ChannelSigningContent::Musig2(m) = content else {
        return Err(invalid("expected MuSig2 content"));
    };
    aggregate_partial_signatures(
        &m.key_agg_ctx,
        &m.agg_nonce,
        [local, peer],
        content.signing_message(),
    )
    .map_err(|_| invalid("invalid complete signature"))
}

impl LifecycleState {
    pub fn record_commitment(
        &mut self,
        content: &ChannelSigningContent,
        context: &CommitmentContext,
        review: &CommitmentReview,
    ) -> Result<(), SignerError> {
        let ChannelSigningContent::Musig2(m) = content else {
            return Err(invalid("expected commitment"));
        };
        let Musig2SignableContent::CommitmentTransaction(tx) = &m.content else {
            return Err(invalid("expected commitment"));
        };
        let reference = CommitmentReference {
            for_remote: review.for_remote,
            version: review.version,
            tx_hash: tx.calc_tx_hash().into(),
        };
        if let Some(old) = self.commitments.iter().find(|r| r.reference == reference) {
            if old.session != context.session {
                return Err(invalid("retry changed participant evidence"));
            }
            return Ok(());
        }
        self.commitments.push(RecoveryRecord {
            reference,
            transaction: tx.clone(),
            settlement: context.settlement.clone(),
            content: content.clone(),
            session: context.session.clone(),
            complete_signature: None,
        });
        Ok(())
    }
    pub fn complete(
        &mut self,
        content: &ChannelSigningContent,
        local: PartialSignature,
    ) -> Result<(), SignerError> {
        let hash = content
            .content_hash(&content.canonical_bytes())
            .map_err(|e| invalid(&e))?;
        for record in &mut self.commitments {
            if record
                .content
                .content_hash(&record.content.canonical_bytes())
                .map_err(|e| invalid(&e))?
                == hash
            {
                if let Some(peer) = record.session.peer_partial_signature {
                    record.complete_signature = Some(aggregate(content, local, peer)?);
                }
            }
        }
        for record in &mut self.revocations {
            if record
                .content
                .content_hash(&record.content.canonical_bytes())
                .map_err(|e| invalid(&e))?
                == hash
            {
                if let Some(peer) = record.context.session.peer_partial_signature {
                    record.signature = Some(aggregate(content, local, peer)?);
                }
            }
        }
        Ok(())
    }
    pub fn find(&self, reference: &CommitmentReference) -> Result<&RecoveryRecord, SignerError> {
        self.commitments
            .iter()
            .find(|r| &r.reference == reference)
            .ok_or_else(|| invalid("unknown commitment evidence"))
    }
}
