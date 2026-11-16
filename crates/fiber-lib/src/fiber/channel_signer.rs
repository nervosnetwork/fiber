//! Node-side channel signer.
//!
//! This is the host counterpart to [`fiber_lsp_sdk::ChannelSigner`]: a
//! channel-scoped key provider with a bound [`InMemorySigner`] or an external
//! submission path. Actual local signatures always use the durable V2 session
//! nonce guard; external signatures arrive through [`SignerNotification`].
//!
//! Distinct from persistable [`fiber_types::ChannelSigningContext`], which tracks
//! awaiting/signed receipts on disk.
//!
//! Morphologically aligned with [`crate::watchtower::WatchtowerSigner`]:
//! both are `Local(...) | External` enums with explicit key ownership.

use fiber_types::{
    Hash256, InMemorySigner, NextChannelSignerMaterial, SignatureRequestId, SubmitSignatureOutcome,
};
use musig2::PartialSignature;
use ractor::RpcReplyPort;

/// Command used by RPC/network to submit one external channel signature.
#[derive(Clone, Debug)]
pub struct SubmitChannelSignatureCommand {
    pub channel_id: Hash256,
    pub request_id: SignatureRequestId,
    pub partial_signature: PartialSignature,
    pub next_material: Option<NextChannelSignerMaterial>,
}

/// Asynchronous channel-signature result delivered to the channel actor.
///
/// The outstanding plaintext is always recovered from
/// [`fiber_types::ChannelSigningContext`]; this notification only carries the
/// signature result (and optional RPC reply port).
#[derive(Debug)]
pub enum SignerNotification {
    /// The requested channel signature is ready (or failed).
    ///
    /// External signers publish the next round's public material alongside
    /// the signature. Local signers omit it because the channel can derive
    /// its own commitment points and nonces on demand.
    ChannelSignatureReady {
        channel_id: Hash256,
        request_id: SignatureRequestId,
        signature: Result<PartialSignature, String>,
        next_material: Option<NextChannelSignerMaterial>,
        rpc_reply: Option<RpcReplyPort<Result<SubmitSignatureOutcome, String>>>,
    },
}

/// Host-side signing entry for one Fiber channel.
///
/// Parallel shape to [`crate::watchtower::WatchtowerSigner`]:
/// `Local` binds key material; `External` awaits RPC submit.
#[derive(Clone, Debug)]
pub enum ChannelSigner {
    /// Host holds the channel key bundle and signs locally.
    Local(InMemorySigner),
    /// Channel keys live on an external device; host only awaits submits.
    External,
}

impl ChannelSigner {
    /// Host holds the channel key bundle and signs locally.
    pub fn local(signer: InMemorySigner) -> Self {
        Self::Local(signer)
    }

    /// Channel keys live on an external device; host only awaits submits.
    pub fn external() -> Self {
        Self::External
    }

    /// Restore once when loading a channel, rejecting inconsistent local keys.
    pub fn restore(
        material: Option<&InMemorySigner>,
        public_keys: &fiber_types::ChannelBasePublicKeys,
    ) -> Result<Self, String> {
        match material {
            Some(signer) if signer.get_base_public_keys() == *public_keys => {
                Ok(Self::Local(signer.clone()))
            }
            Some(_) => Err("Local signer does not match channel public keys".into()),
            None => Ok(Self::External),
        }
    }

    /// Local key material, when this signer owns it.
    pub fn local_material(&self) -> Option<&InMemorySigner> {
        match self {
            Self::Local(signer) => Some(signer),
            Self::External => None,
        }
    }

    /// Adapt the pending request to the external RPC without changing channel progress.
    pub fn signing_status(
        &self,
        context: &fiber_types::ChannelSigningContext,
    ) -> fiber_types::ChannelSigningStatus {
        if self.is_local() {
            fiber_types::ChannelSigningStatus::NoSignatureRequired
        } else {
            context.signing_status()
        }
    }

    /// Whether channel secrets are explicitly available locally.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Package an externally submitted signature as a notification.
    pub fn apply_submitted(
        channel_id: Hash256,
        request_id: SignatureRequestId,
        partial_signature: PartialSignature,
        next_material: Option<NextChannelSignerMaterial>,
        rpc_reply: Option<RpcReplyPort<Result<SubmitSignatureOutcome, String>>>,
    ) -> SignerNotification {
        SignerNotification::ChannelSignatureReady {
            channel_id,
            request_id,
            signature: Ok(partial_signature),
            next_material,
            rpc_reply,
        }
    }
}
