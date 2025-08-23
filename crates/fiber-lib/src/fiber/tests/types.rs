use crate::{
    ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtDep, UdtScript},
    fiber::{
        amp::{AmpChild, AmpSecret},
        config::AnnouncedNodeName,
        features::FeatureVector,
        gen::{fiber as molecule_fiber, gossip},
        hash_algorithm::HashAlgorithm,
        types::{
            pack_hop_data, secp256k1_instance, unpack_hop_data, AddTlc, AmpPaymentData,
            BasicMppPaymentData, BroadcastMessageID, Cursor, Hash256, NodeAnnouncement, NodeId,
            PaymentHopData, PeeledPaymentOnionPacket, Privkey, Pubkey, TlcErr, TlcErrPacket,
            TlcErrorCode, NO_SHARED_SECRET,
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
use tentacle::{multiaddr::MultiAddr, secio::PeerId};

use std::{collections::HashSet, str::FromStr};

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_serde_public_key() {
    let sk = SecretKey::from_slice(&[42; 32]).unwrap();
    let public_key = Pubkey::from(sk.public_key(secp256k1_instance()));
    let pk_str = serde_json::to_string(&public_key).unwrap();
    assert_eq!(
        "\"035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c\"",
        &pk_str
    );
    let pubkey: Pubkey = serde_json::from_str(&pk_str).unwrap();
    assert_eq!(pubkey, public_key)
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
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 5,
            expiry: 6,
            next_hop: Some(keys[2].pubkey()),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: 8,
            expiry: 9,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
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
                funding_tx_hash: Hash256::default(),
                hash_algorithm: HashAlgorithm::Sha256,
                payment_preimage: None,
                custom_records: None,
            });
        }
        hops_infos.push(PaymentHopData {
            amount: 8,
            expiry: 9,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
            custom_records: None,
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
    build_onion_packet(40).expect("build onion packet with 40 hops");
    let res = build_onion_packet(41);
    assert!(
        res.is_err(),
        "should fail to build onion packet with 41 hops"
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
    .map(|s| Pubkey(PublicKey::from_str(s).expect("valid public key")))
    .collect::<Vec<_>>();

    let session_key = SecretKey::from_slice(&[0x41; 32]).expect("32 bytes, within curve order");
    let hops_ss: Vec<[u8; 32]> =
        OnionSharedSecretIter::new(hops_path.iter().map(|k| &k.0), session_key, &secp).collect();

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
                        hash_type: ScriptHashType::Data1,
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
                        hash_type: ScriptHashType::Data1,
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
            "cfe5d1dde285bac326fb290899c14bd05b9afa013c61c3c9338daca07ab0d287647c2101482bc193913b91da0e9284ebd352f1139149b8943876c78ff46608fc",
            "e387e01002f45594d36577ac53c822f8712b378ed52e8dde3bfbdb3b01b29abc",
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
        next_hop: None,
        funding_tx_hash: Hash256::default(),
        hash_algorithm: HashAlgorithm::Sha256,
        payment_preimage: Some([1; 32].into()),
        custom_records: Some(PaymentCustomRecords {
            data: vec![(1, vec![2, 3])].into_iter().collect(),
        }),
    };

    let data = pack_hop_data(&hop_data);
    let unpacked: PaymentHopData = unpack_hop_data(&data).expect("unpack error");
    assert_eq!(hop_data, unpacked);

    let check_sum = hex::encode(blake2b_256(&data));

    // make sure we don't change PaymentHopData format since it's stored in db with encrypted format
    // do migration with old data version is not workable
    let expected_check_sum =
        "1ea2a67b30c7d2cedab21c6e5f4a3b860fc8b1ccc525f42dd1bdd4a7d6dfe489".to_string();
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
            hash_type: ScriptHashType::Data1,
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
        funding_tx_hash: Hash256::default(),
        hash_algorithm: HashAlgorithm::Sha256,
        payment_preimage: Some([1; 32].into()),
        custom_records: Some(PaymentCustomRecords {
            data: vec![(1, vec![2, 3])].into_iter().collect(),
        }),
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

#[test]
fn test_amp_custom_records() {
    let mut payment_custom_records = PaymentCustomRecords::default();
    let parent_payment_hash = gen_rand_sha256_hash();
    let amp_record = AmpPaymentData::new(parent_payment_hash, 0, 3, AmpSecret::random());
    amp_record.write(&mut payment_custom_records);

    let new_amp_record = AmpPaymentData::read(&payment_custom_records).unwrap();
    assert_eq!(new_amp_record, amp_record);
}

#[test]
fn test_share_xor() {
    let share1 = AmpSecret::random();
    let share2 = AmpSecret::random();
    let result = share1.xor(&share2);

    // XOR is commutative
    assert_eq!(result, share2.xor(&share1));

    // XOR with self should be zero
    assert_eq!(share1.xor(&share1), AmpSecret::zero());
}

#[test]
fn test_derive_child() {
    let root = AmpSecret::random();
    let share = AmpSecret::random();

    let amp_data = AmpPaymentData::new(gen_rand_sha256_hash(), 4, 43, share);
    let child = AmpChild::derive_child(root, amp_data.clone(), HashAlgorithm::Sha256);

    assert_ne!(child.preimage, Hash256::default());
    assert_ne!(child.hash, Hash256::default());

    // Deriving the same child should produce the same result
    let child2 = AmpChild::derive_child(root, amp_data, HashAlgorithm::Sha256);
    assert_eq!(child, child2);
}

#[test]
fn test_different_indices_produce_different_children() {
    let root = AmpSecret::random();
    let share = AmpSecret::random();
    let rand_hash = gen_rand_sha256_hash();

    let child1 = AmpChild::derive_child(
        root,
        AmpPaymentData::new(rand_hash, 0, 2, share),
        HashAlgorithm::Sha256,
    );
    let child2 = AmpChild::derive_child(
        root,
        AmpPaymentData::new(rand_hash, 1, 2, share),
        HashAlgorithm::Sha256,
    );

    assert_ne!(child1.preimage, child2.preimage);
    assert_ne!(child1.hash, child2.hash);
}

#[test]
fn test_reconstruct_children() {
    let root = AmpSecret::random();
    let share1 = AmpSecret::random();
    let share2 = root.xor(&share1); // share2 = root ^ share1

    let desc1 = AmpPaymentData::new(gen_rand_sha256_hash(), 0, 2, share1);
    let desc2 = AmpPaymentData::new(gen_rand_sha256_hash(), 1, 2, share2);

    let children =
        AmpChild::construct_amp_children(&[desc1.clone(), desc2.clone()], HashAlgorithm::Sha256);

    assert_eq!(children.len(), 2);

    // Verify that the children are correctly derived from the reconstructed root
    let expected_child1 = AmpChild::derive_child(root, desc1, HashAlgorithm::Sha256);
    let expected_child2 = AmpChild::derive_child(root, desc2, HashAlgorithm::Sha256);

    assert_eq!(children[0], expected_child1);
    assert_eq!(children[1], expected_child2);
}

#[test]
fn test_reconstruct_single_child() {
    let root = AmpSecret::random();
    let share = root;
    let desc = AmpPaymentData::new(gen_rand_sha256_hash(), 0, 2, share);

    let children = AmpChild::construct_amp_children(&[desc.clone()], HashAlgorithm::Sha256);

    assert_eq!(children.len(), 1);

    // Verify that the child is correctly derived from the reconstructed root
    let expected_child = AmpChild::derive_child(root, desc, HashAlgorithm::Sha256);
    assert_eq!(children[0], expected_child);
}

#[test]
fn test_reconstruct_n_children() {
    let root = AmpSecret::random();
    let shares = AmpSecret::gen_random_sequence(root, 100);
    let descs: Vec<AmpPaymentData> = shares
        .iter()
        .enumerate()
        .map(|(i, &share)| AmpPaymentData::new(gen_rand_sha256_hash(), i as u16, 100, share))
        .collect();

    // last hop will reconstruct children and derive them
    let children = AmpChild::construct_amp_children(&descs.clone(), HashAlgorithm::Sha256);

    assert_eq!(children.len(), descs.len());

    // Verify that each child is correctly derived from the reconstructed root
    for (i, desc) in descs.iter().enumerate() {
        let expected_child = AmpChild::derive_child(root, desc.clone(), HashAlgorithm::Sha256);
        assert_eq!(children[i], expected_child);
    }

    // if we only reconstruct the first 10 children, they should not be the same
    let first_10_children = &descs[0..10];
    let children = AmpChild::construct_amp_children(first_10_children, HashAlgorithm::Sha256);

    // the derived child is not equal to expected child
    for (i, desc) in first_10_children.iter().enumerate() {
        let expected_child = AmpChild::derive_child(root, desc.clone(), HashAlgorithm::Sha256);
        assert_ne!(children[i], expected_child);
    }
}

#[test]
fn test_reconstruct_empty_children() {
    let children = AmpChild::construct_amp_children(&[], HashAlgorithm::Sha256);
    assert!(children.is_empty());
}

#[test]
fn test_part_of_attempt_retry() {
    let root = AmpSecret::random();
    let shares = AmpSecret::gen_random_sequence(root, 5);
    let descs: Vec<AmpPaymentData> = shares
        .iter()
        .enumerate()
        .map(|(i, &share)| AmpPaymentData::new(gen_rand_sha256_hash(), i as u16, 100, share))
        .collect();

    let children = AmpChild::construct_amp_children(&descs.clone(), HashAlgorithm::Sha256);

    assert_eq!(children.len(), descs.len());

    let retried_index = [1, 2];
    let new_attempts_count = 9;

    let old_descs: Vec<_> = descs
        .iter()
        .filter(|d| !retried_index.contains(&d.index))
        .cloned()
        .collect();

    let mut new_root_share = AmpSecret::zero();
    let removed_descs: Vec<_> = descs
        .iter()
        .filter(|d| retried_index.contains(&d.index))
        .collect();
    for d in removed_descs.iter() {
        new_root_share = new_root_share.xor(&d.secret);
    }

    let old_index: HashSet<u16> = old_descs.iter().map(|d| d.index).collect();
    let all_index: HashSet<u16> = (0..new_attempts_count as u16).collect();

    let mut new_descs = old_descs.clone();
    let new_indexes: Vec<_> = all_index.difference(&old_index).collect();
    let new_shares = AmpSecret::gen_random_sequence(new_root_share, new_indexes.len() as u16);
    for (i, share) in new_indexes.iter().zip(new_shares.iter()) {
        let desc = AmpPaymentData::new(gen_rand_sha256_hash(), **i, 100, *share);
        new_descs.push(desc);
    }

    // Verify that each child is correctly derived from the reconstructed root
    let mut original_root_share = AmpSecret::zero();
    for a in new_descs.iter() {
        original_root_share = original_root_share.xor(&a.secret);
    }
    assert_eq!(original_root_share, root);
}
