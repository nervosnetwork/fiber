use ckb_types::core::TransactionView;
use ckb_types::packed::{OutPoint, Script};
use fiber_types::{ChannelBasePublicKeys, EntityHex, Hash256};
use musig2::{PartialSignature, PubNonce};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use thiserror::Error;

pub use fiber_types::{
    ChannelSigningContent, CommitmentCounter, Musig2SignableContent, Musig2SigningContent,
    NoncePurpose, NonceSlot, OnchainKeyPurpose, OnchainSigningContent, SigningIntent,
};

/// Funding identity taken from a user-approved unsigned funding transaction.
///
/// This is derived locally by [`crate::ChannelSigner::bind_from_approved_funding`].
/// Later [`crate::ChannelSigner::prepare`] calls check that MuSig2
/// aggregation matches this funding lock, that commitment/close txs spend this
/// outpoint, that close txs pay the local shutdown script, and that
/// announcements name the same outpoint. Commitments additionally require
/// [`crate::ChannelSigner::approve_commitment_parameters`] and mandatory
/// [`crate::ChannelSigner::prepare_commitment`] verification in every policy.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChannelBinding {
    /// Funding transaction outpoint locked by this channel.
    #[serde_as(as = "EntityHex")]
    pub funding_outpoint: OutPoint,
    /// On-chain funding lock script from the approved funding output.
    #[serde_as(as = "EntityHex")]
    pub funding_lock_script: Script,
    /// Local cooperative-close / user-owned shutdown script.
    #[serde_as(as = "EntityHex")]
    pub local_shutdown_script: Script,
}

/// Opaque identifier for one channel key bundle owned by a [`RootSigner`](crate::RootSigner).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ChannelKeyId(pub Hash256);

/// Public material belonging to one signer-owned channel key bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelPublicMaterial {
    /// Stable identifier used to reopen this channel signer.
    pub channel_key_id: ChannelKeyId,
    /// Public funding and TLC base keys.
    pub base_public_keys: ChannelBasePublicKeys,
}

/// Partial MuSig2 signature produced by the channel funding key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Musig2Signature {
    /// Partial signature produced by the signer-owned funding key.
    pub partial_signature: PartialSignature,
}

/// Recoverable secp256k1 signature for a commitment-output spend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnchainSignature {
    /// Compact signature followed by the recovery id.
    pub signature: [u8; 65],
}

/// Signature corresponding to one [`ChannelSigningContent`] variant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelSignature {
    /// Partial MuSig2 signature.
    Musig2(Musig2Signature),
    /// Recoverable on-chain ECDSA signature.
    Onchain(OnchainSignature),
}

/// A compatibility warning discovered while preparing a signing request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SigningWarning {
    /// The same deterministic nonce was previously signed with another digest.
    NoncePreviouslyUsedForDifferentMessage {
        /// Previously signed digest.
        previous_message: [u8; 32],
    },
    /// The supplied commitment counter is below the highest signed value.
    CommitmentNumberRollback {
        /// Highest value already recorded by the signer.
        highest_signed: u64,
        /// Value supplied by this request.
        requested: u64,
    },
    /// The supplied commitment counter skips one or more values.
    CommitmentNumberJump {
        /// Highest value already recorded by the signer.
        highest_signed: u64,
        /// Value supplied by this request.
        requested: u64,
    },
}

/// Signer-computed material suitable for a user review screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SigningReview {
    /// Semantic operation being authorized.
    pub intent: SigningIntent,
    /// Counter lane used by a MuSig2 request.
    pub commitment_counter: Option<CommitmentCounter>,
    /// Commitment number used by the request, when applicable.
    pub commitment_number: Option<u64>,
    /// Digest independently computed by the signer.
    pub signing_message: [u8; 32],
    /// Domain-separated hash of canonical plaintext and signing context.
    pub content_hash: [u8; 32],
    /// Exact canonical bytes from which the digest was computed.
    pub canonical_content: Vec<u8>,
    /// Non-blocking compatibility and safety warnings.
    pub warnings: Vec<SigningWarning>,
}

/// Plaintext and review material before intent-specific semantic validation.
/// This internal type cannot be passed to any public signing method.
#[derive(Clone, Debug)]
pub(crate) struct SigningData {
    pub channel_key_id: ChannelKeyId,
    pub state_revision: u64,
    pub content: ChannelSigningContent,
    pub review: SigningReview,
}

#[cfg(test)]
impl SigningData {
    pub fn review(&self) -> &SigningReview {
        &self.review
    }
    pub fn content(&self) -> &ChannelSigningContent {
        &self.content
    }
}

/// Each variant carries every required result of its intent-specific verifier.
#[derive(Clone, Debug)]
pub(crate) enum PreparedValidation {
    Commitment {
        state: crate::commitment::CommitmentState,
        context: crate::CommitmentContext,
        review: crate::CommitmentReview,
        validated_at_ms: u64,
    },
    Revocation {
        state: crate::commitment::CommitmentState,
        context: crate::RevocationContext,
        validated_at_ms: u64,
    },
    Close {
        state: crate::commitment::CommitmentState,
    },
    Announcement {
        state: crate::commitment::CommitmentState,
    },
    Onchain {
        authorization: crate::OnchainSpendAuthorization,
    },
}

/// Fully validated request whose exact plaintext can be reviewed before signing.
/// Fields are private; only an intent-specific verifier can construct this value.
#[derive(Clone, Debug)]
pub struct PreparedSigning {
    pub(crate) data: SigningData,
    pub(crate) validation: PreparedValidation,
}

impl PreparedSigning {
    /// Verified old/replacement state identities for a revocation review.
    pub fn revocation_context(&self) -> Option<&crate::RevocationContext> {
        match &self.validation {
            PreparedValidation::Revocation { context, .. } => Some(context),
            _ => None,
        }
    }

    /// Wallet-approved destination, fee and inputs for an on-chain review.
    pub fn onchain_authorization(&self) -> Option<&crate::OnchainSpendAuthorization> {
        match &self.validation {
            PreparedValidation::Onchain { authorization } => Some(authorization),
            _ => None,
        }
    }

    /// Exact verified context, including protocol transition and witness orientation.
    pub fn commitment_context(&self) -> Option<&crate::CommitmentContext> {
        match &self.validation {
            PreparedValidation::Commitment { context, .. } => Some(context),
            _ => None,
        }
    }

    /// Verified commitment balances and transition, absent for other operations.
    pub fn commitment_review(&self) -> Option<&crate::CommitmentReview> {
        match &self.validation {
            PreparedValidation::Commitment { review, .. } => Some(review),
            _ => None,
        }
    }

    /// User-facing review generated from the exact signing plaintext.
    pub fn review(&self) -> &SigningReview {
        &self.data.review
    }

    /// Typed plaintext that will be signed if this preparation is approved.
    pub fn content(&self) -> &ChannelSigningContent {
        &self.data.content
    }
}

/// Errors exposed by the portable signer SDK.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SignerError {
    /// Supplied root key bytes are not a valid secp256k1 secret key.
    #[error("invalid signer root key")]
    InvalidRootKey,
    /// A root signer already exists in the supplied store.
    #[error("signer store is already initialized")]
    AlreadyInitialized,
    /// No root signer metadata exists in the supplied store.
    #[error("signer store is not initialized")]
    NotInitialized,
    /// The supplied root key does not match the initialized signer store.
    #[error("signer root key does not match the store")]
    RootKeyMismatch,
    /// The store contains an unsupported data format.
    #[error("unsupported signer store version: {0}")]
    UnsupportedStoreVersion(u16),
    /// Persisted signer data failed validation or decoding.
    #[error("corrupt signer store: {0}")]
    CorruptStore(String),
    /// The storage implementation failed.
    #[error("signer store failed: {0}")]
    Store(String),
    /// Secure random generation failed.
    #[error("secure random generation failed: {0}")]
    Random(String),
    /// No key bundle exists for the supplied opaque identifier.
    #[error("unknown channel key id: {0:?}")]
    UnknownChannelKey(ChannelKeyId),
    /// The requested nonce slot is invalid for its signing purpose.
    #[error("invalid nonce slot: {0}")]
    InvalidNonceSlot(String),
    /// Structured plaintext and signing context are inconsistent.
    #[error("invalid signing content: {0}")]
    InvalidContent(String),
    /// The prepared request belongs to another channel signer.
    #[error("prepared signing request belongs to another channel")]
    PreparedForAnotherChannel,
    /// Signer state changed after the request was reviewed.
    #[error("signer state changed; prepare and review the request again")]
    SigningStateChanged,
    /// MuSig2 or secp256k1 rejected the supplied signing context.
    #[error("signing failed: {0}")]
    Signing(String),
    /// Bound signing was requested before [`crate::ChannelSigner::bind_from_approved_funding`].
    #[error("channel signer is not bound to a Fiber channel")]
    ChannelNotBound,
    /// The signer was bound again with a different approved funding identity.
    #[error("channel signer is already bound to a different Fiber channel")]
    ChannelAlreadyBound,
}

/// Public nonce returned for a nonce slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Musig2Nonce {
    /// Public half of the signer-owned nonce.
    pub public_nonce: PubNonce,
}

/// Compute Fiber's canonical CKB transaction signing message.
pub fn compute_tx_message(transaction: &TransactionView) -> [u8; 32] {
    fiber_types::compute_tx_message(&transaction.data())
}
