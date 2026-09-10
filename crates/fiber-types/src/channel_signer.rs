//! Persistable protocol and state-machine types for channel signing.

use ckb_types::{
    packed::{CellDepVec, CellOutput, Transaction},
    prelude::*,
};
use molecule::prelude::Entity;
use musig2::{AggNonce, KeyAggContext, PubNonce};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use thiserror::Error;

use crate::{
    blake2b_hash_with_salt, ChannelAnnouncement, ChannelBasePublicKeys, EntityHex, Hash256,
    PartialSignatureAsBytes, PubNonceAsBytes, Pubkey, SettlementData,
};

/// Stable identifier for one outstanding channel signature request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SignatureRequestId(pub Hash256);

/// Matches Fiber's native MuSig2 nonce contexts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum NoncePurpose {
    /// Fiber's `Musig2Context::Commitment` derivation.
    Commitment,
    /// Fiber's `Musig2Context::Revoke` derivation.
    Revocation,
    /// The one-off public-channel announcement signature.
    ChannelAnnouncement,
}

/// Unique deterministic MuSig2 nonce location within one channel signer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct NonceSlot {
    /// Signing domain for the nonce.
    pub purpose: NoncePurpose,
    /// Commitment number for commitment/revocation slots; zero for announcement.
    pub commitment_number: u64,
}

/// Selects which Fiber commitment counter supplies a signing request's number.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CommitmentCounter {
    /// The local commitment counter.
    Local,
    /// The remote commitment counter.
    Remote,
}

/// User-visible semantic purpose of a signing request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SigningIntent {
    /// A commitment transaction spending the funding cell.
    CommitmentTransaction,
    /// A cooperative close transaction spending the funding cell.
    CooperativeCloseTransaction,
    /// Revocation data authorizing an old commitment punishment path.
    Revocation,
    /// A public channel announcement.
    ChannelAnnouncement,
    /// A settlement transaction.
    SettlementTransaction,
    /// A transaction spending a derived TLC key.
    TlcTransaction,
}

/// Plaintext MuSig2 payload from which the signer computes the signing digest.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Musig2SignableContent {
    /// An unsigned commitment transaction.
    CommitmentTransaction(#[serde_as(as = "EntityHex")] Transaction),
    /// An unsigned cooperative close transaction.
    CooperativeCloseTransaction(#[serde_as(as = "EntityHex")] Transaction),
    /// The exact byte preimage used by Fiber's revocation signature.
    Revocation {
        /// Settlement output committed by the revocation signature.
        #[serde_as(as = "EntityHex")]
        output: CellOutput,
        /// Settlement output data committed by the revocation signature.
        output_data: Vec<u8>,
        /// Commitment-lock arguments committed by the revocation signature.
        commitment_lock_script_args: Vec<u8>,
    },
    /// Unsigned fields of a public channel announcement.
    ChannelAnnouncement(ChannelAnnouncement),
}

impl Musig2SignableContent {
    /// Semantic signing intent represented by this plaintext.
    pub fn intent(&self) -> SigningIntent {
        match self {
            Self::CommitmentTransaction(_) => SigningIntent::CommitmentTransaction,
            Self::CooperativeCloseTransaction(_) => SigningIntent::CooperativeCloseTransaction,
            Self::Revocation { .. } => SigningIntent::Revocation,
            Self::ChannelAnnouncement(_) => SigningIntent::ChannelAnnouncement,
        }
    }

    /// Compute the exact message used by Fiber's existing signing paths.
    pub fn signing_message(&self) -> [u8; 32] {
        match self {
            Self::CommitmentTransaction(transaction)
            | Self::CooperativeCloseTransaction(transaction) => compute_tx_message(transaction),
            Self::Revocation {
                output,
                output_data,
                commitment_lock_script_args,
            } => {
                let mut preimage = Vec::with_capacity(
                    output.as_slice().len() + output_data.len() + commitment_lock_script_args.len(),
                );
                preimage.extend_from_slice(output.as_slice());
                preimage.extend_from_slice(output_data);
                preimage.extend_from_slice(commitment_lock_script_args);
                ckb_blake2b_256(&preimage)
            }
            Self::ChannelAnnouncement(announcement) => announcement.message_to_sign(),
        }
    }

    /// Canonical bytes shown to and independently hashed by a signer.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        match self {
            Self::CommitmentTransaction(transaction)
            | Self::CooperativeCloseTransaction(transaction) => canonical_tx_bytes(transaction),
            Self::Revocation {
                output,
                output_data,
                commitment_lock_script_args,
            } => {
                let mut bytes = Vec::with_capacity(
                    output.as_slice().len() + output_data.len() + commitment_lock_script_args.len(),
                );
                bytes.extend_from_slice(output.as_slice());
                bytes.extend_from_slice(output_data);
                bytes.extend_from_slice(commitment_lock_script_args);
                bytes
            }
            Self::ChannelAnnouncement(announcement) => {
                let mut unsigned = announcement.clone();
                unsigned.node1_signature = None;
                unsigned.node2_signature = None;
                unsigned.ckb_signature = None;
                let molecule: crate::gen::gossip::ChannelAnnouncement = unsigned.into();
                molecule.as_slice().to_vec()
            }
        }
    }

    /// Nonce domain required by this plaintext variant.
    pub fn expected_nonce_purpose(&self) -> NoncePurpose {
        match self {
            Self::CommitmentTransaction(_) | Self::CooperativeCloseTransaction(_) => {
                NoncePurpose::Commitment
            }
            Self::Revocation { .. } => NoncePurpose::Revocation,
            Self::ChannelAnnouncement(_) => NoncePurpose::ChannelAnnouncement,
        }
    }
}

/// MuSig2 plaintext and session context signed by a channel signer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Musig2SigningContent {
    /// Domain-separated nonce slot.
    pub slot: NonceSlot,
    /// Counter whose value was used for `slot`; absent for announcements.
    pub commitment_counter: Option<CommitmentCounter>,
    /// Ordered MuSig2 key aggregation context.
    pub key_agg_ctx: KeyAggContext,
    /// Aggregate of both participants' public nonces.
    pub agg_nonce: AggNonce,
    /// Plaintext object from which the signer computes the digest.
    pub content: Musig2SignableContent,
}

/// Signer-owned key used to authorize a commitment-output spend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OnchainKeyPurpose {
    /// The channel TLC base key used for the final balance settlement path.
    Settlement,
    /// A TLC key derived from the channel TLC base key and commitment point.
    Tlc {
        /// Commitment point index used by Fiber's native TLC key derivation.
        commitment_number: u64,
    },
}

/// Plaintext on-chain transaction signed by a channel signer.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OnchainSigningContent {
    /// Selects either the TLC base key or one derived TLC key.
    pub key_purpose: OnchainKeyPurpose,
    /// Unsigned transaction from which the signer computes the CKB digest.
    #[serde_as(as = "EntityHex")]
    pub transaction: Transaction,
}

impl PartialEq for OnchainSigningContent {
    fn eq(&self, other: &Self) -> bool {
        self.key_purpose == other.key_purpose
            && self.transaction.as_slice() == other.transaction.as_slice()
    }
}

impl Eq for OnchainSigningContent {}

/// One semantic channel signing operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ChannelSigningContent {
    /// MuSig2 commitment, revocation, close, or announcement content.
    Musig2(Musig2SigningContent),
    /// Settlement or derived TLC on-chain spend content.
    Onchain(OnchainSigningContent),
}

impl ChannelSigningContent {
    /// Compute the exact digest that will be signed.
    pub fn signing_message(&self) -> [u8; 32] {
        match self {
            Self::Musig2(content) => content.content.signing_message(),
            Self::Onchain(content) => compute_tx_message(&content.transaction),
        }
    }

    /// Exact canonical plaintext bytes reviewed by the signer.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        match self {
            Self::Musig2(content) => content.content.canonical_bytes(),
            Self::Onchain(content) => canonical_tx_bytes(&content.transaction),
        }
    }

    /// Domain-separated hash of plaintext plus signing session context.
    pub fn content_hash(&self, canonical_content: &[u8]) -> Result<[u8; 32], String> {
        let mut bytes = Vec::with_capacity(canonical_content.len() + 512);
        bytes.extend_from_slice(b"FIBER_SIGNER_CANONICAL_SIGNING_CONTENT");
        bytes.extend_from_slice(canonical_content);
        match self {
            Self::Musig2(content) => {
                bytes.push(0);
                bytes.extend_from_slice(&bincode::serialize(&content.slot).map_err(to_string)?);
                bytes.extend_from_slice(
                    &bincode::serialize(&content.commitment_counter).map_err(to_string)?,
                );
                bytes.extend_from_slice(
                    &bincode::serialize(&content.key_agg_ctx).map_err(to_string)?,
                );
                bytes
                    .extend_from_slice(&bincode::serialize(&content.agg_nonce).map_err(to_string)?);
            }
            Self::Onchain(content) => {
                bytes.push(1);
                match content.key_purpose {
                    OnchainKeyPurpose::Settlement => bytes.push(0),
                    OnchainKeyPurpose::Tlc { commitment_number } => {
                        bytes.push(1);
                        bytes.extend_from_slice(&commitment_number.to_be_bytes());
                    }
                }
            }
        }
        Ok(blake2b_hash_with_salt(&bytes, b"FIBER_SIGNER_CONTENT_HASH"))
    }

    /// User-visible semantic purpose.
    pub fn intent(&self) -> SigningIntent {
        match self {
            Self::Musig2(content) => content.content.intent(),
            Self::Onchain(content) => match content.key_purpose {
                OnchainKeyPurpose::Settlement => SigningIntent::SettlementTransaction,
                OnchainKeyPurpose::Tlc { .. } => SigningIntent::TlcTransaction,
            },
        }
    }

    /// MuSig2 nonce slot, when this request uses one.
    pub fn nonce_slot(&self) -> Option<NonceSlot> {
        match self {
            Self::Musig2(content) => Some(content.slot),
            Self::Onchain(_) => None,
        }
    }

    /// Commitment counter lane, when this request uses one.
    pub fn commitment_counter(&self) -> Option<CommitmentCounter> {
        match self {
            Self::Musig2(content) => content.commitment_counter,
            Self::Onchain(_) => None,
        }
    }
}

/// Internal ChannelActor signing state. Its variant is the state-machine resume point.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ChannelSignatureRequest {
    /// Sign and then send our `CommitmentSigned` message.
    SendCommitmentSigned {
        content: Musig2SigningContent,
        /// Observer notification data captured with the exact transaction state.
        settlement_data: SettlementData,
    },
    /// Complete processing a peer `CommitmentSigned` after our signature is supplied.
    CompleteReceivedCommitment {
        content: Musig2SigningContent,
        #[serde_as(as = "PartialSignatureAsBytes")]
        peer_partial_signature: musig2::PartialSignature,
        #[serde_as(as = "PubNonceAsBytes")]
        peer_next_commitment_nonce: PubNonce,
        /// Observer notification data captured with the exact transaction state.
        settlement_data: SettlementData,
    },
    /// Sign and then send our `RevokeAndAck` message.
    SendRevokeAndAck { content: Musig2SigningContent },
    /// Complete processing a peer `RevokeAndAck` after our signature is supplied.
    CompleteReceivedRevokeAndAck {
        content: Musig2SigningContent,
        #[serde_as(as = "PartialSignatureAsBytes")]
        peer_partial_signature: musig2::PartialSignature,
        next_per_commitment_point: Pubkey,
        #[serde_as(as = "PubNonceAsBytes")]
        next_revocation_nonce: PubNonce,
    },
    /// Sign and then send our `ClosingSigned` message.
    SendClosingSigned { content: Musig2SigningContent },
    /// Sign the public channel announcement.
    SignChannelAnnouncement { content: Musig2SigningContent },
}

impl ChannelSignatureRequest {
    /// Public signing plaintext sent to the external signer.
    pub fn content(&self) -> &Musig2SigningContent {
        match self {
            Self::SendCommitmentSigned { content, .. }
            | Self::CompleteReceivedCommitment { content, .. }
            | Self::SendRevokeAndAck { content }
            | Self::CompleteReceivedRevokeAndAck { content, .. }
            | Self::SendClosingSigned { content }
            | Self::SignChannelAnnouncement { content } => content,
        }
    }

    /// Settlement snapshot captured with this request, when it assigns balances.
    pub fn settlement_data(&self) -> Option<&SettlementData> {
        match self {
            Self::SendCommitmentSigned {
                settlement_data, ..
            }
            | Self::CompleteReceivedCommitment {
                settlement_data, ..
            } => Some(settlement_data),
            _ => None,
        }
    }

    /// User-facing transition associated with this internal state-machine point.
    pub fn transition(&self) -> ChannelSigningTransition {
        match self {
            Self::SendCommitmentSigned { .. } => ChannelSigningTransition::SendCommitmentSigned,
            Self::CompleteReceivedCommitment { .. } => {
                ChannelSigningTransition::CompleteReceivedCommitment
            }
            Self::SendRevokeAndAck { .. } => ChannelSigningTransition::SendRevokeAndAck,
            Self::CompleteReceivedRevokeAndAck { .. } => {
                ChannelSigningTransition::CompleteReceivedRevokeAndAck
            }
            Self::SendClosingSigned { .. } => ChannelSigningTransition::SendClosingSigned,
            Self::SignChannelAnnouncement { .. } => {
                ChannelSigningTransition::SignChannelAnnouncement
            }
        }
    }
}

/// Public semantic label for a channel signing transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ChannelSigningTransition {
    SendCommitmentSigned,
    CompleteReceivedCommitment,
    SendRevokeAndAck,
    CompleteReceivedRevokeAndAck,
    SendClosingSigned,
    SignChannelAnnouncement,
}

/// Persisted signer sub-state for a channel.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub enum ChannelSignerState {
    /// The node owns and invokes the channel signer directly.
    #[default]
    Internal,
    /// The node only holds public channel material and waits for external signatures.
    External(ExternalChannelSignerState),
}

/// Persisted state for one external channel signer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalChannelSignerState {
    state: ExternalSignerState,
    /// Last signature that successfully resumed this channel.
    ///
    /// Used to answer identical network retries with `AlreadyApplied`.
    #[serde(default)]
    last_applied: Option<LastAppliedChannelSignature>,
}

/// Receipt for one successfully applied external channel signature.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LastAppliedChannelSignature {
    /// Identifier of the applied signature request.
    pub request_id: SignatureRequestId,
    /// MuSig2 partial signature that resumed the channel.
    #[serde_as(as = "PartialSignatureAsBytes")]
    pub partial_signature: musig2::PartialSignature,
    /// Next-round public material submitted with the signature.
    pub next_material: Option<NextChannelSignerMaterial>,
}

/// Result of submitting a signature to an external signer state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitSignatureOutcome {
    /// The signature was verified and the state machine resumed.
    Applied,
    /// The same signature was already applied for this request.
    AlreadyApplied,
}

/// Current state-machine location of an external signer channel.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub enum ExternalSignerState {
    /// No signature is currently required.
    #[default]
    Ready,
    /// Channel processing is paused until this exact signature is submitted.
    AwaitingSignature {
        request_id: SignatureRequestId,
        request: ChannelSignatureRequest,
    },
}

/// Public channel-signer material required to send Fiber's `OpenChannel` message.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChannelOpenSignerMaterial {
    /// Static funding and TLC public keys for this channel.
    pub base_public_keys: ChannelBasePublicKeys,
    /// Per-commitment point for commitment number 1.
    pub first_commitment_point: Pubkey,
    /// Per-commitment point for commitment number 2.
    pub second_commitment_point: Pubkey,
    /// Commitment public nonce at the initial local commitment number (0).
    #[serde_as(as = "PubNonceAsBytes")]
    pub commitment_nonce: PubNonce,
    /// Commitment public nonce published in `TxComplete` (commitment number 1).
    #[serde_as(as = "PubNonceAsBytes")]
    pub next_commitment_nonce: PubNonce,
    /// Revocation public nonce published with `OpenChannel` (commitment number 2).
    #[serde_as(as = "PubNonceAsBytes")]
    pub revocation_nonce: PubNonce,
    /// Channel-announcement public nonce; required for public channels and forbidden for private ones.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub channel_announcement_nonce: Option<PubNonce>,
}

/// Follow-up public signer material submitted together with a channel signature.
#[serde_as]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NextChannelSignerMaterial {
    /// Next local per-commitment point the node will need.
    pub next_commitment_point: Option<Pubkey>,
    /// Next commitment public nonce the node will need.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub next_commitment_nonce: Option<PubNonce>,
    /// Next revocation public nonce the node will need.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub next_revocation_nonce: Option<PubNonce>,
}

/// Read-only public projection returned by the future signing-status RPC.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ChannelSigningStatus {
    Internal,
    NoSignatureRequired,
    SignatureRequired {
        request_id: SignatureRequestId,
        transition: ChannelSigningTransition,
        content: Musig2SigningContent,
        /// Balance and TLC snapshot captured with this commitment, when present.
        #[serde(default)]
        settlement_data: Option<SettlementData>,
    },
}

/// Invalid transition attempted on the channel signer sub-state machine.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ChannelSignerStateError {
    #[error("channel does not use an external signer")]
    InternalSigner,
    #[error("channel is already waiting for an external signature")]
    AlreadyAwaitingSignature,
    #[error("channel is not waiting for an external signature")]
    NoSignatureRequired,
    #[error("signature request id does not match the current request")]
    RequestMismatch,
    #[error("submitted signature does not match the previously applied result")]
    ResultMismatch,
}

impl ChannelSignerState {
    /// Construct an external signer in its idle state.
    pub fn external() -> Self {
        Self::External(ExternalChannelSignerState {
            state: ExternalSignerState::Ready,
            last_applied: None,
        })
    }

    /// Begin waiting for an external signature.
    pub fn request_signature(
        &mut self,
        request_id: SignatureRequestId,
        request: ChannelSignatureRequest,
    ) -> Result<(), ChannelSignerStateError> {
        let Self::External(external) = self else {
            return Err(ChannelSignerStateError::InternalSigner);
        };
        if !matches!(external.state, ExternalSignerState::Ready) {
            return Err(ChannelSignerStateError::AlreadyAwaitingSignature);
        }
        external.state = ExternalSignerState::AwaitingSignature {
            request_id,
            request,
        };
        Ok(())
    }

    /// Validate and clone the current request without changing state.
    pub fn pending_request(
        &self,
        request_id: SignatureRequestId,
    ) -> Result<ChannelSignatureRequest, ChannelSignerStateError> {
        let Self::External(external) = self else {
            return Err(ChannelSignerStateError::InternalSigner);
        };
        let ExternalSignerState::AwaitingSignature {
            request_id: expected_id,
            request,
        } = &external.state
        else {
            return Err(ChannelSignerStateError::NoSignatureRequired);
        };
        if *expected_id != request_id {
            return Err(ChannelSignerStateError::RequestMismatch);
        }
        Ok(request.clone())
    }

    /// Return the pending request, or `AlreadyApplied` when this receipt is a retry.
    pub fn replay_or_pending(
        &self,
        receipt: &LastAppliedChannelSignature,
    ) -> Result<Option<ChannelSignatureRequest>, ChannelSignerStateError> {
        let Self::External(external) = self else {
            return Err(ChannelSignerStateError::InternalSigner);
        };
        if let Some(applied) = &external.last_applied {
            if applied == receipt {
                return Ok(None);
            }
            if applied.request_id == receipt.request_id {
                return Err(ChannelSignerStateError::ResultMismatch);
            }
        }
        match &external.state {
            ExternalSignerState::AwaitingSignature {
                request_id,
                request,
            } => {
                if *request_id != receipt.request_id {
                    return Err(ChannelSignerStateError::RequestMismatch);
                }
                Ok(Some(request.clone()))
            }
            ExternalSignerState::Ready => Err(ChannelSignerStateError::NoSignatureRequired),
        }
    }

    /// Mark the validated current request complete and remember its receipt.
    pub fn complete_request(
        &mut self,
        receipt: LastAppliedChannelSignature,
    ) -> Result<(), ChannelSignerStateError> {
        self.pending_request(receipt.request_id)?;
        let Self::External(external) = self else {
            unreachable!("pending_request already checked external state")
        };
        external.last_applied = Some(receipt);
        external.state = ExternalSignerState::Ready;
        Ok(())
    }

    /// Last signature that successfully resumed this channel, if any.
    pub fn last_applied(&self) -> Option<&LastAppliedChannelSignature> {
        match self {
            Self::External(external) => external.last_applied.as_ref(),
            Self::Internal => None,
        }
    }

    /// Whether this channel is paused waiting for an external signature.
    pub fn is_awaiting_signature(&self) -> bool {
        matches!(
            self,
            Self::External(ExternalChannelSignerState {
                state: ExternalSignerState::AwaitingSignature { .. },
                ..
            })
        )
    }

    /// The currently awaited signature request, if any.
    ///
    /// Used to re-drive a persisted request after a process restart: local
    /// signing is deterministic, so re-submitting the same request yields the
    /// same signature and the idempotency receipt keeps the completion safe.
    pub fn awaiting_signature(&self) -> Option<(SignatureRequestId, ChannelSignatureRequest)> {
        match self {
            Self::External(ExternalChannelSignerState {
                state:
                    ExternalSignerState::AwaitingSignature {
                        request_id,
                        request,
                    },
                ..
            }) => Some((*request_id, request.clone())),
            _ => None,
        }
    }

    /// Public projection of the current signer state.
    pub fn signing_status(&self) -> ChannelSigningStatus {
        match self {
            Self::Internal => ChannelSigningStatus::Internal,
            Self::External(ExternalChannelSignerState {
                state: ExternalSignerState::Ready,
                ..
            }) => ChannelSigningStatus::NoSignatureRequired,
            Self::External(ExternalChannelSignerState {
                state:
                    ExternalSignerState::AwaitingSignature {
                        request_id,
                        request,
                    },
                ..
            }) => ChannelSigningStatus::SignatureRequired {
                request_id: *request_id,
                transition: request.transition(),
                content: request.content().clone(),
                settlement_data: request.settlement_data().cloned(),
            },
        }
    }
}

/// Compute Fiber's canonical CKB transaction signing message.
pub fn compute_tx_message(transaction: &Transaction) -> [u8; 32] {
    ckb_blake2b_256(&canonical_tx_bytes(transaction))
}

fn canonical_tx_bytes(transaction: &Transaction) -> Vec<u8> {
    transaction
        .raw()
        .as_builder()
        .cell_deps(CellDepVec::default())
        .build()
        .as_slice()
        .to_vec()
}

fn ckb_blake2b_256(bytes: &[u8]) -> [u8; 32] {
    blake2b_hash_with_salt(bytes, &[])
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> Musig2SigningContent {
        use musig2::{AggNonce, KeyAggContext, SecNonceBuilder};

        let secret_key = secp256k1::SecretKey::from_byte_array(&[42; 32]).unwrap();
        let public_key = secret_key.public_key(secp256k1::SECP256K1);
        let nonce = SecNonceBuilder::new([7; 32]).build().public_nonce();
        Musig2SigningContent {
            slot: NonceSlot {
                purpose: NoncePurpose::Commitment,
                commitment_number: 1,
            },
            commitment_counter: Some(CommitmentCounter::Local),
            key_agg_ctx: KeyAggContext::new([public_key]).unwrap(),
            agg_nonce: AggNonce::sum([nonce]),
            content: Musig2SignableContent::CommitmentTransaction(Transaction::default()),
        }
    }

    fn settlement_data() -> SettlementData {
        SettlementData {
            local_amount: 0,
            remote_amount: 0,
            tlcs: Vec::new(),
        }
    }

    fn receipt(request_id: SignatureRequestId, tag: u8) -> LastAppliedChannelSignature {
        LastAppliedChannelSignature {
            request_id,
            partial_signature: musig2::PartialSignature::from_slice(&[tag; 32]).unwrap(),
            next_material: None,
        }
    }

    #[test]
    fn external_signer_state_requires_matching_request() {
        let mut state = ChannelSignerState::external();
        let request_id = SignatureRequestId(Hash256::from([1; 32]));
        let request = ChannelSignatureRequest::SendCommitmentSigned {
            content: content(),
            settlement_data: settlement_data(),
        };

        state
            .request_signature(request_id, request.clone())
            .unwrap();
        assert_eq!(
            state.request_signature(SignatureRequestId(Hash256::from([4; 32])), request.clone()),
            Err(ChannelSignerStateError::AlreadyAwaitingSignature)
        );
        assert!(matches!(
            state.signing_status(),
            ChannelSigningStatus::SignatureRequired {
                request_id: id,
                transition: ChannelSigningTransition::SendCommitmentSigned,
                ..
            } if id == request_id
        ));
        assert_eq!(
            state.complete_request(receipt(SignatureRequestId(Hash256::from([2; 32])), 1)),
            Err(ChannelSignerStateError::RequestMismatch)
        );
        state.complete_request(receipt(request_id, 1)).unwrap();
        assert!(matches!(
            state.signing_status(),
            ChannelSigningStatus::NoSignatureRequired
        ));

        let next_request_id = SignatureRequestId(Hash256::from([5; 32]));
        state.request_signature(next_request_id, request).unwrap();
    }

    #[test]
    fn completing_a_request_makes_identical_retries_already_applied() {
        let mut state = ChannelSignerState::external();
        let request_id = SignatureRequestId(Hash256::from([1; 32]));
        let request = ChannelSignatureRequest::SendCommitmentSigned {
            content: content(),
            settlement_data: settlement_data(),
        };
        state.request_signature(request_id, request).unwrap();
        let applied = receipt(request_id, 1);
        assert!(state.replay_or_pending(&applied).unwrap().is_some());
        state.complete_request(applied.clone()).unwrap();
        assert!(state.replay_or_pending(&applied).unwrap().is_none());
    }

    #[test]
    fn identical_retry_is_already_applied_while_waiting_for_the_next_request() {
        let mut state = ChannelSignerState::external();
        let first = SignatureRequestId(Hash256::from([1; 32]));
        let second = SignatureRequestId(Hash256::from([2; 32]));
        state
            .request_signature(
                first,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                },
            )
            .unwrap();
        let applied = receipt(first, 1);
        state.complete_request(applied.clone()).unwrap();
        state
            .request_signature(
                second,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                },
            )
            .unwrap();
        assert!(state.replay_or_pending(&applied).unwrap().is_none());
        assert!(matches!(
            state.replay_or_pending(&receipt(first, 2)),
            Err(ChannelSignerStateError::ResultMismatch)
        ));
    }

    #[test]
    fn replay_of_the_same_request_with_a_different_result_is_rejected() {
        let mut state = ChannelSignerState::external();
        let request_id = SignatureRequestId(Hash256::from([1; 32]));
        state
            .request_signature(
                request_id,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                },
            )
            .unwrap();
        state.complete_request(receipt(request_id, 1)).unwrap();
        assert!(matches!(
            state.replay_or_pending(&receipt(request_id, 2)),
            Err(ChannelSignerStateError::ResultMismatch)
        ));
    }

    #[test]
    fn older_request_after_a_later_apply_is_no_longer_pending() {
        let mut state = ChannelSignerState::external();
        let first = SignatureRequestId(Hash256::from([1; 32]));
        let second = SignatureRequestId(Hash256::from([2; 32]));
        state
            .request_signature(
                first,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                },
            )
            .unwrap();
        state.complete_request(receipt(first, 1)).unwrap();
        state
            .request_signature(
                second,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                },
            )
            .unwrap();
        state.complete_request(receipt(second, 2)).unwrap();
        assert!(matches!(
            state.replay_or_pending(&receipt(first, 1)),
            Err(ChannelSignerStateError::NoSignatureRequired)
        ));
    }

    #[test]
    fn internal_signer_rejects_external_state_transitions() {
        let mut state = ChannelSignerState::Internal;
        let request_id = SignatureRequestId(Hash256::from([6; 32]));

        assert_eq!(
            state.request_signature(
                request_id,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                }
            ),
            Err(ChannelSignerStateError::InternalSigner)
        );
        assert_eq!(
            state.complete_request(receipt(request_id, 1)),
            Err(ChannelSignerStateError::InternalSigner)
        );
        assert!(matches!(
            state.signing_status(),
            ChannelSigningStatus::Internal
        ));
    }

    #[test]
    fn signer_state_roundtrips_while_waiting() {
        let mut state = ChannelSignerState::external();
        let request_id = SignatureRequestId(Hash256::from([3; 32]));
        state
            .request_signature(
                request_id,
                ChannelSignatureRequest::SendCommitmentSigned {
                    content: content(),
                    settlement_data: settlement_data(),
                },
            )
            .unwrap();

        let encoded = bincode::serialize(&state).unwrap();
        let restored: ChannelSignerState = bincode::deserialize(&encoded).unwrap();
        assert!(matches!(
            restored.signing_status(),
            ChannelSigningStatus::SignatureRequired {
                request_id: id,
                ..
            } if id == request_id
        ));
    }
}
