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
    /// Dedicated cooperative-close nonce, never a commitment nonce.
    Closing,
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
            Self::CommitmentTransaction(_) => NoncePurpose::Commitment,
            Self::CooperativeCloseTransaction(_) => NoncePurpose::Closing,
            Self::Revocation { .. } => NoncePurpose::Revocation,
            Self::ChannelAnnouncement(_) => NoncePurpose::ChannelAnnouncement,
        }
    }
}

/// MuSig2 plaintext and session context signed by a channel signer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Musig2SigningContent {
    /// Required V2 allocation identity; contains public data only.
    pub v2: Option<V2NonceBinding>,
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

/// Public identity of one fresh V2 nonce allocation. The SDK retains its secret.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct V2NonceBinding {
    /// Channel this allocation is signing for.
    pub channel_id: Hash256,
    /// Commitment owner (not necessarily the signing participant).
    pub owner: Pubkey,
    /// Fresh nonce allocated by the signer, identifying its durable secret record.
    #[serde_as(as = "PubNonceAsBytes")]
    pub public_nonce: PubNonce,
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
                bytes.extend_from_slice(&bincode::serialize(&content.v2).map_err(to_string)?);
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
    /// V2 durable continuation. Peer inputs and exact transactions live in the V2 session.
    V2 {
        content: Musig2SigningContent,
        transition: ChannelSigningTransition,
        settlement_data: Option<SettlementData>,
    },
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
            Self::V2 { content, .. } => content,
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
            Self::V2 {
                settlement_data, ..
            } => settlement_data.as_ref(),
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
            Self::V2 { transition, .. } => *transition,
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

    /// Returns true if this signing request advances commitment state and
    /// strictly requires follow-up public material from an external signer.
    pub fn requires_next_signer_material(&self) -> bool {
        matches!(
            self.transition(),
            ChannelSigningTransition::SendCommitmentSigned
                | ChannelSigningTransition::CompleteReceivedCommitment
                | ChannelSigningTransition::SendRevokeAndAck
                | ChannelSigningTransition::CompleteReceivedRevokeAndAck
        )
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

/// Immutable request to resume after signer unavailability or node restart.
/// This is not a channel lifecycle or an idle/busy state.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingChannelSignature {
    pub request_id: SignatureRequestId,
    pub request: ChannelSignatureRequest,
}

/// Signing context, separate from the channel lifecycle. Only `pending_signature`
/// represents unfinished signing. A matching receipt retains its accepted signature
/// for recovery; once the request is cleared, the receipt identifies completed retries.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChannelSigningContext {
    /// Peer opened with external funding; retain across actor eviction so its
    /// immutable external signing intent can pin exact CS replay on reconnect.
    pub peer_external_funding: bool,
    /// Fresh signer-generated public nonces, never seeds or secret nonces.
    pub v2_nonce_pool: Vec<PubNonce>,
    /// Dedicated external public announcement nonce.
    pub v2_announcement_nonce: Option<PubNonce>,
    /// Immutable allocation records; retained to prevent recycling on replay/restart.
    pub v2_allocations: Vec<crate::SigningNonceV2>,
    /// Public-only local commitment points supplied by the external signer.
    pub v2_commitment_points: Vec<(u64, Pubkey)>,
    pub pending_signature: Option<PendingChannelSignature>,
    last_applied: Option<LastAppliedChannelSignature>,
}

/// Validated external channel signature, retained for recovery and submission retries.
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

/// Public channel-signer material required to send Fiber's `OpenChannel` message.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChannelOpenSignerMaterial {
    /// Additional fresh V2 public nonces; secrets remain in the SDK store.
    pub v2_nonces: Vec<PubNonce>,
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
    /// Fresh independent V2 public nonces for future allocations.
    pub v2_nonces: Vec<PubNonce>,
    /// Public commitment points indexed by exact V2 commitment number.
    pub commitment_points: Vec<(u64, Pubkey)>,
    /// Next local per-commitment point the node will need.
    pub next_commitment_point: Option<Pubkey>,
    /// Next commitment public nonce the node will need.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub next_commitment_nonce: Option<PubNonce>,
    /// Next revocation public nonce the node will need.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub next_revocation_nonce: Option<PubNonce>,
}

/// Read-only projection of a channel's external signing requests.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ChannelSigningStatus {
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

/// Invalid operation on the channel signing context.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ChannelSigningContextError {
    #[error("channel already has a pending signature")]
    AlreadyAwaitingSignature,
    #[error("channel is not waiting for a signature")]
    NoSignatureRequired,
    #[error("signature request id does not match the current request")]
    RequestMismatch,
    #[error("submitted signature does not match the previously applied result")]
    ResultMismatch,
}

impl ChannelSigningContext {
    /// Install one immutable request without displacing an outstanding signature.
    pub fn request_signature(
        &mut self,
        request_id: SignatureRequestId,
        request: ChannelSignatureRequest,
    ) -> Result<(), ChannelSigningContextError> {
        if self.is_awaiting_signature() {
            return Err(ChannelSigningContextError::AlreadyAwaitingSignature);
        }
        self.pending_signature = Some(PendingChannelSignature {
            request_id,
            request,
        });
        Ok(())
    }

    /// Validate a response without changing execution progress.
    pub fn pending_request(
        &self,
        id: SignatureRequestId,
    ) -> Result<ChannelSignatureRequest, ChannelSigningContextError> {
        match &self.pending_signature {
            Some(PendingChannelSignature {
                request_id,
                request,
                ..
            }) if *request_id == id => Ok(request.clone()),
            Some(PendingChannelSignature { .. }) => {
                Err(ChannelSigningContextError::RequestMismatch)
            }
            _ => Err(ChannelSigningContextError::NoSignatureRequired),
        }
    }

    /// Recognize a retry even while the next request is outstanding.
    pub fn replay_or_pending(
        &self,
        receipt: &LastAppliedChannelSignature,
    ) -> Result<Option<ChannelSignatureRequest>, ChannelSigningContextError> {
        if let Some(applied) = &self.last_applied {
            if applied == receipt {
                if let Some(pending) = &self.pending_signature {
                    if pending.request_id == receipt.request_id {
                        return Ok(Some(pending.request.clone()));
                    }
                }
                return Ok(None);
            }
            if applied.request_id == receipt.request_id {
                return Err(ChannelSigningContextError::ResultMismatch);
            }
        }
        self.pending_request(receipt.request_id).map(Some)
    }

    /// Retain a validated signature together with its unfinished continuation.
    /// Persist this checkpoint before clearing the request or sending peer messages.
    pub fn accept_request(
        &mut self,
        receipt: LastAppliedChannelSignature,
    ) -> Result<(), ChannelSigningContextError> {
        self.replay_or_pending(&receipt)?;
        self.pending_request(receipt.request_id)?;
        self.last_applied = Some(receipt);
        Ok(())
    }

    /// A validated signature whose protocol continuation still needs recovery.
    pub fn accepted_signature(&self) -> Option<&LastAppliedChannelSignature> {
        let pending = self.pending_signature.as_ref()?;
        self.last_applied
            .as_ref()
            .filter(|receipt| receipt.request_id == pending.request_id)
    }

    /// Clear the request and install its receipt with the caller's protocol update.
    /// The caller must persist the complete channel before acknowledging the result.
    pub fn complete_request(
        &mut self,
        receipt: LastAppliedChannelSignature,
    ) -> Result<(), ChannelSigningContextError> {
        self.pending_request(receipt.request_id)?;
        self.last_applied = Some(receipt);
        self.pending_signature = None;
        Ok(())
    }

    /// Clear the pending signature request (e.g. on channel force-close, expiry, or cancellation).
    pub fn cancel_request(&mut self) -> Option<PendingChannelSignature> {
        let pending = self.pending_signature.take();
        if pending.as_ref().is_some_and(|pending| {
            self.last_applied
                .as_ref()
                .is_some_and(|receipt| receipt.request_id == pending.request_id)
        }) {
            // An accepted but cancelled continuation must not acknowledge a retry
            // as successfully applied (for example after its safety deadline).
            self.last_applied = None;
        }
        pending
    }

    /// Most recently accepted signature response; a matching pending request is unfinished.
    pub fn last_applied(&self) -> Option<&LastAppliedChannelSignature> {
        self.last_applied.as_ref()
    }

    /// Whether the current step needs a signature response.
    pub fn is_awaiting_signature(&self) -> bool {
        self.pending_signature.is_some()
    }

    /// The exact request to redrive after restart.
    pub fn awaiting_signature(&self) -> Option<(SignatureRequestId, ChannelSignatureRequest)> {
        match &self.pending_signature {
            Some(PendingChannelSignature {
                request_id,
                request,
                ..
            }) => Some((*request_id, request.clone())),
            _ => None,
        }
    }

    /// Project the pending request independently of the signer implementation.
    pub fn signing_status(&self) -> ChannelSigningStatus {
        match &self.pending_signature {
            Some(PendingChannelSignature {
                request_id,
                request,
                ..
            }) => ChannelSigningStatus::SignatureRequired {
                request_id: *request_id,
                transition: request.transition(),
                content: request.content().clone(),
                settlement_data: request.settlement_data().cloned(),
            },
            _ => ChannelSigningStatus::NoSignatureRequired,
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
mod tests;
