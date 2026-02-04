use crate::{
    ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtDep, UdtScript},
    fiber::{
        config::AnnouncedNodeName,
        features::FeatureVector,
        gen::{fiber as molecule_fiber, gossip},
        hash_algorithm::HashAlgorithm,
        types::{
            pack_hop_data, secp256k1_instance, unpack_hop_data, AddTlc, BasicMppPaymentData,
            BroadcastMessageID, Cursor, Hash256, NodeAnnouncement, NodeId, PaymentHopData,
            PeeledPaymentOnionPacket, Privkey, Pubkey, TlcErr, TlcErrData, TlcErrPacket,
            TlcErrorCode, TrampolineHopPayload, TrampolineOnionPacket, NO_SHARED_SECRET,
        },
        PaymentCustomRecords,
    },
    gen_deterministic_fiber_private_key, gen_rand_channel_outpoint, gen_rand_fiber_private_key,
    gen_rand_fiber_public_key, gen_rand_sha256_hash, now_timestamp_as_millis_u64,
};
use ckb_hash::blake2b_256;
use ckb_jsonrpc_types::OutPoint;
use ckb_types::{
    core::{DepType, ScriptHashType},
    prelude::Pack,
    H256,
};
use fiber_sphinx::OnionSharedSecretIter;
use molecule::prelude::{Builder, Byte, Entity};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::Deserialize;
use serde::Serialize;
use std::str::FromStr;
use tentacle::{multiaddr::MultiAddr, secio::PeerId};

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_public_key() {
    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let public_key = Pubkey::from(sk.public_key(secp256k1_instance()));
    let pk_str = serde_json::to_string(&public_key).unwrap();
    // Now Pubkey uses SliceHex which adds "0x" prefix
    assert_eq!(
        "\"0x035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c\"",
        &pk_str
    );
    let pubkey: Pubkey = serde_json::from_str(&pk_str).unwrap();
    assert_eq!(pubkey, public_key)
}

/// Test that Pubkey can be deserialized from hex strings without "0x" prefix
/// for backward compatibility with secp256k1::PublicKey's serde format
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_public_key_without_0x_prefix() {
    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let expected_pubkey = Pubkey::from(sk.public_key(secp256k1_instance()));

    // Old secp256k1::PublicKey format without "0x" prefix
    let pk_str_without_prefix =
        "\"035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c\"";
    let pubkey: Pubkey =
        serde_json::from_str(pk_str_without_prefix).expect("should accept hex without 0x prefix");
    assert_eq!(pubkey, expected_pubkey);

    // Also verify "0x" prefixed still works
    let pk_str_with_prefix =
        "\"0x035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c\"";
    let pubkey2: Pubkey =
        serde_json::from_str(pk_str_with_prefix).expect("should accept hex with 0x prefix");
    assert_eq!(pubkey2, expected_pubkey);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_pubkey_debug_format() {
    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let pubkey = Pubkey::from(sk.public_key(secp256k1_instance()));

    // Debug format should show 33-byte compressed public key in hex
    // This is a BREAKING CHANGE from the old format which showed 64-byte uncompressed coordinates
    let debug_str = format!("{:?}", pubkey);
    assert_eq!(
        debug_str,
        "Pubkey(035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c)"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_cursor_node_announcement() {
    let now = 0u64;
    let node_id = gen_rand_fiber_public_key();
    let cursor = Cursor::new(now, BroadcastMessageID::NodeAnnouncement(node_id));
    let moleculed_cursor: gossip::Cursor = cursor.clone().into();
    let unmoleculed_cursor: Cursor = moleculed_cursor.try_into().expect("decode");
    assert_eq!(cursor, unmoleculed_cursor);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_cursor_channel_announcement() {
    let now = 0u64;
    let channel_announcement_id = gen_rand_channel_outpoint();
    let cursor = Cursor::new(
        now,
        BroadcastMessageID::ChannelAnnouncement(channel_announcement_id),
    );
    let moleculed_cursor: gossip::Cursor = cursor.clone().into();
    let unmoleculed_cursor: Cursor = moleculed_cursor.try_into().expect("decode");
    assert_eq!(cursor, unmoleculed_cursor);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_cursor_channel_update() {
    let now = 0u64;
    let channel_update_id = gen_rand_channel_outpoint();
    let cursor = Cursor::new(now, BroadcastMessageID::ChannelUpdate(channel_update_id));
    let moleculed_cursor: gossip::Cursor = cursor.clone().into();
    let unmoleculed_cursor: Cursor = moleculed_cursor.try_into().expect("decode");
    assert_eq!(cursor, unmoleculed_cursor);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_cursor_timestamp() {
    let node_id = gen_rand_fiber_public_key();
    // 255 is larger than 256 in little endian.
    assert!(
        Cursor::new(255, BroadcastMessageID::NodeAnnouncement(node_id))
            < Cursor::new(256, BroadcastMessageID::NodeAnnouncement(node_id))
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_cursor_types() {
    let node_id = gen_rand_fiber_public_key();
    let channel_outpoint = gen_rand_channel_outpoint();
    assert!(
        Cursor::new(
            0,
            BroadcastMessageID::ChannelAnnouncement(channel_outpoint.clone())
        ) < Cursor::new(0, BroadcastMessageID::NodeAnnouncement(node_id))
    );
    assert!(
        Cursor::new(
            0,
            BroadcastMessageID::ChannelAnnouncement(channel_outpoint.clone())
        ) < Cursor::new(
            0,
            BroadcastMessageID::ChannelUpdate(channel_outpoint.clone())
        )
    );
    assert!(
        Cursor::new(
            0,
            BroadcastMessageID::ChannelUpdate(channel_outpoint.clone())
        ) < Cursor::new(0, BroadcastMessageID::NodeAnnouncement(node_id))
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_add_tlc_serialization() {
    let add_tlc = AddTlc {
        channel_id: [42; 32].into(),
        tlc_id: 42,
        amount: 42,
        payment_hash: [42; 32].into(),
        expiry: 42,
        hash_algorithm: HashAlgorithm::Sha256,
        onion_packet: None,
    };
    let add_tlc_mol: molecule_fiber::AddTlc = add_tlc.clone().into();
    let add_tlc2 = add_tlc_mol.try_into().expect("decode");
    assert_eq!(add_tlc, add_tlc2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet() {
    let secp = Secp256k1::new();
    let keys: Vec<Privkey> = std::iter::repeat_with(gen_rand_fiber_private_key)
        .take(3)
        .collect();
    let hops_infos = vec![
        PaymentHopData {
            amount: 2,
            expiry: 3,
            next_hop: Some(keys[1].pubkey()),
            hash_algorithm: HashAlgorithm::Sha256,
            ..Default::default()
        },
        PaymentHopData {
            amount: 5,
            expiry: 6,
            next_hop: Some(keys[2].pubkey()),
            hash_algorithm: HashAlgorithm::Sha256,
            ..Default::default()
        },
        PaymentHopData {
            amount: 8,
            expiry: 9,
            hash_algorithm: HashAlgorithm::Sha256,
            ..Default::default()
        },
    ];
    let packet = PeeledPaymentOnionPacket::create(
        gen_rand_fiber_private_key(),
        hops_infos.clone(),
        None,
        &secp,
    )
    .expect("create peeled packet");

    let serialized = packet.serialize();
    let deserialized = PeeledPaymentOnionPacket::deserialize(&serialized).expect("deserialize");

    assert_eq!(packet, deserialized);

    assert_eq!(packet.current, hops_infos[0].clone().into());
    assert!(!packet.is_last());

    let packet = packet
        .next
        .expect("next hop")
        .peel(&keys[1], None, &secp)
        .expect("peel");
    assert_eq!(packet.current, hops_infos[1].clone().into());
    assert!(!packet.is_last());

    let packet = packet
        .next
        .expect("next hop")
        .peel(&keys[2], None, &secp)
        .expect("peel");
    assert_eq!(packet.current, hops_infos[2].clone().into());
    assert!(packet.is_last());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_large_onion_packet() {
    fn build_onion_packet(hops_num: usize) -> Result<(), String> {
        let secp = Secp256k1::new();
        let keys: Vec<Privkey> = std::iter::repeat_with(gen_rand_fiber_private_key)
            .take(hops_num + 1)
            .collect();
        let mut hops_infos = vec![];

        for key in keys.iter().take(hops_num) {
            hops_infos.push(PaymentHopData {
                amount: 2,
                expiry: 3,
                next_hop: Some(key.pubkey()),
                hash_algorithm: HashAlgorithm::Sha256,
                ..Default::default()
            });
        }
        hops_infos.push(PaymentHopData {
            amount: 8,
            expiry: 9,
            hash_algorithm: HashAlgorithm::Sha256,
            ..Default::default()
        });

        let packet = PeeledPaymentOnionPacket::create(
            gen_rand_fiber_private_key(),
            hops_infos.clone(),
            None,
            &secp,
        )
        .map_err(|e| format!("create peeled packet error: {}", e))?;

        let serialized = packet.serialize();
        let deserialized = PeeledPaymentOnionPacket::deserialize(&serialized).expect("deserialize");

        assert_eq!(packet, deserialized);

        let mut now = Some(packet);
        for i in 0..hops_infos.len() - 1 {
            let packet = now
                .unwrap()
                .next
                .expect("next hop")
                .peel(&keys[i], None, &secp)
                .expect("peel");
            assert_eq!(packet.current, hops_infos[i + 1].clone().into());
            now = Some(packet.clone());
        }
        let last_packet = now.unwrap();
        assert_eq!(
            last_packet.current,
            hops_infos[hops_infos.len() - 1].clone().into()
        );
        assert!(last_packet.is_last());
        return Ok(());
    }

    // default PACKET_DATA_LEN is 6500
    build_onion_packet(39).expect("build onion packet with 39 hops");
    let res = build_onion_packet(40);
    assert!(
        res.is_err(),
        "should fail to build onion packet with 40 hops"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_trampoline_onion_packet_multi_hop_peel() {
    let secp = Secp256k1::new();

    let t1 = gen_rand_fiber_private_key();
    let t2 = gen_rand_fiber_private_key();
    let final_node = gen_rand_fiber_private_key();
    let session_key = gen_rand_fiber_private_key();

    let payloads = vec![
        TrampolineHopPayload::Forward {
            next_node_id: t2.pubkey(),
            amount_to_forward: 50_000,
            build_max_fee_amount: 0,
            tlc_expiry_delta: 1234,
            max_parts: None,
            hash_algorithm: HashAlgorithm::Sha256,
            tlc_expiry_limit: crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        },
        TrampolineHopPayload::Forward {
            next_node_id: final_node.pubkey(),
            amount_to_forward: 50_000,
            build_max_fee_amount: 0,
            tlc_expiry_delta: 1234,
            max_parts: None,
            hash_algorithm: HashAlgorithm::Sha256,
            tlc_expiry_limit: crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        },
        TrampolineHopPayload::Final {
            final_amount: 50_000,
            final_tlc_expiry_delta: 1234,
            payment_preimage: None,
            custom_records: None,
        },
    ];

    let pkt = TrampolineOnionPacket::create(
        session_key,
        vec![t1.pubkey(), t2.pubkey(), final_node.pubkey()],
        payloads.clone(),
        None,
        &secp,
    )
    .expect("create trampoline onion");

    let p1 = pkt.peel(&t1, None, &secp).expect("peel at t1");
    assert_eq!(p1.current, payloads[0]);
    assert!(p1.next.is_some());

    let p2 = p1
        .next
        .expect("next")
        .peel(&t2, None, &secp)
        .expect("peel at t2");
    assert_eq!(p2.current, payloads[1]);
    assert!(p2.next.is_some());

    let p3 = p2
        .next
        .expect("next")
        .peel(&final_node, None, &secp)
        .expect("peel at final");
    assert_eq!(p3.current, payloads[2]);
    assert!(p3.next.is_none());

    // Cover assoc_data != None cases:
    // - Using the correct assoc_data should succeed.
    // - Using missing/mismatched assoc_data should fail (MAC mismatch).
    let assoc_data = b"fiber-trampoline-assoc-data".to_vec();
    let session_key_with_ad = gen_rand_fiber_private_key();
    let pkt_with_ad = TrampolineOnionPacket::create(
        session_key_with_ad,
        vec![t1.pubkey(), t2.pubkey(), final_node.pubkey()],
        payloads.clone(),
        Some(assoc_data.clone()),
        &secp,
    )
    .expect("create trampoline onion with assoc_data");

    assert!(
        pkt_with_ad.clone().peel(&t1, None, &secp).is_err(),
        "peel should fail when assoc_data is missing"
    );
    assert!(
        pkt_with_ad
            .clone()
            .peel(&t1, Some("wrong".as_bytes()), &secp)
            .is_err(),
        "peel should fail when assoc_data mismatches"
    );

    let p1 = pkt_with_ad
        .peel(&t1, Some(&assoc_data), &secp)
        .expect("peel at t1 with assoc_data");
    assert_eq!(p1.current, payloads[0]);
    assert!(p1.next.is_some());

    let p2 = p1
        .next
        .expect("next")
        .peel(&t2, Some(&assoc_data), &secp)
        .expect("peel at t2 with assoc_data");
    assert_eq!(p2.current, payloads[1]);
    assert!(p2.next.is_some());

    let p3 = p2
        .next
        .expect("next")
        .peel(&final_node, Some(&assoc_data), &secp)
        .expect("peel at final with assoc_data");
    assert_eq!(p3.current, payloads[2]);
    assert!(p3.next.is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet_deserialize_u64_max_overflow() {
    // Length header is u64::MAX, which would cause overflow when adding HOP_DATA_HEAD_LEN
    // Input: [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
    let malicious_input = [255u8, 255, 255, 255, 255, 255, 255, 255, 0];
    let result = PeeledPaymentOnionPacket::deserialize(&malicious_input);
    assert!(
        result.is_err(),
        "Should reject input with overflow-causing length"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet_deserialize_large_claimed_length() {
    // Length header claims more data than available
    let mut large_claim = vec![0u8; 16];
    large_claim[..8].copy_from_slice(&(1000u64).to_be_bytes()); // Claims 1000 bytes
    let result = PeeledPaymentOnionPacket::deserialize(&large_claim);
    assert!(
        result.is_err(),
        "Should reject input claiming more data than available"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet_deserialize_empty_input() {
    let result = PeeledPaymentOnionPacket::deserialize(&[]);
    assert!(result.is_err(), "Should reject empty input");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet_deserialize_short_header() {
    // Input too short for header (need 8 bytes, only 7 provided)
    let result = PeeledPaymentOnionPacket::deserialize(&[1, 2, 3, 4, 5, 6, 7]);
    assert!(
        result.is_err(),
        "Should reject input shorter than header length"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet_deserialize_exceeds_buffer() {
    // Large length that exceeds buffer size
    // Claimed length (6501 + 8 = 6509) far exceeds actual buffer (16 bytes)
    let large_len: u64 = 6501;
    let mut large_input = vec![0u8; 16];
    large_input[..8].copy_from_slice(&large_len.to_be_bytes());
    let result = PeeledPaymentOnionPacket::deserialize(&large_input);
    assert!(
        result.is_err(),
        "Should reject when claimed length exceeds buffer"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_peeled_onion_packet_deserialize_near_max_overflow() {
    // Near-max value that would overflow with header addition
    let near_max = (usize::MAX - 7) as u64; // Adding 8 would overflow
    let mut near_max_input = vec![0u8; 16];
    near_max_input[..8].copy_from_slice(&near_max.to_be_bytes());
    let result = PeeledPaymentOnionPacket::deserialize(&near_max_input);
    assert!(
        result.is_err(),
        "Should reject near-max length that overflows"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_tlc_fail_error() {
    let tlc_fail_detail = TlcErr::new(TlcErrorCode::InvalidOnionVersion);
    assert!(!tlc_fail_detail.error_code.is_node());
    assert!(tlc_fail_detail.error_code.is_bad_onion());
    assert!(tlc_fail_detail.error_code.is_perm());
    let tlc_fail = TlcErrPacket::new(tlc_fail_detail.clone(), &NO_SHARED_SECRET);

    let convert_back: TlcErr = tlc_fail.decode(&[0u8; 32], vec![]).expect("decoded fail");
    assert_eq!(tlc_fail_detail, convert_back);

    let node_fail = TlcErr::new_node_fail(
        TlcErrorCode::PermanentNodeFailure,
        gen_rand_fiber_public_key(),
    );
    assert!(node_fail.error_code.is_node());
    let tlc_fail = TlcErrPacket::new(node_fail.clone(), &NO_SHARED_SECRET);
    let convert_back = tlc_fail.decode(&[0u8; 32], vec![]).expect("decoded fail");
    assert_eq!(node_fail, convert_back);

    let error_code = TlcErrorCode::PermanentNodeFailure;
    let convert = TlcErrorCode::from_str("PermanentNodeFailure").expect("convert error");
    assert_eq!(error_code, convert);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_tlc_err_packet_encryption() {
    // Setup
    let secp = Secp256k1::new();
    let hops_path = [
        "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
        "0324653eac434488002cc06bbfb7f10fe18991e35f9fe4302dbea6d2353dc0ab1c",
        "027f31ebc5462c1fdce1b737ecff52d37d75dea43ce11c74d25aa297165faa2007",
    ]
    .iter()
    .map(|s| {
        let pk = PublicKey::from_str(s).expect("valid public key");
        Pubkey(pk.serialize())
    })
    .collect::<Vec<_>>();

    let session_key = SecretKey::from_slice(&[0x41; 32]).expect("32 bytes, within curve order");
    // Convert [u8; 33] back to PublicKey for OnionSharedSecretIter
    let hops_pubkeys: Vec<PublicKey> = hops_path
        .iter()
        .map(|k| PublicKey::from_slice(&k.0).expect("valid pubkey"))
        .collect();
    let hops_ss: Vec<[u8; 32]> =
        OnionSharedSecretIter::new(hops_pubkeys.iter(), session_key, &secp).collect();

    let tlc_fail_detail = TlcErr::new(TlcErrorCode::InvalidOnionVersion);
    {
        // Error from the first hop
        let tlc_fail = TlcErrPacket::new(tlc_fail_detail.clone(), &hops_ss[0]);
        let decrypted_tlc_fail_detail = tlc_fail
            .decode(session_key.as_ref(), hops_path.clone())
            .expect("decrypted");
        assert_eq!(decrypted_tlc_fail_detail, tlc_fail_detail);
    }

    {
        // Error from the the last hop
        let mut tlc_fail = TlcErrPacket::new(tlc_fail_detail.clone(), &hops_ss[2]);
        tlc_fail = tlc_fail.backward(&hops_ss[1]);
        tlc_fail = tlc_fail.backward(&hops_ss[0]);
        let decrypted_tlc_fail_detail = tlc_fail
            .decode(session_key.as_ref(), hops_path.clone())
            .expect("decrypted");
        assert_eq!(decrypted_tlc_fail_detail, tlc_fail_detail);
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_trampoline_failed_wrapper_is_decodable_by_payer() {
    // Simulate a trampoline boundary wrapping a downstream error packet:
    // - The downstream error packet bytes are opaque to the payer.
    // - The wrapper is encrypted with the *outer* shared secret of the trampoline hop,
    //   so the payer can decode at least the TrampolineFailed envelope.

    let secp = Secp256k1::new();
    let hops_path = [
        "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
        "0324653eac434488002cc06bbfb7f10fe18991e35f9fe4302dbea6d2353dc0ab1c",
    ]
    .iter()
    .map(|s| PublicKey::from_str(s).expect("valid public key").into())
    .collect::<Vec<Pubkey>>();

    let session_key = SecretKey::from_slice(&[0x42; 32]).expect("32 bytes, within curve order");
    let hops_keys: Vec<PublicKey> = hops_path.iter().map(|k| k.into()).collect();
    let hops_ss: Vec<[u8; 32]> =
        OnionSharedSecretIter::new(hops_keys.iter(), session_key, &secp).collect();

    // Pretend the downstream error originated beyond the trampoline boundary.
    let inner_err = TlcErr::new(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
    let inner_err_packet = TlcErrPacket::new(inner_err.clone(), &hops_ss[1]);

    // Trampoline wraps the opaque downstream error bytes.
    let trampoline_node_id = hops_path[0];
    let wrapper_err = TlcErr {
        error_code: inner_err.error_code,
        extra_data: Some(TlcErrData::TrampolineFailed {
            node_id: trampoline_node_id,
            inner_error_packet: inner_err_packet.onion_packet.clone(),
        }),
    };
    let wrapper_packet = TlcErrPacket::new(wrapper_err.clone(), &hops_ss[0]);

    let decoded = wrapper_packet
        .decode(session_key.as_ref(), hops_path.clone())
        .expect("payer decodes wrapper");

    assert_eq!(decoded, wrapper_err);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_tlc_error_code() {
    let code = TlcErrorCode::PermanentNodeFailure;
    let str = code.as_ref().to_string();
    let code2 = TlcErrorCode::from_str(&str).expect("parse");
    assert_eq!(code, code2);

    let code = TlcErrorCode::IncorrectOrUnknownPaymentDetails;
    let code_int: u16 = code.into();
    let code = TlcErrorCode::try_from(code_int).expect("invalid code");
    assert_eq!(code, TlcErrorCode::IncorrectOrUnknownPaymentDetails);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_create_and_verify_node_announcement() {
    let privkey = gen_rand_fiber_private_key();
    let node_announcement = NodeAnnouncement::new(
        AnnouncedNodeName::from_string("node1").expect("valid name"),
        FeatureVector::default(),
        vec![],
        &privkey,
        now_timestamp_as_millis_u64(),
        0,
    );
    assert!(
        node_announcement.verify(),
        "Node announcement message signature verification failed: {:?}",
        &node_announcement
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_node_announcement() {
    let privkey = gen_rand_fiber_private_key();
    let node_announcement = NodeAnnouncement::new(
        AnnouncedNodeName::from_string("node1").expect("valid name"),
        FeatureVector::default(),
        vec![],
        &privkey,
        now_timestamp_as_millis_u64(),
        0,
    );
    assert!(
        node_announcement.verify(),
        "Node announcement verification failed: {:?}",
        &node_announcement
    );
    let serialized = bincode::serialize(&node_announcement).expect("serialize");
    let deserialized: NodeAnnouncement = bincode::deserialize(&serialized).expect("deserialize");
    assert_eq!(node_announcement, deserialized);
    assert!(
        deserialized.verify(),
        "Node announcement verification failed: {:?}",
        &deserialized
    );
}

// There was a bug in the node announcement verification logic which uses local udt whitelist to
// verify the signature. This bug causes different nodes to have different results on signature verification.
// We add a few hard coded node announcements with different udt_cfg_infos to ensure the verification logic is correct.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_verify_hard_coded_node_announcement() {
    // hard code node announcement 1
    fn node1() -> NodeAnnouncement {
        let privkey = gen_deterministic_fiber_private_key();
        let node_id = privkey.pubkey();
        let mut node_announcement = NodeAnnouncement {
            signature: None,
            features: FeatureVector::default(),
            timestamp: 1737451664358,
            node_id,
            version: "1.0".to_string(),
            node_name: AnnouncedNodeName::from_string("fiber-1").expect("valid name"),
            addresses: vec![MultiAddr::from_str(
                "/ip4/127.0.0.1/tcp/8344/p2p/QmbvRjJHAQDmj3cgnUBGQ5zVnGxUKwb2qJygwNs2wk41h8",
            )
            .expect("valid multiaddr")],
            chain_hash: Hash256::from_str(
                "0x9c0a8fff24a7be339b92088730c2dc7fac6dfcbdf0a73774d6d2d6b29523fa5b",
            )
            .expect("valid hash"),
            auto_accept_min_ckb_funding_amount: 10000000000,
            udt_cfg_infos: UdtCfgInfos(vec![
                UdtArgInfo {
                    name: "SIMPLE_UDT".to_string(),
                    script: UdtScript {
                        code_hash: H256::from_str(
                            "e1e354d6d643ad42724d40967e334984534e0367405c5ae42a9d7d63d77df419",
                        )
                        .expect("valid hash"),
                        hash_type: ScriptHashType::Data2,
                        args: "0x.*".to_string(),
                    },
                    auto_accept_amount: Some(1000),
                    cell_deps: vec![UdtDep::with_cell_dep(UdtCellDep {
                        dep_type: DepType::Code,
                        out_point: OutPoint {
                            tx_hash: H256::from_str(
                                "f897bfc51766ee9cdb2b9279e63c8abdba4b35b6ee7dde5fed9b0a5a41c95dc4",
                            )
                            .expect("valid hash"),
                            index: 8.into(),
                        },
                    })],
                },
                UdtArgInfo {
                    name: "XUDT".to_string(),
                    script: UdtScript {
                        code_hash: H256::from_str(
                            "50bd8d6680b8b9cf98b73f3c08faf8b2a21914311954118ad6609be6e78a1b95",
                        )
                        .expect("valid hash"),
                        hash_type: ScriptHashType::Data2,
                        args: "0x.*".to_string(),
                    },
                    auto_accept_amount: Some(1000),
                    cell_deps: vec![UdtDep::with_cell_dep(UdtCellDep {
                        dep_type: DepType::Code,
                        out_point: OutPoint {
                            tx_hash: H256::from_str(
                                "f897bfc51766ee9cdb2b9279e63c8abdba4b35b6ee7dde5fed9b0a5a41c95dc4",
                            )
                            .expect("valid hash"),
                            index: 9.into(),
                        },
                    })],
                },
            ]),
        };
        let signature = privkey.sign(node_announcement.message_to_sign());
        node_announcement.signature = Some(signature);
        node_announcement
    }

    // hard code node announcement 2
    fn node2() -> NodeAnnouncement {
        let privkey = gen_deterministic_fiber_private_key();
        let mut node_announcement = NodeAnnouncement {
            signature: None,
            features: FeatureVector::default(),
            timestamp: 1737449487183,
            node_id: privkey.pubkey(),
            version: "1.0".to_string(),
            node_name: AnnouncedNodeName::default(),
            addresses: vec![MultiAddr::from_str(
                "/ip4/221.187.61.162/tcp/18228/p2p/QmSr3bkMcG9Fy3PAf3HdrxttAE6EiLxHitKJW6HmiV9o6U",
            )
            .unwrap()],
            chain_hash: Hash256::from_str(
                "10639e0895502b5688a6be8cf69460d76541bfa4821629d86d62ba0aae3f9606",
            )
            .unwrap(),
            auto_accept_min_ckb_funding_amount: 10000000000,
            udt_cfg_infos: UdtCfgInfos(vec![UdtArgInfo {
                name: "RUSD".to_string(),
                script: UdtScript {
                    code_hash: H256::from_str(
                        "1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a",
                    )
                    .unwrap(),
                    hash_type: ScriptHashType::Type,
                    args: "0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"
                        .to_string(),
                },
                auto_accept_amount: Some(1000000000),
                cell_deps: vec![UdtDep::with_cell_dep(UdtCellDep {
                    dep_type: DepType::Code,
                    out_point: OutPoint {
                        tx_hash: H256::from_str(
                            "ed7d65b9ad3d99657e37c4285d585fea8a5fcaf58165d54dacf90243f911548b",
                        )
                        .unwrap(),
                        index: 0.into(),
                    },
                })],
            }]),
        };
        let signature = privkey.sign(node_announcement.message_to_sign());
        node_announcement.signature = Some(signature);
        node_announcement
    }

    for (signature, message, node_announcement) in [
        (
            "d5102b528c475e568981c43a8505606333129d4e71142482f59e5bb0a02bc70324d0cdf396eb6dd537c971de34bec77636565f54ded88b9dda53b65570b9ca70",
            "c63db3aec76b6a62e9d563dc35450de058d37047f80dc6c60abad344dd48beba",
            node1(),
        ),
        (
            "1fec23d92c9fc9fafd39f477bf1fbb79cfb8f63604a6aeb0712cfd7dbe31e4e21a174f4e6733e78970f4489859aa1ba615fe712d4d212dd7f1c1a6678dff5d00",
            "3e612fcfa66885352ac18e1fdd602199fb125fa4435ea509f472c0c870b0d307",
            node2(),
        ),
    ] {
        assert_eq!(
            hex::encode(node_announcement.signature.as_ref().unwrap().0.serialize_compact()),
            signature,
            "signature mismatch"
        );
        assert_eq!(
            hex::encode(node_announcement.message_to_sign()),
            message,
            "message mismatch"
        );
        assert!(node_announcement.verify(), "node announcement verification failed");
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_custom_records_serialize_deserialize() {
    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    pub struct Custom {
        pub custom_records: Option<PaymentCustomRecords>,
    }

    let custom = Custom {
        custom_records: Some(PaymentCustomRecords {
            data: vec![(1, vec![2, 3]), (4, vec![5, 33])]
                .into_iter()
                .collect(),
        }),
    };

    let json = serde_json::to_string(&custom).expect("serialize");
    eprintln!("json: {}", json);

    let deserialized: Custom = serde_json::from_str(&json).expect("deserialize");
    eprintln!("deserialized: {:?}", deserialized);
    assert_eq!(custom, deserialized);

    let invalid = "{\"custom_records\":{\"0x4\":\"0x0521\",\"0x1\":\"0x0203\"}}";
    let deserialized = serde_json::from_str::<Custom>(invalid);
    assert!(deserialized.is_err());

    let bincode_serialize = bincode::serialize(&custom).expect("serialize");
    let _deserialized: Custom = bincode::deserialize(&bincode_serialize).expect("deserialize");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_verify_payment_hop_data() {
    let hop_data = PaymentHopData {
        amount: 1000,
        expiry: 1000,
        hash_algorithm: HashAlgorithm::Sha256,
        payment_preimage: Some([1; 32].into()),
        custom_records: Some(PaymentCustomRecords {
            data: vec![(1, vec![2, 3])].into_iter().collect(),
        }),
        ..Default::default()
    };

    let data = pack_hop_data(&hop_data);
    let unpacked: PaymentHopData = unpack_hop_data(&data).expect("unpack error");
    assert_eq!(hop_data, unpacked);

    let check_sum = hex::encode(blake2b_256(&data));

    // make sure we don't change PaymentHopData format since it's stored in db with encrypted format
    // do migration with old data version is not workable
    let expected_check_sum =
        "7abddd1a352a8191bea7b694973a83d1d21c91ce50b2c657521ac83233103ce4".to_string();
    if check_sum != expected_check_sum {
        panic!(
            "PaymentHopData check sum mismatch, you need compatible with old data version when deserializing, \
            migration will not work with PaymentHopData"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_convert_udt_arg_info() {
    let udt_arg_info = UdtArgInfo {
        name: "SIMPLE_UDT".to_string(),
        script: UdtScript {
            code_hash: H256::from_str(
                "e1e354d6d643ad42724d40967e334984534e0367405c5ae42a9d7d63d77df419",
            )
            .expect("valid hash"),
            hash_type: ScriptHashType::Data2,
            args: "0x.*".to_string(),
        },
        auto_accept_amount: Some(1000),
        cell_deps: vec![UdtDep::with_cell_dep(UdtCellDep {
            dep_type: DepType::Code,
            out_point: OutPoint {
                tx_hash: H256::from_str(
                    "f897bfc51766ee9cdb2b9279e63c8abdba4b35b6ee7dde5fed9b0a5a41c95dc4",
                )
                .expect("valid hash"),
                index: 8.into(),
            },
        })],
    };
    let udt_arg_info_gen = molecule_fiber::UdtArgInfo::from(udt_arg_info.clone());
    assert_eq!(udt_arg_info, udt_arg_info_gen.clone().into());

    // 0x80 is not a valid utf-8 string, so it should be converted to empty string
    let udt_arg_info_modified: UdtArgInfo = udt_arg_info_gen
        .as_builder()
        .name([0x80].pack())
        .build()
        .into();
    assert_eq!("", udt_arg_info_modified.name);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_convert_payment_hop_data() {
    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let public_key = Pubkey::from(sk.public_key(secp256k1_instance()));

    let payment_hop_data = PaymentHopData {
        amount: 1000,
        expiry: 1000,
        next_hop: Some(public_key),
        hash_algorithm: HashAlgorithm::Sha256,
        payment_preimage: Some([1; 32].into()),
        custom_records: Some(PaymentCustomRecords {
            data: vec![(1, vec![2, 3])].into_iter().collect(),
        }),
        ..Default::default()
    };
    let payment_hop_data_gen = molecule_fiber::PaymentHopData::from(payment_hop_data.clone());
    assert_eq!(payment_hop_data, payment_hop_data_gen.clone().into());

    // 3 is not a valid hash algorithm, so it should be converted to CkbHash
    let payment_hop_data_modified: PaymentHopData = payment_hop_data_gen
        .clone()
        .as_builder()
        .hash_algorithm(Byte::new(3))
        .build()
        .into();
    assert_eq!(
        HashAlgorithm::CkbHash,
        payment_hop_data_modified.hash_algorithm
    );

    // default pubkey value is [0; 33], it's not a valid public key, so it should be converted to None
    let payment_hop_data_modified: PaymentHopData = payment_hop_data_gen
        .clone()
        .as_builder()
        .next_hop(
            molecule_fiber::PubkeyOpt::new_builder()
                .set(Some(molecule_fiber::Pubkey::default()))
                .build(),
        )
        .build()
        .into();
    assert_eq!(None, payment_hop_data_modified.next_hop);
}

#[test]
fn test_serde_node_id() {
    let peer_id = PeerId::random();
    let expected_str = serde_json::to_string(&peer_id.to_base58()).expect("serialize");
    let node_id = NodeId::from_bytes(peer_id.into_bytes());
    let node_id_str = serde_json::to_string(&node_id).expect("serialize");
    assert_eq!(node_id_str, expected_str, "to base58");
    assert_eq!(
        node_id,
        serde_json::from_str(&node_id_str).unwrap(),
        "to NodeId"
    );
}

#[test]
fn test_basic_mpp_custom_records() {
    let mut payment_custom_records = PaymentCustomRecords::default();
    let payment_secret = gen_rand_sha256_hash();
    let record = BasicMppPaymentData::new(payment_secret, 100);
    record.write(&mut payment_custom_records);

    let new_record = BasicMppPaymentData::read(&payment_custom_records).unwrap();
    assert_eq!(new_record, record);
}

/// Test bincode serialization compatibility of Pubkey
/// This test verifies that the new Pubkey([u8; 33]) format is compatible with
/// the old Pubkey(PublicKey) format when serialized with bincode.
#[test]
fn test_pubkey_bincode_serialization_compatibility() {
    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let secp_pubkey = sk.public_key(secp256k1_instance());
    let pubkey = Pubkey::from(secp_pubkey);

    // Serialize the new Pubkey type
    let serialized = bincode::serialize(&pubkey).expect("serialize pubkey");

    // The old secp256k1::PublicKey serializes using serialize_tuple(33),
    // which in bincode should be 33 bytes (no length prefix for tuples).
    // Our new format using serde_with::Bytes serializes with serialize_bytes,
    // which adds a length prefix in bincode.
    //
    // If this test fails, we need to either:
    // 1. Write a custom SerializeAs implementation that uses serialize_tuple
    // 2. Or write a migration to convert old data

    // Expected old format: just 33 bytes, the compressed public key
    let expected_old_format = secp_pubkey.serialize();
    assert_eq!(expected_old_format.len(), 33);

    // Check if the serialization matches the expected format
    // Note: bincode::serialize for Bytes adds 8-byte length prefix (u64) by default
    if serialized.len() == 33 {
        // Direct match - compatible!
        assert_eq!(
            serialized.as_slice(),
            expected_old_format.as_slice(),
            "Pubkey bincode serialization should match the old format"
        );
    } else if serialized.len() == 41 {
        // Has 8-byte length prefix - NOT compatible with old format!
        // This means we need a migration
        panic!(
            "MIGRATION NEEDED: New Pubkey bincode format has length prefix (41 bytes) \
             but old format was 33 bytes without prefix."
        );
    } else {
        panic!(
            "Unexpected Pubkey bincode serialization length: {} bytes (expected 33 or 41)",
            serialized.len()
        );
    }

    // Also verify deserialization works correctly
    let deserialized: Pubkey = bincode::deserialize(&serialized).expect("deserialize pubkey");
    assert_eq!(deserialized, pubkey);
}

/// Test that old Pubkey(PublicKey) bincode data can be deserialized by new Pubkey([u8; 33])
/// This ensures backward compatibility with existing stored data.
#[test]
fn test_pubkey_bincode_backward_compatibility() {
    // Define the old Pubkey type that wraps secp256k1::PublicKey directly
    #[derive(Copy, Clone, Debug, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
    struct OldPubkey(pub PublicKey);

    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let secp_pubkey = sk.public_key(secp256k1_instance());

    // Create and serialize using old type
    let old_pubkey = OldPubkey(secp_pubkey);
    eprintln!("Old Pubkey: {:?}", old_pubkey);

    let old_serialized = bincode::serialize(&old_pubkey).expect("serialize old pubkey");

    // Verify old format is 33 bytes (no length prefix)
    assert_eq!(
        old_serialized.len(),
        33,
        "Old Pubkey(PublicKey) should serialize to exactly 33 bytes"
    );

    // Deserialize using new Pubkey type - this tests backward compatibility
    let new_pubkey: Pubkey =
        bincode::deserialize(&old_serialized).expect("deserialize old data with new type");

    eprintln!("Deserialized Pubkey: {:?}", new_pubkey);

    // Verify the deserialized data is correct
    assert_eq!(
        new_pubkey.0,
        secp_pubkey.serialize(),
        "Deserialized pubkey bytes should match original"
    );

    // Also verify we can convert back to PublicKey and it matches
    let recovered_pubkey = PublicKey::from_slice(&new_pubkey.0).expect("convert back to PublicKey");
    assert_eq!(
        recovered_pubkey, secp_pubkey,
        "Recovered PublicKey should match original"
    );

    // Test the reverse: new type serialized can be read by old type
    let new_pubkey = Pubkey::from(secp_pubkey);
    let new_serialized = bincode::serialize(&new_pubkey).expect("serialize new pubkey");

    assert_eq!(
        new_serialized.len(),
        33,
        "New Pubkey([u8; 33]) should also serialize to 33 bytes"
    );

    let old_deserialized: OldPubkey =
        bincode::deserialize(&new_serialized).expect("deserialize new data with old type");

    assert_eq!(
        old_deserialized.0, secp_pubkey,
        "Old type should correctly deserialize new format"
    );

    // Verify the serialized bytes are identical
    assert_eq!(
        old_serialized, new_serialized,
        "Old and new serialization formats should be identical"
    );
}
