//! Signer actor.
//!
//! This module moves MuSig2 channel signing behind an actor boundary so that
//! callers request signatures by sending a message instead of signing inline.
//!
//! The contract is deliberately asynchronous: `SignChannel` carries no reply
//! port. The result is delivered later as a
//! [`SignerNotification::ChannelSignatureReady`] message to the channel actor.
//! This keeps the caller's code identical for a local signer (which answers
//! immediately) and an external remote signer (which answers when a signature is
//! submitted via RPC).
//!
//! Nonce derivation replicates the deterministic signing path so signatures
//! produced through the actor match protocol expectations.

use std::collections::HashMap;

#[cfg(feature = "watchtower")]
use fiber_json_types::SubmitWatchtowerSignatureResult;
use fiber_types::{
    blake2b_hash_with_salt, ChannelSignatureRequest, Hash256, InMemorySigner, Musig2Context,
    Musig2SigningContent, NextChannelSignerMaterial, NoncePurpose, SignatureRequestId,
    SubmitSignatureOutcome,
};
#[cfg(feature = "watchtower")]
use fiber_types::{compute_tx_message, NodeId, OnchainSigningContent, Privkey, Pubkey};
use musig2::{sign_partial, PartialSignature, SecNonce};
use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
#[cfg(feature = "watchtower")]
use secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, SECP256K1,
};
use tracing::warn;

use super::channel::ChannelActorMessage;
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerMessage;

/// Signature returned for one exact pending channel signer request.
#[derive(Clone, Debug)]
pub struct SubmitChannelSignatureCommand {
    pub channel_id: Hash256,
    pub request_id: SignatureRequestId,
    pub partial_signature: PartialSignature,
    pub next_material: Option<NextChannelSignerMaterial>,
}

/// Messages accepted by the signer actor.
#[derive(Debug)]
pub enum SignerActorMessage {
    /// Request a MuSig2 partial signature for a typed channel request.
    ///
    /// When `signer` is `Some`, the signature is computed immediately using the
    /// local signer and returned via `SignerNotification::ChannelSignatureReady`.
    ///
    /// When `signer` is `None`, the request is held in `pending_requests` until
    /// an external signature is submitted via [`SignerActorMessage::SubmitSignature`].
    SignChannel {
        channel_id: Hash256,
        request_id: SignatureRequestId,
        signer: Option<InMemorySigner>,
        request: ChannelSignatureRequest,
        reply_to: ActorRef<ChannelActorMessage>,
    },
    /// Submit an externally generated partial signature for a pending request.
    SubmitSignature {
        channel_id: Hash256,
        request_id: SignatureRequestId,
        partial_signature: PartialSignature,
        next_material: Option<NextChannelSignerMaterial>,
        rpc_reply: Option<RpcReplyPort<Result<SubmitSignatureOutcome, String>>>,
    },
    /// Clear a completed pending request.
    ClearPending {
        channel_id: Hash256,
        request_id: SignatureRequestId,
    },
    /// Request an on-chain transaction ECDSA signature for a watchtower request.
    #[cfg(feature = "watchtower")]
    SignWatchtower {
        node_id: NodeId,
        channel_id: Hash256,
        request_id: Hash256,
        signer: Option<Privkey>,
        content: OnchainSigningContent,
        reply_to: Option<ActorRef<WatchtowerMessage>>,
    },
    /// Submit an externally generated on-chain ECDSA signature for a watchtower request.
    #[cfg(feature = "watchtower")]
    SubmitWatchtowerSignature {
        node_id: NodeId,
        channel_id: Hash256,
        request_id: Hash256,
        signature: [u8; 65],
        rpc_reply: Option<RpcReplyPort<Result<SubmitWatchtowerSignatureResult, String>>>,
    },
    /// Clear a completed watchtower pending request.
    #[cfg(feature = "watchtower")]
    ClearWatchtowerPending {
        node_id: NodeId,
        channel_id: Hash256,
        request_id: Hash256,
    },
}

/// Asynchronous results pushed back to a channel or watchtower actor.
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
        request: ChannelSignatureRequest,
        signature: Result<PartialSignature, String>,
        next_material: Option<NextChannelSignerMaterial>,
        rpc_reply: Option<RpcReplyPort<Result<SubmitSignatureOutcome, String>>>,
    },
    /// The requested watchtower on-chain signature is ready (or failed).
    #[cfg(feature = "watchtower")]
    WatchtowerSignatureReady {
        node_id: NodeId,
        channel_id: Hash256,
        request_id: Hash256,
        signature: Result<[u8; 65], String>,
        rpc_reply: Option<RpcReplyPort<Result<SubmitWatchtowerSignatureResult, String>>>,
    },
}

/// Key identifying one watchtower signing request: (node_id, channel_id, request_id)
#[cfg(feature = "watchtower")]
pub type WatchtowerRequestKey = (NodeId, Hash256, Hash256);

/// Pending watchtower signing request entry: (content, reply_to)
#[cfg(feature = "watchtower")]
pub type PendingWatchtowerRequest = (OnchainSigningContent, Option<ActorRef<WatchtowerMessage>>);

/// Internal state of the SignerActor tracking pending and last-applied signature requests.
#[derive(Default)]
pub struct SignerActorState {
    /// Pending external signature requests: (channel_id, request_id) -> (request, reply_to)
    pub pending_requests: HashMap<
        (Hash256, SignatureRequestId),
        (ChannelSignatureRequest, ActorRef<ChannelActorMessage>),
    >,
    /// Last applied signature request ID per channel for idempotency detection
    pub last_applied: HashMap<Hash256, SignatureRequestId>,

    /// Pending external watchtower signature requests: (node_id, channel_id, request_id) -> (content, reply_to)
    #[cfg(feature = "watchtower")]
    pub pending_watchtower_requests: HashMap<WatchtowerRequestKey, PendingWatchtowerRequest>,
    /// Last applied watchtower signature request ID and signature per (node_id, channel_id)
    #[cfg(feature = "watchtower")]
    pub last_applied_watchtower: HashMap<(NodeId, Hash256), (Hash256, [u8; 65])>,
}

/// Actor dispatching channel signing requests to local or remote signers.
pub struct SignerActor;

#[async_trait::async_trait]
impl Actor for SignerActor {
    type Msg = SignerActorMessage;
    type State = SignerActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _arguments: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(SignerActorState::default())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SignerActorMessage::SignChannel {
                channel_id,
                request_id,
                signer,
                request,
                reply_to,
            } => {
                if let Some(signer) = signer {
                    let signature = sign_channel_request(&signer, &request);
                    let next_material = next_material(&signer, &request);
                    if let Err(error) = &signature {
                        warn!(
                            "SignerActor: signing failed for channel {:?} request {:?}: {}",
                            channel_id, request_id, error
                        );
                    }
                    // Fire-and-forget: the channel actor continues from the
                    // notification handler. A delivery failure means the channel
                    // actor is gone; dropping the result is correct then.
                    let _ = reply_to.send_message(ChannelActorMessage::SignerNotification(
                        SignerNotification::ChannelSignatureReady {
                            channel_id,
                            request_id,
                            request,
                            signature,
                            next_material,
                            rpc_reply: None,
                        },
                    ));
                } else {
                    state
                        .pending_requests
                        .insert((channel_id, request_id), (request, reply_to));
                }
            }
            SignerActorMessage::SubmitSignature {
                channel_id,
                request_id,
                partial_signature,
                next_material,
                rpc_reply,
            } => {
                if let Some((request, reply_to)) =
                    state.pending_requests.get(&(channel_id, request_id))
                {
                    let _ = reply_to.send_message(ChannelActorMessage::SignerNotification(
                        SignerNotification::ChannelSignatureReady {
                            channel_id,
                            request_id,
                            request: request.clone(),
                            signature: Ok(partial_signature),
                            next_material,
                            rpc_reply,
                        },
                    ));
                } else if state.last_applied.get(&channel_id) == Some(&request_id) {
                    if let Some(reply) = rpc_reply {
                        let _ = reply.send(Ok(SubmitSignatureOutcome::AlreadyApplied));
                    }
                } else if let Some(reply) = rpc_reply {
                    let _ = reply.send(Err(
                        "signature request id does not match the current request".to_string(),
                    ));
                }
            }
            SignerActorMessage::ClearPending {
                channel_id,
                request_id,
            } => {
                state.pending_requests.remove(&(channel_id, request_id));
                state.last_applied.insert(channel_id, request_id);
            }
            #[cfg(feature = "watchtower")]
            SignerActorMessage::SignWatchtower {
                node_id,
                channel_id,
                request_id,
                signer,
                content,
                reply_to,
            } => {
                if let Some(signer) = signer {
                    let signature = sign_onchain_request(&signer, &content);
                    if let Err(error) = &signature {
                        warn!(
                            "SignerActor: watchtower signing failed for channel {:?} request {:?}: {}",
                            channel_id, request_id, error
                        );
                    }
                    if let Some(reply_to) = reply_to {
                        let _ = reply_to.send_message(WatchtowerMessage::SignerNotification(
                            SignerNotification::WatchtowerSignatureReady {
                                node_id,
                                channel_id,
                                request_id,
                                signature,
                                rpc_reply: None,
                            },
                        ));
                    }
                } else {
                    state
                        .pending_watchtower_requests
                        .insert((node_id, channel_id, request_id), (content, reply_to));
                }
            }
            #[cfg(feature = "watchtower")]
            SignerActorMessage::SubmitWatchtowerSignature {
                node_id,
                channel_id,
                request_id,
                signature,
                rpc_reply,
            } => {
                if let Some((_content, reply_to)) = state.pending_watchtower_requests.get(&(
                    node_id.clone(),
                    channel_id,
                    request_id,
                )) {
                    if let Some(reply_to) = reply_to {
                        let _ = reply_to.send_message(WatchtowerMessage::SignerNotification(
                            SignerNotification::WatchtowerSignatureReady {
                                node_id,
                                channel_id,
                                request_id,
                                signature: Ok(signature),
                                rpc_reply,
                            },
                        ));
                    } else if let Some(reply) = rpc_reply {
                        state.pending_watchtower_requests.remove(&(
                            node_id.clone(),
                            channel_id,
                            request_id,
                        ));
                        state
                            .last_applied_watchtower
                            .insert((node_id, channel_id), (request_id, signature));
                        let _ = reply.send(Ok(SubmitWatchtowerSignatureResult::Applied));
                    }
                } else if let Some((applied_req, applied_sig)) =
                    state.last_applied_watchtower.get(&(node_id, channel_id))
                {
                    if *applied_req == request_id {
                        if let Some(reply) = rpc_reply {
                            if *applied_sig == signature {
                                let _ =
                                    reply.send(Ok(SubmitWatchtowerSignatureResult::AlreadyApplied));
                            } else {
                                let _ = reply.send(Err(
                                    "submitted signature does not match the previously applied result".to_string(),
                                ));
                            }
                        }
                    } else if let Some(reply) = rpc_reply {
                        let _ = reply.send(Err(
                            "signature request id does not match the current request".to_string(),
                        ));
                    }
                } else if let Some(reply) = rpc_reply {
                    let _ = reply.send(Err(
                        "signature request id does not match the current request".to_string(),
                    ));
                }
            }
            #[cfg(feature = "watchtower")]
            SignerActorMessage::ClearWatchtowerPending {
                node_id,
                channel_id,
                request_id,
            } => {
                state
                    .pending_watchtower_requests
                    .remove(&(node_id, channel_id, request_id));
            }
        }
        Ok(())
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
/// inline local signing path does.
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

/// Sign an on-chain CKB transaction for watchtower settlement/TLC spend using recoverable ECDSA.
#[cfg(feature = "watchtower")]
pub fn sign_onchain_request(
    privkey: &Privkey,
    content: &OnchainSigningContent,
) -> Result<[u8; 65], String> {
    let message = compute_tx_message(&content.transaction);
    let secp_msg = Message::from_digest(message);
    let signature = SECP256K1.sign_ecdsa_recoverable(&secp_msg, &privkey.0);
    let (recov_id, data) = signature.serialize_compact();
    let mut signature_bytes = [0u8; 65];
    signature_bytes[0..64].copy_from_slice(&data[0..64]);
    signature_bytes[64] = i32::from(recov_id) as u8;
    Ok(signature_bytes)
}

/// Verify a 65-byte recoverable ECDSA signature against the expected public key and transaction.
#[cfg(feature = "watchtower")]
pub fn verify_onchain_signature(
    expected_pubkey: &Pubkey,
    content: &OnchainSigningContent,
    signature_bytes: &[u8; 65],
) -> Result<(), String> {
    let message = compute_tx_message(&content.transaction);
    let secp_msg = Message::from_digest(message);
    let recov_id = RecoveryId::try_from(signature_bytes[64] as i32)
        .map_err(|e| format!("invalid recovery id: {e}"))?;
    let sig = RecoverableSignature::from_compact(&signature_bytes[0..64], recov_id)
        .map_err(|e| format!("invalid compact signature: {e}"))?;
    let recovered_pubkey = SECP256K1
        .recover_ecdsa(&secp_msg, &sig)
        .map_err(|e| format!("failed to recover pubkey: {e}"))?;
    if recovered_pubkey.serialize() != expected_pubkey.0 {
        return Err("recovered public key does not match expected public key".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ckb_types::core::TransactionBuilder;
    use fiber_types::{CommitmentCounter, Musig2SignableContent, NonceSlot, Privkey};
    use musig2::{verify_partial, AggNonce, KeyAggContext};

    use super::*;

    /// Minimal actor capturing signer notifications for assertions.
    struct NotificationProbe;

    #[async_trait::async_trait]
    impl Actor for NotificationProbe {
        type Msg = ChannelActorMessage;
        type State = Arc<Mutex<Vec<SignerNotification>>>;
        type Arguments = Arc<Mutex<Vec<SignerNotification>>>;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            captured: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(captured)
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            if let ChannelActorMessage::SignerNotification(notification) = message {
                state.lock().expect("probe lock").push(notification);
            }
            Ok(())
        }
    }

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

    #[tokio::test]
    async fn signs_request_and_notifies_back() {
        let (signer_actor, _handle) = Actor::spawn(None, SignerActor, ())
            .await
            .expect("spawn signer actor");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (probe, _probe_handle) = Actor::spawn(None, NotificationProbe, captured.clone())
            .await
            .expect("spawn probe");

        let signer = InMemorySigner::generate_from_seed(b"signer-actor-test-local");
        let remote = InMemorySigner::generate_from_seed(b"signer-actor-test-remote");
        let channel_id = Hash256::from([7u8; 32]);
        let (request, key_agg_ctx, agg_nonce, message, local_pubnonce) =
            revocation_request(&signer, &remote);
        let request_id = SignatureRequestId(Hash256::from([9u8; 32]));
        signer_actor
            .send_message(SignerActorMessage::SignChannel {
                channel_id,
                request_id,
                signer: Some(signer.clone()),
                request,
                reply_to: probe,
            })
            .expect("sign request");

        // The notification arrives asynchronously; poll briefly.
        let notification = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let mut guard = captured.lock().expect("probe lock");
                    if let Some(notification) = guard.pop() {
                        return notification;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("notification within timeout");

        let SignerNotification::ChannelSignatureReady {
            channel_id: notified_channel,
            request_id: notified_request,
            request: _notified_req,
            next_material,
            signature,
            rpc_reply: _,
        } = notification
        else {
            panic!("expected ChannelSignatureReady");
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

    /// The actor must produce byte-identical signatures to the inline path it
    /// replaces (`ChannelActorState::sign_with_channel_signer` local branch).
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
            commitment_counter: Some(CommitmentCounter::Local),
            key_agg_ctx: key_agg_ctx.clone(),
            agg_nonce: agg_nonce.clone(),
            content: Musig2SignableContent::CommitmentTransaction(unsigned_tx),
        };
        let request = ChannelSignatureRequest::SendRevokeAndAck { content };

        let via_actor = sign_channel_request(&signer, &request).expect("actor signature");

        // Replicate the inline path: same secnonce derivation, same inputs.
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

        assert_eq!(via_actor, inline);
    }

    #[cfg(feature = "watchtower")]
    #[tokio::test]
    async fn signs_watchtower_request_and_verifies() {
        let (signer_actor, _handle) = Actor::spawn(None, SignerActor, ())
            .await
            .expect("spawn signer actor");

        let privkey = Privkey::from(&[99u8; 32]);
        let pubkey = privkey.pubkey();
        let unsigned_tx = TransactionBuilder::default().build().data();
        let content = OnchainSigningContent {
            key_purpose: fiber_types::OnchainKeyPurpose::Settlement,
            transaction: unsigned_tx,
        };

        // 1. Direct signing helper test
        let signature = sign_onchain_request(&privkey, &content).expect("sign onchain ok");
        verify_onchain_signature(&pubkey, &content, &signature).expect("verify onchain ok");

        // Wrong pubkey should fail
        let wrong_pubkey = Privkey::from(&[100u8; 32]).pubkey();
        assert!(verify_onchain_signature(&wrong_pubkey, &content, &signature).is_err());

        // 2. Submit external signature via SignerActor test
        let node_id = NodeId::local();
        let channel_id = Hash256::from([11u8; 32]);
        let request_id = Hash256::from([22u8; 32]);

        // Register pending external request
        signer_actor
            .send_message(SignerActorMessage::SignWatchtower {
                node_id: node_id.clone(),
                channel_id,
                request_id,
                signer: None,
                content: content.clone(),
                reply_to: None,
            })
            .expect("register pending watchtower request");

        // Submit external signature
        let outcome = ractor::call!(signer_actor, |rpc_reply| {
            SignerActorMessage::SubmitWatchtowerSignature {
                node_id: node_id.clone(),
                channel_id,
                request_id,
                signature,
                rpc_reply: Some(rpc_reply),
            }
        })
        .expect("actor call ok")
        .expect("submit signature ok");

        assert_eq!(outcome, SubmitWatchtowerSignatureResult::Applied);

        // Replay submission should return AlreadyApplied
        let replayed = ractor::call!(signer_actor, |rpc_reply| {
            SignerActorMessage::SubmitWatchtowerSignature {
                node_id: node_id.clone(),
                channel_id,
                request_id,
                signature,
                rpc_reply: Some(rpc_reply),
            }
        })
        .expect("actor call ok")
        .expect("replay signature ok");

        assert_eq!(replayed, SubmitWatchtowerSignatureResult::AlreadyApplied);
    }
}
