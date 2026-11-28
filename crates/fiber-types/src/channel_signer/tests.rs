use super::*;

fn content() -> Musig2SigningContent {
    use musig2::{AggNonce, KeyAggContext, SecNonceBuilder};

    let secret_key = secp256k1::SecretKey::from_byte_array(&[42; 32]).unwrap();
    let public_key = secret_key.public_key(secp256k1::SECP256K1);
    let nonce = SecNonceBuilder::new([7; 32]).build().public_nonce();
    Musig2SigningContent {
        v2: Some(V2NonceBinding {
            channel_id: Hash256::from([5; 32]),
            owner: public_key.into(),
            public_nonce: nonce.clone(),
        }),
        slot: NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 1,
        },
        commitment_counter: None,
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
    let mut state = ChannelSigningContext::default();
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
        Err(ChannelSigningContextError::AlreadyAwaitingSignature)
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
        Err(ChannelSigningContextError::RequestMismatch)
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
    let mut state = ChannelSigningContext::default();
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
fn accepted_signature_retains_continuation_across_serialization() {
    let mut state = ChannelSigningContext::default();
    let request_id = SignatureRequestId(Hash256::from([1; 32]));
    let request = ChannelSignatureRequest::SendCommitmentSigned {
        content: content(),
        settlement_data: settlement_data(),
    };
    state.request_signature(request_id, request).unwrap();
    let accepted = receipt(request_id, 1);
    state.accept_request(accepted.clone()).unwrap();
    let mut restored: ChannelSigningContext =
        bincode::deserialize(&bincode::serialize(&state).unwrap()).unwrap();
    assert_eq!(restored.accepted_signature(), Some(&accepted));
    assert!(restored.replay_or_pending(&accepted).unwrap().is_some());
    assert!(matches!(
        restored.replay_or_pending(&receipt(request_id, 2)),
        Err(ChannelSigningContextError::ResultMismatch)
    ));
    restored.complete_request(accepted.clone()).unwrap();
    assert!(restored.accepted_signature().is_none());
    assert!(restored.replay_or_pending(&accepted).unwrap().is_none());
    restored
        .request_signature(
            SignatureRequestId(Hash256::from([2; 32])),
            ChannelSignatureRequest::SendCommitmentSigned {
                content: content(),
                settlement_data: settlement_data(),
            },
        )
        .unwrap();
    assert!(restored.accepted_signature().is_none());
    assert!(restored.replay_or_pending(&accepted).unwrap().is_none());
}

#[test]
fn cancelling_an_accepted_signature_does_not_acknowledge_it_as_applied() {
    let mut state = ChannelSigningContext::default();
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
    let accepted = receipt(request_id, 1);
    state.accept_request(accepted.clone()).unwrap();
    assert!(state.cancel_request().is_some());
    assert!(state.last_applied().is_none());
    assert!(matches!(
        state.replay_or_pending(&accepted),
        Err(ChannelSigningContextError::NoSignatureRequired)
    ));
}

#[test]
fn cancelling_a_request_clears_awaiting_signature() {
    let mut state = ChannelSigningContext::default();
    let request_id = SignatureRequestId(Hash256::from([1; 32]));
    let request = ChannelSignatureRequest::SendCommitmentSigned {
        content: content(),
        settlement_data: settlement_data(),
    };
    state
        .request_signature(request_id, request.clone())
        .unwrap();
    assert!(state.is_awaiting_signature());
    assert!(matches!(
        state.signing_status(),
        ChannelSigningStatus::SignatureRequired { .. }
    ));

    let cancelled = state.cancel_request().unwrap();
    assert_eq!(cancelled.request_id, request_id);
    assert!(!state.is_awaiting_signature());
    assert!(matches!(
        state.signing_status(),
        ChannelSigningStatus::NoSignatureRequired
    ));
    assert!(matches!(
        state.pending_request(request_id),
        Err(ChannelSigningContextError::NoSignatureRequired)
    ));

    // Can install another request after cancellation
    let next_request_id = SignatureRequestId(Hash256::from([2; 32]));
    state.request_signature(next_request_id, request).unwrap();
    assert!(state.is_awaiting_signature());
}

#[test]
fn identical_retry_is_already_applied_while_waiting_for_the_next_request() {
    let mut state = ChannelSigningContext::default();
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
        Err(ChannelSigningContextError::ResultMismatch)
    ));
}

#[test]
fn replay_of_the_same_request_with_a_different_result_is_rejected() {
    let mut state = ChannelSigningContext::default();
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
        Err(ChannelSigningContextError::ResultMismatch)
    ));
}

#[test]
fn older_request_after_a_later_apply_is_no_longer_pending() {
    let mut state = ChannelSigningContext::default();
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
        Err(ChannelSigningContextError::NoSignatureRequired)
    ));
}

#[test]
fn processing_exposes_pending_request_independently_of_signer() {
    let mut state = ChannelSigningContext::default();
    state
        .request_signature(
            SignatureRequestId(Hash256::from([6; 32])),
            ChannelSignatureRequest::SendCommitmentSigned {
                content: content(),
                settlement_data: settlement_data(),
            },
        )
        .unwrap();
    assert!(state.is_awaiting_signature());
    assert!(matches!(
        state.signing_status(),
        ChannelSigningStatus::SignatureRequired { .. }
    ));
}

#[test]
fn signing_context_roundtrips_while_waiting() {
    let mut state = ChannelSigningContext::default();
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
    let restored: ChannelSigningContext = bincode::deserialize(&encoded).unwrap();
    assert!(matches!(
        restored.signing_status(),
        ChannelSigningStatus::SignatureRequired {
            request_id: id,
            ..
        } if id == request_id
    ));
}

#[test]
fn test_requires_next_signer_material() {
    let dummy_content = content();
    let dummy_settlement = settlement_data();
    let peer_partial_signature = musig2::PartialSignature::from_slice(&[1u8; 32]).unwrap();
    let nonce = musig2::SecNonceBuilder::new([7; 32]).build().public_nonce();
    let pubkey = secp256k1::SecretKey::from_byte_array(&[42; 32])
        .unwrap()
        .public_key(secp256k1::SECP256K1);

    assert!(ChannelSignatureRequest::SendCommitmentSigned {
        content: dummy_content.clone(),
        settlement_data: dummy_settlement.clone(),
    }
    .requires_next_signer_material());

    assert!(ChannelSignatureRequest::CompleteReceivedCommitment {
        content: dummy_content.clone(),
        peer_partial_signature,
        peer_next_commitment_nonce: nonce.clone(),
        settlement_data: dummy_settlement,
    }
    .requires_next_signer_material());

    assert!(ChannelSignatureRequest::SendRevokeAndAck {
        content: dummy_content.clone(),
    }
    .requires_next_signer_material());

    assert!(ChannelSignatureRequest::CompleteReceivedRevokeAndAck {
        content: dummy_content.clone(),
        peer_partial_signature,
        next_per_commitment_point: pubkey.into(),
        next_revocation_nonce: nonce,
    }
    .requires_next_signer_material());

    assert!(!ChannelSignatureRequest::SendClosingSigned {
        content: dummy_content.clone(),
    }
    .requires_next_signer_material());

    assert!(!ChannelSignatureRequest::SignChannelAnnouncement {
        content: dummy_content,
    }
    .requires_next_signer_material());
}
