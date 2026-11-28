use fiber_types::Pubkey;
use musig2::SecNonce;
use secp256k1::{PublicKey, SecretKey, SECP256K1};

use super::*;
use crate::{MemoryStore, RootKey, RootSigner};

#[tokio::test]
async fn open_material_json_is_usable_by_an_rpc_client() {
    let root = RootSigner::create(
        RootKey::import([42; 32]).expect("root key"),
        MemoryStore::default(),
    )
    .await
    .expect("create root");
    let channel = root.create_channel().await.expect("create channel");
    let material = channel
        .channel_open_material(false)
        .await
        .expect("open material");
    let json = serde_json::to_value(open_material_to_rpc(&material)).expect("json");

    assert_eq!(
        json["base_public_keys"]["funding_pubkey"]
            .as_str()
            .expect("funding pubkey")
            .len(),
        66
    );
    assert!(json["commitment_nonce"]
        .as_str()
        .expect("nonce")
        .starts_with("0x"));
    assert!(json["channel_announcement_nonce"].is_null());
}

#[tokio::test]
async fn musig2_rpc_content_round_trips() {
    let root = RootSigner::create(
        RootKey::import([42; 32]).expect("root key"),
        MemoryStore::default(),
    )
    .await
    .expect("create root");
    let channel = root.create_channel().await.expect("create channel");
    let slot = NonceSlot {
        purpose: NoncePurpose::Commitment,
        commitment_number: 1,
    };
    let local_nonce = channel
        .get_musig2_nonce(slot)
        .await
        .expect("local nonce")
        .public_nonce;
    let remote_secret = SecretKey::from_byte_array(&[3u8; 32]).expect("remote secret");
    let remote_pubkey = PublicKey::from_secret_key(SECP256K1, &remote_secret);
    let remote_nonce = SecNonce::build([7u8; 32]).build().public_nonce();
    let content = Musig2SigningContent {
        v2: None,
        slot,
        commitment_counter: Some(CommitmentCounter::Local),
        key_agg_ctx: KeyAggContext::new([
            channel.public_material().base_public_keys.funding_pubkey,
            Pubkey::from(remote_pubkey),
        ])
        .expect("aggregate keys"),
        agg_nonce: AggNonce::sum([local_nonce, remote_nonce]),
        content: Musig2SignableContent::CommitmentTransaction(Default::default()),
    };
    let restored = musig2_from_rpc(musig2_to_rpc(&content)).expect("restore");

    assert_eq!(restored.slot, content.slot);
    assert_eq!(
        restored.key_agg_ctx.serialize(),
        content.key_agg_ctx.serialize()
    );
    assert_eq!(
        restored.agg_nonce.serialize(),
        content.agg_nonce.serialize()
    );
}

#[test]
fn settlement_rpc_round_trips_fields_needed_to_bind_a_commitment() {
    let local = Pubkey::from(PublicKey::from_secret_key(
        SECP256K1,
        &SecretKey::from_byte_array(&[3u8; 32]).expect("local secret"),
    ));
    let remote = Pubkey::from(PublicKey::from_secret_key(
        SECP256K1,
        &SecretKey::from_byte_array(&[4u8; 32]).expect("remote secret"),
    ));
    let payment_hash = fiber_types::Hash256::from([9; 32]);
    let json = JsonSigningSettlement {
        local_amount: 15,
        remote_amount: 1,
        local_settlement_pubkey: local.into(),
        remote_settlement_pubkey: remote.into(),
        for_remote: true,
        tlcs: vec![fiber_json_types::SigningSettlementTlc {
            tlc_id: 42,
            local_key_commitment_number: 3,
            inbound: true,
            payment_hash: payment_hash.into(),
            payment_amount: 5,
            hash_algorithm: fiber_json_types::HashAlgorithm::CkbHash,
            expiry: 1_000,
            local_key_pubkey: local.into(),
            remote_key: remote.into(),
        }],
    };
    let (data, local_key, remote_key, for_remote) =
        settlement_from_rpc(&json).expect("convert settlement");
    assert_eq!(data.local_amount, 15);
    assert_eq!(local_key, local);
    assert_eq!(remote_key, remote);
    assert!(for_remote);
    assert_eq!(data.tlcs[0].payment_hash, payment_hash);
    assert!(matches!(data.tlcs[0].tlc_id, TLCId::Received(42)));
}

#[test]
fn parse_key_agg_ctx_checked_rejects_malformed_and_overflow_counts() {
    // Too short (< 38 bytes).
    assert!(parse_key_agg_ctx_checked(&[]).is_err());
    assert!(parse_key_agg_ctx_checked(&[0u8; 37]).is_err());

    // n_pubkey == 0 (would panic inside KeyAggContext::new if unchecked).
    let mut zero_pubkeys = vec![0u8; 38];
    zero_pubkeys[1..5].copy_from_slice(&0u32.to_be_bytes());
    let err = parse_key_agg_ctx_checked(&zero_pubkeys).unwrap_err();
    assert!(err.contains("non-zero"), "{err}");

    // n_pubkey = 0x07c1_f07d (0x07c1_f07d * 33 wraps around 2^32 on wasm32).
    let mut overflow_no_tweak = vec![0u8; 38];
    overflow_no_tweak[0] = 0b00;
    overflow_no_tweak[1..5].copy_from_slice(&0x07c1_f07du32.to_be_bytes());
    let err = parse_key_agg_ctx_checked(&overflow_no_tweak).unwrap_err();
    assert!(err.contains("length mismatch"), "{err}");

    // Same overflow payload with tweak flag set (header_byte & 0b10 != 0).
    let mut overflow_with_tweak = vec![0u8; 1 + 32 + 4 + 33];
    overflow_with_tweak[0] = 0b10;
    overflow_with_tweak[33..37].copy_from_slice(&0x07c1_f07du32.to_be_bytes());
    let err = parse_key_agg_ctx_checked(&overflow_with_tweak).unwrap_err();
    assert!(err.contains("length mismatch"), "{err}");

    // Valid KeyAggContext round-trips cleanly (with and without tweak).
    let pk1 = Pubkey::from(PublicKey::from_secret_key(
        SECP256K1,
        &SecretKey::from_byte_array(&[3u8; 32]).expect("pk1"),
    ));
    let pk2 = Pubkey::from(PublicKey::from_secret_key(
        SECP256K1,
        &SecretKey::from_byte_array(&[4u8; 32]).expect("pk2"),
    ));
    let ctx = KeyAggContext::new([pk1, pk2]).expect("key agg");
    let serialized = ctx.serialize();
    let parsed = parse_key_agg_ctx_checked(&serialized).expect("valid key agg");
    assert_eq!(parsed.serialize(), serialized);

    // Trailing garbage byte rejected.
    let mut trailing = serialized.clone();
    trailing.push(0);
    assert!(parse_key_agg_ctx_checked(&trailing).is_err());

    // Tweaked KeyAggContext also round-trips cleanly.
    let tweak = SecretKey::from_byte_array(&[9u8; 32]).expect("tweak");
    let tweaked = ctx.with_plain_tweak(tweak).expect("tweaked");
    let tweaked_bytes = tweaked.serialize();
    let parsed_tweaked = parse_key_agg_ctx_checked(&tweaked_bytes).expect("valid tweaked key agg");
    assert_eq!(parsed_tweaked.serialize(), tweaked_bytes);
}
