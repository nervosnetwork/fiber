use crate::{
    fiber::{
        config::AnnouncedNodeName,
        gen::{fiber as molecule_fiber, gossip},
        hash_algorithm::HashAlgorithm,
        types::{
            secp256k1_instance, AddTlc, BroadcastMessage, BroadcastMessageID, Cursor, Hash256,
            NodeAnnouncement, PaymentHopData, PeeledOnionPacket, Privkey, Pubkey, TlcErr,
            TlcErrPacket, TlcErrorCode, NO_SHARED_SECRET,
        },
    },
    gen_rand_channel_outpoint, gen_rand_fiber_private_key, gen_rand_fiber_public_key,
    now_timestamp_as_millis_u64,
};
use fiber_sphinx::OnionSharedSecretIter;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
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
        Cursor::new(255, BroadcastMessageID::NodeAnnouncement(node_id.clone()))
            < Cursor::new(256, BroadcastMessageID::NodeAnnouncement(node_id.clone()))
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
        ) < Cursor::new(0, BroadcastMessageID::NodeAnnouncement(node_id.clone()))
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
        ) < Cursor::new(0, BroadcastMessageID::NodeAnnouncement(node_id.clone()))
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
    let keys: Vec<Privkey> = std::iter::repeat_with(|| gen_rand_fiber_private_key())
        .take(3)
        .collect();
    let hops_infos = vec![
        PaymentHopData {
            amount: 2,
            expiry: 3,
            next_hop: Some(keys[1].pubkey().into()),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
        PaymentHopData {
            amount: 5,
            expiry: 6,
            next_hop: Some(keys[2].pubkey().into()),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
        PaymentHopData {
            amount: 8,
            expiry: 9,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
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
        OnionSharedSecretIter::new(hops_path.iter().map(|k| &k.0), session_key.clone(), &secp)
            .collect();

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
        AnnouncedNodeName::from_str("node1").expect("valid name"),
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
        AnnouncedNodeName::from_str("node1").expect("valid name"),
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
    for s in [
        "000000000146000000000000003044022015c1b36c0f5d08cbcb7ac77939506495cbe6dbd4bdd6076de54e8cabe707f894022003c4e84c69e88906499e00e0bc9e601c727bead819233c0e1a2f66c59385f11f0000000000000000e67330889401000002a64b8993f33b2ebd37a4de1c9441f491291a4e779da8e519bcfb7c1f3f56c9c0200000000000000066696265722d310000000000000000000000000000000000000000000000000001000000000000002d00000000000000047f000001062098a503221220c9cf006bbaa881b6962c3a61f4dc7100aaedd875253d1bbb78408e2be7f5c93f420000000000000030783963306138666666323461376265333339623932303838373330633264633766616336646663626466306137333737346436643264366232393532336661356200e40b540200000002000000000000000a0000000000000053494d504c455f554454420000000000000030786531653335346436643634336164343237323464343039363765333334393834353334653033363734303563356165343261396437643633643737646634313905000000000000006461746131040000000000000030782e2a01e803000000000000000000000000000001000000000000000400000000000000636f6465420000000000000030786638393762666335313736366565396364623262393237396536336338616264626134623335623665653764646535666564396230613561343163393564633408000000040000000000000058554454420000000000000030783530626438643636383062386239636639386237336633633038666166386232613231393134333131393534313138616436363039626536653738613162393505000000000000006461746131040000000000000030782e2a01e803000000000000000000000000000001000000000000000400000000000000636f6465420000000000000030786638393762666335313736366565396364623262393237396536336338616264626134623335623665653764646535666564396230613561343163393564633409000000",
        "000000000146000000000000003044022052158ADBFCEA30AEAF89CA00200DF0CC3D1E593EE635DB7FBFF01A28A34D07CF022065FD67E565540A1EFE99B939D2F084CBACEFCF15F2B17AA52CFFEE0614819C7B00000000000000004F3B0F889401000003781A50829680593CD47EDCCB646E62625212CF9AEA83EF4BE421A2B2C08872102000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000002D0000000000000004DDBB3DA2064734A50322122042F6793087F4481CEA9768D563FD8EBE6FEDA11E6A409CD84E92094CA69CE955420000000000000030783130363339653038393535303262353638386136626538636636393436306437363534316266613438323136323964383664363262613061616533663936303600E40B54020000000100000000000000040000000000000052555344420000000000000030783131343237353561303434626632656533353863626139663264613138376365393238633931636434646338363932646564303333376566613637376432316104000000000000007479706542000000000000003078383738666363366631663038643438653837626231633362336435303833663233663861333963356435633736346632353362353562393938353236343339620100CA9A3B00000000000000000000000001000000000000000400000000000000636F6465420000000000000030786564376436356239616433643939363537653337633432383564353835666561386135666361663538313635643534646163663930323433663931313534386200000000"
    ] {
        let bytes = hex::decode(s).expect("decode");

        let node_announcement = match bincode::deserialize(&bytes).expect("deserialize") {
            BroadcastMessage::NodeAnnouncement(node_announcement) => node_announcement,
            _ => panic!("deserialize failed"),
        };
        assert!(node_announcement.verify())
    }
}
