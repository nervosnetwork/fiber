use ckb_types::{packed::Script, prelude::Pack};
use fiber_types::{
    gen::fiber::{self as molecule_fiber, PubNonceOpt},
    ChannelFlags, Hash256, Privkey, Pubkey,
};
use molecule::prelude::{Builder, Entity};
use musig2::{PartialSignature, PubNonce};

use super::{
    AcceptChannelV2, ChannelReadyV2, ClosingSigned, ClosingSignedV2, CommitmentSigned,
    CommitmentSignedV2, FiberChannelMessage, FiberMessage, OpenChannelV2, ReestablishChannelV2,
    RevokeAndAckV2, ShutdownV2, TxCompleteV2,
};

fn nonce(seed: u8) -> PubNonce {
    musig2::SecNonce::build([seed; 32]).build().public_nonce()
}

fn key(seed: u8) -> Pubkey {
    Privkey::from_slice(&[seed; 32]).pubkey()
}

fn signature() -> PartialSignature {
    PartialSignature::from_slice(&[42; 32]).unwrap()
}

macro_rules! roundtrip {
    ($value:expr, $constructor:ident, $variant:ident, $id:expr) => {{
        let value = $value;
        let message = FiberMessage::$constructor(value.clone());
        let FiberMessage::ChannelNormalOperation(ref channel_message) = message else {
            panic!("wrong domain variant");
        };
        assert_eq!(channel_message.get_channel_id(), value.channel_id);
        assert_eq!(channel_message.to_string(), stringify!($variant));
        let bytes = message.to_molecule_bytes();
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), $id);
        let decoded = FiberMessage::from_molecule_slice(&bytes).unwrap();
        let FiberMessage::ChannelNormalOperation(FiberChannelMessage::$variant(actual)) = decoded
        else {
            panic!("wrong decoded variant");
        };
        assert_eq!(actual.channel_id, value.channel_id);
        assert_eq!(actual, value);
    }};
}

#[test]
fn test_wire_v2_commitment_signed_optional_revocation() {
    for revocation_nonce in [None, Some(nonce(3))] {
        roundtrip!(
            CommitmentSignedV2 {
                channel_id: [11; 32].into(),
                commitment_number: u64::MAX - 1,
                funding_tx_partial_signature: signature(),
                funding_nonce: nonce(2),
                revocation_nonce,
            },
            commitment_signed_v2,
            CommitmentSignedV2,
            21
        );
    }
}

#[test]
fn test_wire_v2_revoke_and_ack() {
    roundtrip!(
        RevokeAndAckV2 {
            channel_id: [12; 32].into(),
            commitment_number: 0x0102030405060708,
            revocation_partial_signature: signature(),
            revocation_nonce: nonce(4),
            next_per_commitment_point: key(5),
            next_commitment_nonce: nonce(6),
        },
        revoke_and_ack_v2,
        RevokeAndAckV2,
        22
    );
}

#[test]
fn test_wire_v2_channel_ready() {
    roundtrip!(
        ChannelReadyV2 {
            channel_id: [13; 32].into(),
            next_commitment_number: u64::MAX,
            next_commitment_nonce: nonce(7),
        },
        channel_ready_v2,
        ChannelReadyV2,
        23
    );
}

#[test]
fn test_wire_v2_reestablish_channel() {
    roundtrip!(
        ReestablishChannelV2 {
            channel_id: [14; 32].into(),
            next_commitment_number: 123,
            next_ack_number: 456,
            next_local_commitment_nonce: nonce(8),
        },
        reestablish_channel_v2,
        ReestablishChannelV2,
        24
    );
}

#[test]
fn test_wire_v2_shutdown() {
    roundtrip!(
        ShutdownV2 {
            channel_id: [15; 32].into(),
            fee_rate: 789,
            close_script: Script::new_builder()
                .code_hash([17; 32].pack())
                .args([1u8, 2, 3].pack())
                .build(),
            closing_nonce: nonce(9),
        },
        shutdown_v2,
        ShutdownV2,
        25
    );
}

#[test]
fn test_wire_v2_closing_signed() {
    roundtrip!(
        ClosingSignedV2 {
            channel_id: [16; 32].into(),
            partial_signature: signature(),
        },
        closing_signed_v2,
        ClosingSignedV2,
        26
    );
}

#[test]
fn test_wire_v2_tx_complete() {
    roundtrip!(
        TxCompleteV2 {
            channel_id: [17; 32].into(),
            initial_commitment_nonce: nonce(10),
        },
        tx_complete_v2,
        TxCompleteV2,
        27
    );
}

#[test]
fn test_wire_v2_rejects_invalid_crypto_fields_and_truncation() {
    let valid = CommitmentSignedV2 {
        channel_id: [11; 32].into(),
        commitment_number: 37,
        funding_tx_partial_signature: signature(),
        funding_nonce: nonce(38),
        revocation_nonce: Some(nonce(39)),
    };
    let encoded = FiberMessage::commitment_signed_v2(valid.clone()).to_molecule_bytes();
    for length in 0..encoded.len() {
        assert!(FiberMessage::from_molecule_slice(&encoded[..length]).is_err());
    }
    let wire: molecule_fiber::CommitmentSignedV2 = valid.into();
    let invalid_nonce = wire
        .clone()
        .as_builder()
        .funding_nonce(molecule_fiber::PubNonce::default())
        .build();
    assert!(CommitmentSignedV2::try_from(invalid_nonce).is_err());
    let invalid_optional_nonce = wire
        .clone()
        .as_builder()
        .revocation_nonce(
            PubNonceOpt::new_builder()
                .set(Some(molecule_fiber::PubNonce::default()))
                .build(),
        )
        .build();
    assert!(CommitmentSignedV2::try_from(invalid_optional_nonce).is_err());
    let invalid_signature = wire
        .as_builder()
        .funding_tx_partial_signature([255; 32].pack())
        .build();
    assert!(CommitmentSignedV2::try_from(invalid_signature).is_err());
    let opening: molecule_fiber::OpenChannelV2 = open_v2().into();
    assert!(OpenChannelV2::try_from(
        opening
            .clone()
            .as_builder()
            .funding_pubkey(molecule_fiber::Pubkey::default())
            .build()
    )
    .is_err());
    assert!(
        OpenChannelV2::try_from(opening.as_builder().channel_flags(255u8.into()).build()).is_err()
    );
}

#[test]
fn test_wire_legacy_roundtrip_remains_v1() {
    let id: Hash256 = [40; 32].into();
    let bytes = FiberMessage::commitment_signed(CommitmentSigned {
        channel_id: id,
        funding_tx_partial_signature: signature(),
        next_commitment_nonce: nonce(41),
    })
    .to_molecule_bytes();
    assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 9);
    let FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSigned(actual)) =
        FiberMessage::from_molecule_slice(&bytes).unwrap()
    else {
        panic!("V1 must remain V1");
    };
    assert_eq!(actual.channel_id, id);
    assert_eq!(actual.funding_tx_partial_signature, signature());
    assert_eq!(actual.next_commitment_nonce, nonce(41));
    let bytes = FiberMessage::closing_signed(ClosingSigned {
        channel_id: id,
        partial_signature: signature(),
    })
    .to_molecule_bytes();
    assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 16);
    let FiberMessage::ChannelNormalOperation(FiberChannelMessage::ClosingSigned(actual)) =
        FiberMessage::from_molecule_slice(&bytes).unwrap()
    else {
        panic!("V1 closing signature must remain V1");
    };
    assert_eq!(actual.channel_id, id);
    assert_eq!(actual.partial_signature, signature());
}

fn open_v2() -> OpenChannelV2 {
    OpenChannelV2 {
        chain_hash: [18; 32].into(),
        channel_id: [19; 32].into(),
        funding_udt_type_script: Some(
            Script::new_builder()
                .code_hash([20; 32].pack())
                .args([4u8, 5].pack())
                .build(),
        ),
        funding_amount: u128::MAX - 1,
        shutdown_script: Script::new_builder().args([6u8, 7].pack()).build(),
        reserved_ckb_amount: 21,
        funding_fee_rate: 22,
        commitment_fee_rate: 23,
        commitment_delay_epoch: 24,
        max_tlc_value_in_flight: u128::MAX - 25,
        max_tlc_number_in_flight: 26,
        funding_pubkey: key(27),
        tlc_basepoint: key(28),
        first_per_commitment_point: key(29),
        second_per_commitment_point: key(30),
        channel_announcement_nonce: Some(nonce(31)),
        initial_commitment_nonce: nonce(32),
        channel_flags: ChannelFlags::PUBLIC | ChannelFlags::ONE_WAY,
        channel_features: 1,
    }
}

fn accept_v2() -> AcceptChannelV2 {
    let open = open_v2();
    AcceptChannelV2 {
        channel_id: open.channel_id,
        funding_amount: open.funding_amount,
        reserved_ckb_amount: open.reserved_ckb_amount,
        max_tlc_value_in_flight: open.max_tlc_value_in_flight,
        max_tlc_number_in_flight: open.max_tlc_number_in_flight,
        funding_pubkey: open.funding_pubkey,
        shutdown_script: open.shutdown_script,
        tlc_basepoint: open.tlc_basepoint,
        first_per_commitment_point: open.first_per_commitment_point,
        second_per_commitment_point: open.second_per_commitment_point,
        channel_announcement_nonce: open.channel_announcement_nonce,
        initial_commitment_nonce: open.initial_commitment_nonce,
        channel_features: 1,
    }
}

#[test]
fn test_wire_v2_opening_fields() {
    for present in [false, true] {
        let mut open = open_v2();
        let mut accept = accept_v2();
        if !present {
            open.funding_udt_type_script = None;
            open.channel_announcement_nonce = None;
            accept.channel_announcement_nonce = None;
        }
        let bytes = FiberMessage::open_channel_v2(open.clone()).to_molecule_bytes();
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 19);
        let FiberMessage::ChannelInitializationV2(actual) =
            FiberMessage::from_molecule_slice(&bytes).unwrap()
        else {
            panic!("wrong opening variant");
        };
        assert_eq!(actual, open);
        roundtrip!(accept, accept_channel_v2, AcceptChannelV2, 20);
    }
}

#[test]
fn test_wire_v2_opening_requires_explicit_confirmation() {
    for features in [0, 2, 255] {
        let mut open = open_v2();
        open.channel_features = features;
        assert!(FiberMessage::from_molecule_slice(
            &FiberMessage::open_channel_v2(open).to_molecule_bytes()
        )
        .is_err());
        let mut accept = accept_v2();
        accept.channel_features = features;
        assert!(FiberMessage::from_molecule_slice(
            &FiberMessage::accept_channel_v2(accept).to_molecule_bytes()
        )
        .is_err());
    }
}

#[test]
fn test_wire_legacy_discriminants_stable() {
    macro_rules! check {
        ($($name:ident = $id:expr),* $(,)?) => { $(
            let wire = molecule_fiber::FiberMessage::new_builder()
                .set(molecule_fiber::$name::default()).build();
            assert_eq!(u32::from_le_bytes(wire.as_slice()[..4].try_into().unwrap()), $id, stringify!($name));
        )* };
    }
    check!(
        Init = 0,
        OpenChannel = 1,
        AcceptChannel = 2,
        TxSignatures = 3,
        TxUpdate = 4,
        TxComplete = 5,
        TxAbort = 6,
        TxInitRBF = 7,
        TxAckRBF = 8,
        CommitmentSigned = 9,
        ChannelReady = 10,
        UpdateTlcInfo = 11,
        AddTlc = 12,
        RemoveTlc = 13,
        RevokeAndAck = 14,
        Shutdown = 15,
        ClosingSigned = 16,
        ReestablishChannel = 17,
        AnnouncementSignatures = 18
    );
}
