use super::test_utils::generate_seckey;
use crate::fiber::{
    gen::fiber as molecule_fiber,
    hash_algorithm::HashAlgorithm,
    tests::test_utils::generate_pubkey,
    types::{
        secp256k1_instance, AddTlc, PaymentHopData, PeeledOnionPacket, Privkey, Pubkey, TlcErr,
        TlcErrPacket, TlcErrorCode,
    },
};
use ckb_types::packed::OutPointBuilder;
use ckb_types::prelude::Builder;
use secp256k1::{Secp256k1, SecretKey};
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
    let keys: Vec<Privkey> = std::iter::repeat_with(|| generate_seckey().into())
        .take(3)
        .collect();
    let payment_hash = [1; 32].into();
    let hops_infos = vec![
        PaymentHopData {
            payment_hash,
            amount: 2,
            expiry: 3,
            next_hop: Some(keys[1].pubkey().into()),
            channel_outpoint: Some(OutPointBuilder::default().build().into()),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
        PaymentHopData {
            payment_hash,
            amount: 5,
            expiry: 6,
            next_hop: Some(keys[2].pubkey().into()),
            channel_outpoint: Some(OutPointBuilder::default().build().into()),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
        PaymentHopData {
            payment_hash,
            amount: 8,
            expiry: 9,
            next_hop: None,
            channel_outpoint: Some(OutPointBuilder::default().build().into()),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_preimage: None,
        },
    ];
    let packet = PeeledOnionPacket::create(generate_seckey().into(), hops_infos.clone(), &secp)
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
    let tlc_fail = TlcErrPacket::new(tlc_fail_detail.clone());

    let convert_back: TlcErr = tlc_fail.decode().expect("decoded fail");
    assert_eq!(tlc_fail_detail, convert_back);

    let node_fail =
        TlcErr::new_node_fail(TlcErrorCode::PermanentNodeFailure, generate_pubkey().into());
    assert!(node_fail.error_code.is_node());
    let tlc_fail = TlcErrPacket::new(node_fail.clone());
    let convert_back = tlc_fail.decode().expect("decoded fail");
    assert_eq!(node_fail, convert_back);

    let error_code = TlcErrorCode::PermanentNodeFailure;
    let convert = TlcErrorCode::from_str("PermanentNodeFailure").expect("convert error");
    assert_eq!(error_code, convert);
}
