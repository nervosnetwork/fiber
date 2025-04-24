use crate::{
    ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtDep, UdtScript},
    fiber::{
        config::AnnouncedNodeName,
        gen::{fiber as molecule_fiber, gossip},
        hash_algorithm::HashAlgorithm,
        types::{
            pack_hop_data, secp256k1_instance, unpack_hop_data, AddTlc, BroadcastMessageID, Cursor,
            Hash256, NodeAnnouncement, PaymentHopData, PeeledOnionPacket, Privkey, Pubkey, TlcErr,
            TlcErrPacket, TlcErrorCode, NO_SHARED_SECRET,
        },
        PaymentCustomRecords,
    },
    gen_deterministic_fiber_private_key, gen_rand_channel_outpoint, gen_rand_fiber_private_key,
    gen_rand_fiber_public_key, now_timestamp_as_millis_u64,
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
use tentacle::multiaddr::MultiAddr;

use std::str::FromStr;

#[test]
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

#[test]
fn test_serde_cursor_node_announcement() {
    let now = 0u64;
    let node_id = gen_rand_fiber_public_key();
    let cursor = Cursor::new(now, BroadcastMessageID::NodeAnnouncement(node_id));
    let moleculed_cursor: gossip::Cursor = cursor.clone().into();
    let unmoleculed_cursor: Cursor = moleculed_cursor.try_into().expect("decode");
    assert_eq!(cursor, unmoleculed_cursor);
}

#[test]
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

#[test]
fn test_serde_cursor_channel_update() {
    let now = 0u64;
    let channel_update_id = gen_rand_channel_outpoint();
    let cursor = Cursor::new(now, BroadcastMessageID::ChannelUpdate(channel_update_id));
    let moleculed_cursor: gossip::Cursor = cursor.clone().into();
    let unmoleculed_cursor: Cursor = moleculed_cursor.try_into().expect("decode");
    assert_eq!(cursor, unmoleculed_cursor);
}

#[test]
fn test_cursor_timestamp() {
    let node_id = gen_rand_fiber_public_key();
    // 255 is larger than 256 in little endian.
    assert!(
        Cursor::new(255, BroadcastMessageID::NodeAnnouncement(node_id))
            < Cursor::new(256, BroadcastMessageID::NodeAnnouncement(node_id))
    );
}

#[test]
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

#[test]
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

#[test]
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
    let packet = PeeledOnionPacket::create(
        gen_rand_fiber_private_key(),
        hops_infos.clone(),
        None,
        &secp,
    )
    .expect("create peeled packet");

    let serialized = packet.serialize();
    let deserialized = PeeledOnionPacket::deserialize(&serialized).expect("deserialize");

    assert_eq!(packet, deserialized);

    assert_eq!(packet.current, hops_infos[0]);
    assert!(!packet.is_last());

    let packet = packet.peel(&keys[1], &secp).expect("peel");
    assert_eq!(packet.current, hops_infos[1]);
    assert!(!packet.is_last());

    let packet = packet.peel(&keys[2], &secp).expect("peel");
    assert_eq!(packet.current, hops_infos[2]);
    assert!(packet.is_last());
}

#[test]
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

#[test]
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

#[test]
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

#[test]
fn test_create_and_verify_node_announcement() {
    let privkey = gen_rand_fiber_private_key();
    let node_announcement = NodeAnnouncement::new(
        AnnouncedNodeName::from_string("node1").expect("valid name"),
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

#[test]
fn test_serde_node_announcement() {
    let privkey = gen_rand_fiber_private_key();
    let node_announcement = NodeAnnouncement::new(
        AnnouncedNodeName::from_string("node1").expect("valid name"),
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
#[test]
fn test_verify_hard_coded_node_announcement() {
    // hard code node announcement 1
    fn node1() -> NodeAnnouncement {
        let privkey = gen_deterministic_fiber_private_key();
        let node_id = privkey.pubkey();
        let mut node_announcement = NodeAnnouncement {
            signature: None,
            features: 0,
            timestamp: 1737451664358,
            node_id,
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
            features: 0,
            timestamp: 1737449487183,
            node_id: privkey.pubkey(),
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
            "75a5419da26e24eace8426ed84180d7c340001c99f94e2657330361126d4f6854cf3c72c2632bf103361fbf3149535077d99e24833164d54b217e4daa2b4def5",
            "8eea99f1c1a541b89c3ea84b5fed1a1311cd9c9e6490c9f9a1393348cf337855",
            node1(),
        ),
        (
            "6bcab4422ae8bf96db20089c04e41d5ba2726bc60ef1c5bc10f3aea7e9f2d30c1f9d3e64a65bdb4878f0953de93c4b4a622bc2e82d37d833472dfe04ecbd56e2",
            "a71050674b30dc5c0355d83f210ccce4ff07a8c4412142659817b0ae2308bd71",
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

#[test]
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

#[test]
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

#[test]
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

#[test]
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
