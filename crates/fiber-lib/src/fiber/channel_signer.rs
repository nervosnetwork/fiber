//! Node-side channel signer.
//!
//! This is the host counterpart to [`fiber_lsp_sdk::ChannelSigner`]: a
//! channel-scoped signing entry that either signs immediately with a bound
//! [`InMemorySigner`] or waits for an external submission. The caller's
//! continuation still goes through [`SignerNotification`], so local and remote
//! paths stay uniform without a shared signer actor.
//!
//! Distinct from persistable [`fiber_types::ChannelSignerState`], which tracks
//! awaiting/signed receipts on disk.
//!
//! Morphologically aligned with [`crate::watchtower::WatchtowerSigner`]:
//! both are `Local(...) | External` enums with request / apply_submitted APIs.

use fiber_types::{
    blake2b_hash_with_salt, ChannelSignatureRequest, Hash256, InMemorySigner, Musig2Context,
    Musig2SigningContent, NextChannelSignerMaterial, NoncePurpose, SignatureRequestId,
    SubmitSignatureOutcome,
};
use musig2::{sign_partial, PartialSignature, SecNonce};
use ractor::RpcReplyPort;
use tracing::warn;

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
/// [`fiber_types::ChannelSignerState`]; this notification only carries the
/// signature result (and optional RPC reply port).
#[derive(Debug)]
pub enum SignerNotification {
    /// The requested channel signature is ready (or failed).
    ///
    /// `next_material` fulfils the signer contract's nonce-publication
    /// cadence: for commitment-counter requests the signer publishes the next
    /// round's commitment point and nonces alongside the signature, exactly
    /// like a remote device would.
    ChannelSignatureReady {
        channel_id: Hash256,
        request_id: SignatureRequestId,
        signature: Result<PartialSignature, String>,
        next_material: Option<NextChannelSignerMaterial>,
        rpc_reply: Option<RpcReplyPort<Result<SubmitSignatureOutcome, String>>>,
    },
}

/// Outcome of requesting a signature from [`ChannelSigner`].
#[derive(Debug)]
pub enum ChannelSignOutcome {
    /// Local key material produced a notification immediately.
    Ready(SignerNotification),
    /// No local keys; wait for an external submit against `ChannelSignerState`.
    AwaitingExternal,
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

    /// Build from optional local signer material (e.g. `get_local_signer()`).
    pub fn from_local_material(signer: Option<InMemorySigner>) -> Self {
        match signer {
            Some(signer) => Self::Local(signer),
            None => Self::External,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Request a MuSig2 partial signature for a typed channel request.
    ///
    /// Local signers return [`ChannelSignOutcome::Ready`] immediately. External
    /// signers return [`ChannelSignOutcome::AwaitingExternal`]; the caller must
    /// already have recorded the request in [`fiber_types::ChannelSignerState`].
    pub fn request_signature(
        &self,
        channel_id: Hash256,
        request_id: SignatureRequestId,
        request: ChannelSignatureRequest,
    ) -> ChannelSignOutcome {
        let Self::Local(signer) = self else {
            return ChannelSignOutcome::AwaitingExternal;
        };
        let signature = sign_channel_request(signer, &request);
        let next_material = next_material(signer, &request);
        if let Err(error) = &signature {
            warn!(
                "ChannelSigner: signing failed for channel {:?} request {:?}: {}",
                channel_id, request_id, error
            );
        }
        ChannelSignOutcome::Ready(SignerNotification::ChannelSignatureReady {
            channel_id,
            request_id,
            signature,
            next_material,
            rpc_reply: None,
        })
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

/// Publish the next commitment round's material alongside a signature, the
/// same way a remote device submits it. Requests without a commitment counter
/// (channel announcements) publish nothing.
fn next_material(
    signer: &InMemorySigner,
    request: &ChannelSignatureRequest,
) -> Option<NextChannelSignerMaterial> {
    let content = request.content();
    // Channel announcements have no commitment counter and publish nothing.
    content.commitment_counter?;
    let commitment_number = content.slot.commitment_number.checked_add(1)?;
    Some(NextChannelSignerMaterial {
        next_commitment_point: Some(signer.get_commitment_point(commitment_number)),
        next_commitment_nonce: Some(
            signer
                .derive_musig2_nonce(commitment_number, Musig2Context::Commitment)
                .public_nonce(),
        ),
        next_revocation_nonce: Some(
            signer
                .derive_musig2_nonce(commitment_number, Musig2Context::Revoke)
                .public_nonce(),
        ),
    })
}

/// Sign one typed channel request, deriving the secnonce exactly like the
/// previous signer-actor / inline local path.
fn sign_channel_request(
    signer: &InMemorySigner,
    request: &ChannelSignatureRequest,
) -> Result<PartialSignature, String> {
    let content: &Musig2SigningContent = request.content();
    let message = content.content.signing_message();
    let secnonce = match content.slot.purpose {
        NoncePurpose::Commitment => {
            signer.derive_musig2_nonce(content.slot.commitment_number, Musig2Context::Commitment)
        }
        NoncePurpose::Revocation => {
            signer.derive_musig2_nonce(content.slot.commitment_number, Musig2Context::Revoke)
        }
        NoncePurpose::ChannelAnnouncement => {
            let seckey = blake2b_hash_with_salt(
                signer.musig2_base_nonce.as_ref(),
                b"channel_announcement".as_slice(),
            );
            SecNonce::build(seckey).build()
        }
    };
    sign_partial(
        &content.key_agg_ctx,
        &signer.funding_key,
        secnonce,
        &content.agg_nonce,
        message,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use ckb_types::core::TransactionBuilder;
    use fiber_types::{
        CommitmentCounter, Musig2SignableContent, NonceSlot, Privkey, SignatureRequestId,
    };
    use musig2::{verify_partial, AggNonce, KeyAggContext};

    use super::*;

    fn revocation_request(
        signer: &InMemorySigner,
        remote: &InMemorySigner,
    ) -> (
        ChannelSignatureRequest,
        KeyAggContext,
        AggNonce,
        [u8; 32],
        musig2::PubNonce,
    ) {
        let slot = NonceSlot {
            purpose: NoncePurpose::Revocation,
            commitment_number: 3,
        };
        let local_secnonce =
            signer.derive_musig2_nonce(slot.commitment_number, Musig2Context::Revoke);
        let local_pubnonce = local_secnonce.public_nonce();
        let remote_secnonce =
            remote.derive_musig2_nonce(slot.commitment_number, Musig2Context::Revoke);
        let key_agg_ctx = KeyAggContext::new([
            signer.get_base_public_keys().funding_pubkey,
            remote.get_base_public_keys().funding_pubkey,
        ])
        .expect("valid aggregate keys");
        let agg_nonce = AggNonce::sum([local_pubnonce.clone(), remote_secnonce.public_nonce()]);
        let unsigned_tx = TransactionBuilder::default().build().data();
        let content = Musig2SigningContent {
            slot,
            commitment_counter: Some(CommitmentCounter::Remote),
            key_agg_ctx: key_agg_ctx.clone(),
            agg_nonce: agg_nonce.clone(),
            content: Musig2SignableContent::CommitmentTransaction(unsigned_tx.clone()),
        };
        let message = content.content.signing_message();
        (
            ChannelSignatureRequest::SendRevokeAndAck { content },
            key_agg_ctx,
            agg_nonce,
            message,
            local_pubnonce,
        )
    }

    #[test]
    fn local_request_signs_and_returns_ready() {
        let signer = InMemorySigner::generate_from_seed(b"channel-signer-test-local");
        let remote = InMemorySigner::generate_from_seed(b"channel-signer-test-remote");
        let channel_id = Hash256::from([7u8; 32]);
        let (request, key_agg_ctx, agg_nonce, message, local_pubnonce) =
            revocation_request(&signer, &remote);
        let request_id = SignatureRequestId(Hash256::from([9u8; 32]));

        let outcome =
            ChannelSigner::local(signer.clone()).request_signature(channel_id, request_id, request);
        let ChannelSignOutcome::Ready(SignerNotification::ChannelSignatureReady {
            channel_id: notified_channel,
            request_id: notified_request,
            next_material,
            signature,
            ..
        }) = outcome
        else {
            panic!("expected Ready notification");
        };
        assert!(
            next_material.is_some(),
            "commitment rounds publish material"
        );
        assert_eq!(notified_channel, channel_id);
        assert_eq!(notified_request, request_id);
        let partial = signature.expect("signature ok");

        verify_partial(
            &key_agg_ctx,
            partial,
            &agg_nonce,
            signer.get_base_public_keys().funding_pubkey,
            &local_pubnonce,
            message,
        )
        .expect("partial signature must verify");
    }

    #[test]
    fn external_request_awaits_submission() {
        let remote = InMemorySigner::generate_from_seed(b"channel-signer-external-remote");
        let local = InMemorySigner::generate_from_seed(b"channel-signer-external-local");
        let (request, _, _, _, _) = revocation_request(&local, &remote);
        let outcome = ChannelSigner::external().request_signature(
            Hash256::from([1u8; 32]),
            SignatureRequestId(Hash256::from([2u8; 32])),
            request,
        );
        assert!(matches!(outcome, ChannelSignOutcome::AwaitingExternal));
    }

    /// Must produce byte-identical signatures to the previous inline path.
    #[test]
    fn signature_matches_inline_derivation() {
        use musig2::sign_partial as inline_sign_partial;

        let signer = InMemorySigner::generate_from_seed(b"parity-check");
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 5,
        };
        let key_agg_ctx = KeyAggContext::new([
            signer.get_base_public_keys().funding_pubkey,
            Privkey::from(&[3u8; 32]).pubkey(),
        ])
        .expect("keys");
        let agg_nonce = AggNonce::sum([signer
            .derive_musig2_nonce(slot.commitment_number, Musig2Context::Commitment)
            .public_nonce()]);
        let unsigned_tx = TransactionBuilder::default().build().data();
        let message =
            Musig2SignableContent::CommitmentTransaction(unsigned_tx.clone()).signing_message();
        let content = Musig2SigningContent {
            slot,
            commitment_counter: Some(fiber_types::CommitmentCounter::Local),
            key_agg_ctx: key_agg_ctx.clone(),
            agg_nonce: agg_nonce.clone(),
            content: Musig2SignableContent::CommitmentTransaction(unsigned_tx),
        };
        let request = ChannelSignatureRequest::SendRevokeAndAck { content };

        let via_gate = sign_channel_request(&signer, &request).expect("gate signature");

        let secnonce =
            signer.derive_musig2_nonce(slot.commitment_number, Musig2Context::Commitment);
        let inline = inline_sign_partial(
            &key_agg_ctx,
            &signer.funding_key,
            secnonce,
            &agg_nonce,
            message,
        )
        .expect("inline signature");

        assert_eq!(via_gate, inline);
    }
}
