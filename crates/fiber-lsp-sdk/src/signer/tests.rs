use super::*;
use fiber_types::{Hash256, Musig2SigningContent, V2NonceBinding};
use musig2::{AggNonce, KeyAggContext};

fn slot(purpose: NoncePurpose) -> NonceSlot {
    NonceSlot {
        purpose,
        commitment_number: if matches!(
            purpose,
            NoncePurpose::Closing | NoncePurpose::ChannelAnnouncement
        ) {
            0
        } else {
            2
        },
    }
}

#[tokio::test]
async fn opening_material_is_stable_across_concurrent_calls_and_restart() {
    let store = MemoryStore::default();
    let root = RootSigner::create(RootKey::import([23; 32]).unwrap(), store.clone())
        .await
        .unwrap();
    let signer = root.create_channel().await.unwrap();
    let other = root.open_channel(signer.channel_key_id()).await.unwrap();
    let (first, second) = tokio::join!(
        signer.channel_open_material(false),
        other.channel_open_material(false),
    );
    let material = first.unwrap();
    assert_eq!(material, second.unwrap());
    let restored = RootSigner::open(
        RootKey::import([23; 32]).unwrap(),
        MemoryStore::from_snapshot(&store.snapshot().unwrap()).unwrap(),
    )
    .await
    .unwrap();
    let reopened = restored
        .open_channel(signer.channel_key_id())
        .await
        .unwrap();
    assert_eq!(
        material,
        reopened.channel_open_material(false).await.unwrap()
    );
    assert_ne!(material.commitment_nonce, material.v2_nonces[0]);
}

#[tokio::test]
async fn fresh_nonces_survive_restart_and_bind_exact_context() {
    let store = MemoryStore::default();
    let root = RootSigner::create(RootKey::import([19; 32]).unwrap(), store.clone())
        .await
        .unwrap();
    let signer = root.create_channel().await.unwrap();
    let a = signer
        .get_musig2_nonce(slot(NoncePurpose::Commitment))
        .await
        .unwrap()
        .public_nonce;
    let b = signer
        .get_musig2_nonce(slot(NoncePurpose::Commitment))
        .await
        .unwrap()
        .public_nonce;
    let c = signer
        .get_musig2_nonce(slot(NoncePurpose::Closing))
        .await
        .unwrap()
        .public_nonce;
    assert_ne!(a, b);
    assert_ne!(a, c);
    let restored = root.open_channel(signer.channel_key_id()).await.unwrap();
    let (_, state) = restored.load_signing_state().await.unwrap();
    assert_eq!(state.nonces.len(), 3);
    let remote = InMemorySigner::generate_from_seed(b"v2 remote");
    let keys = [
        signer.key_material.funding_key.pubkey(),
        remote.funding_key.pubkey(),
    ];
    let ctx = KeyAggContext::new(keys).unwrap();
    let peer = SecNonceBuilder::new([81; 32])
        .with_seckey(&remote.funding_key)
        .build();
    let content = ChannelSigningContent::Musig2(Musig2SigningContent {
        v2: Some(V2NonceBinding {
            channel_id: Hash256::from([3; 32]),
            owner: keys[1],
            public_nonce: a.clone(),
        }),
        slot: slot(NoncePurpose::Commitment),
        commitment_counter: None,
        key_agg_ctx: ctx.clone(),
        agg_nonce: AggNonce::sum([a.clone(), peer.public_nonce()]),
        content: Musig2SignableContent::CommitmentTransaction(
            ckb_types::packed::Transaction::default(),
        ),
    });
    let data = |content: ChannelSigningContent, revision: u64| SigningData {
        channel_key_id: signer.channel_key_id(),
        state_revision: revision,
        review: SigningReview {
            intent: content.intent(),
            commitment_counter: None,
            commitment_number: Some(2),
            signing_message: content.signing_message(),
            content_hash: content.content_hash(&content.canonical_bytes()).unwrap(),
            canonical_content: content.canonical_bytes(),
            warnings: vec![],
        },
        content,
    };
    let first = restored
        .sign_inner(data(content.clone(), state.revision), None)
        .await
        .unwrap();
    let replay = restored
        .sign_inner(data(content.clone(), state.revision), None)
        .await
        .unwrap();
    assert_eq!(format!("{first:?}"), format!("{replay:?}"));
    let (_, state) = restored.load_signing_state().await.unwrap();
    let ChannelSigningContent::Musig2(mut changed) = content.clone() else {
        unreachable!()
    };
    changed.v2.as_mut().unwrap().owner = keys[0];
    assert!(restored
        .sign_inner(
            data(ChannelSigningContent::Musig2(changed), state.revision),
            None
        )
        .await
        .is_err());
    let ChannelSigningContent::Musig2(mut changed) = content else {
        unreachable!()
    };
    changed.slot.commitment_number += 1;
    assert!(restored
        .sign_inner(
            data(ChannelSigningContent::Musig2(changed), state.revision),
            None
        )
        .await
        .is_err());
}
