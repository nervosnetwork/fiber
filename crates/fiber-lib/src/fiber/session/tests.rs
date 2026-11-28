use super::*;
use fiber_types::{Hash256, Privkey};
use musig2::{AggNonce, KeyAggContext};

#[test]
fn test_v2_bound_unsigned_nonce_is_valid_recovery_intent() {
    let a = Privkey::from_slice(&[41; 32]);
    let b = Privkey::from_slice(&[42; 32]);
    let mut record = allocate_nonce(
        Hash256::from([17; 32]),
        a.pubkey(),
        2,
        NoncePurposeV2::Commitment,
        &a,
    );
    let peer = allocate_nonce(
        record.channel_id,
        a.pubkey(),
        2,
        NoncePurposeV2::Commitment,
        &b,
    );
    let nonce = AggNonce::sum([record.public_nonce.clone(), peer.public_nonce]);
    bind_context(&mut record, [a.pubkey(), b.pubkey()], &nonce, &[1; 32]).unwrap();
    assert!(record.signature.is_none());
    assert!(valid_nonce(&record, &a));
}

#[test]
fn test_v2_session_aggregation_guard_and_independent_directions() {
    let a = Privkey::from_slice(&[41; 32]);
    let b = Privkey::from_slice(&[42; 32]);
    let channel = Hash256::from([17; 32]);
    let mut owner = allocate_nonce(channel, b.pubkey(), 2, NoncePurposeV2::Commitment, &b);
    let mut signer = allocate_nonce(channel, b.pubkey(), 2, NoncePurposeV2::Commitment, &a);
    let reverse = allocate_nonce(channel, a.pubkey(), 2, NoncePurposeV2::Commitment, &a);
    let revoke = allocate_nonce(channel, b.pubkey(), 2, NoncePurposeV2::Revocation, &a);
    assert_ne!(signer.public_nonce, reverse.public_nonce);
    assert_ne!(signer.public_nonce, revoke.public_nonce);
    let keys = [a.pubkey(), b.pubkey()];
    let agg = KeyAggContext::new(keys).unwrap();
    let nonce = AggNonce::sum([owner.public_nonce.clone(), signer.public_nonce.clone()]);
    let sa = guarded_sign(&mut signer, &a, keys, &agg, &nonce, b"commitment").unwrap();
    let sb = guarded_sign(&mut owner, &b, keys, &agg, &nonce, b"commitment").unwrap();
    let _: musig2::CompactSignature =
        musig2::aggregate_partial_signatures(&agg, &nonce, [sa, sb], b"commitment").unwrap();
    // Local signing during aggregation must be cached just like a sent CS.
    assert_eq!(
        guarded_sign(&mut owner, &b, keys, &agg, &nonce, b"commitment").unwrap(),
        sb
    );
    assert!(guarded_sign(&mut owner, &b, keys, &agg, &nonce, b"different transaction").is_err());
    let changed_nonce = AggNonce::sum([owner.public_nonce.clone(), reverse.public_nonce.clone()]);
    assert!(guarded_sign(&mut owner, &b, keys, &agg, &changed_nonce, b"commitment").is_err());
    let serialized = bincode::serialize(&owner).unwrap();
    let mut restored = bincode::deserialize(&serialized).unwrap();
    assert_eq!(
        guarded_sign(&mut restored, &b, keys, &agg, &nonce, b"commitment").unwrap(),
        sb
    );
    assert!(guarded_sign(
        &mut restored,
        &b,
        keys,
        &agg,
        &nonce,
        b"different transaction"
    )
    .is_err());
}

fn valid_nonce(record: &SigningNonceV2, key: &Privkey) -> bool {
    record.public_nonce == secret_nonce(record, key).public_nonce()
        && (record.signature.is_none() || record.context.is_some())
}
