use fiber_types::{Privkey, SettlementData, SettlementTlc, TLCId};

use super::{
    to_rpc_musig2_signing_content, to_rpc_signing_settlement, try_into_next_channel_signer_material,
};

#[test]
fn v2_signing_projection_preserves_closing_allocation_and_rejects_missing_binding() {
    let local = Privkey::from(&[1; 32]).pubkey();
    let remote = Privkey::from(&[2; 32]).pubkey();
    let public_nonce = musig2::SecNonce::build([3; 32]).build().public_nonce();
    let binding = fiber_types::V2NonceBinding {
        channel_id: [4; 32].into(),
        owner: local,
        public_nonce: public_nonce.clone(),
    };
    let mut content = fiber_types::Musig2SigningContent {
        v2: Some(binding.clone()),
        slot: fiber_types::NonceSlot {
            purpose: fiber_types::NoncePurpose::Closing,
            commitment_number: 0,
        },
        commitment_counter: None,
        key_agg_ctx: musig2::KeyAggContext::new([local, remote]).unwrap(),
        agg_nonce: musig2::AggNonce::sum([public_nonce]),
        content: fiber_types::Musig2SignableContent::CooperativeCloseTransaction(
            ckb_types::packed::Transaction::default(),
        ),
    };
    let json = to_rpc_musig2_signing_content(content.clone()).unwrap();
    let json_binding = json.v2.unwrap();
    assert_eq!(json_binding.channel_id, binding.channel_id.into());
    assert_eq!(json_binding.owner, local.into());
    assert_eq!(json_binding.public_nonce, binding.public_nonce.serialize());
    assert_eq!(json.slot.purpose, fiber_json_types::NoncePurpose::Closing);
    content.v2 = None;
    assert_eq!(
        to_rpc_musig2_signing_content(content).unwrap_err(),
        "missing V2 external nonce allocation"
    );
}

#[test]
fn v2_followup_material_validates_public_nonce_encodings() {
    let local = Privkey::from(&[1; 32]).pubkey();
    let nonce = musig2::SecNonce::build([3; 32]).build().public_nonce();
    let mut material = fiber_json_types::NextChannelSignerMaterial {
        v2_nonces: vec![nonce.serialize().to_vec()],
        commitment_points: vec![(9, local.into())],
        ..Default::default()
    };
    let internal = try_into_next_channel_signer_material(material.clone()).unwrap();
    assert_eq!(internal.v2_nonces, vec![nonce]);
    assert_eq!(internal.commitment_points, vec![(9, local)]);
    material.v2_nonces.push(vec![0; 1]);
    assert!(try_into_next_channel_signer_material(material).is_err());
}

#[test]
fn signing_snapshot_preserves_ids_keys_and_wallet_relative_direction() {
    let local = Privkey::from(&[1; 32]).pubkey();
    let remote = Privkey::from(&[2; 32]).pubkey();
    for for_remote in [false, true] {
        for inbound in [false, true] {
            let data = SettlementData {
                local_amount: 100,
                remote_amount: 200,
                tlcs: vec![SettlementTlc {
                    tlc_id: if inbound == for_remote {
                        TLCId::Received(42)
                    } else {
                        TLCId::Offered(42)
                    },
                    hash_algorithm: Default::default(),
                    payment_amount: 10,
                    payment_hash: [3; 32].into(),
                    expiry: 10_000,
                    local_key: None,
                    local_key_pubkey: Some(local),
                    local_key_commitment_number: Some(7),
                    remote_key: remote,
                }],
            };
            let json = to_rpc_signing_settlement(data.clone(), local, remote, for_remote).unwrap();
            assert_eq!(json.tlcs[0].inbound, inbound);
            assert_eq!(json.tlcs[0].tlc_id, 42);
            assert_eq!(json.tlcs[0].local_key_commitment_number, 7);
            let (restored, _, _, _) = fiber_lsp_sdk::json::settlement_from_rpc(&json).unwrap();
            assert_eq!(restored, data);
            let mut missing_index = data;
            missing_index.tlcs[0].local_key_commitment_number = None;
            assert_eq!(
                to_rpc_signing_settlement(missing_index, local, remote, for_remote).unwrap_err(),
                "missing TLC key derivation index"
            );
        }
    }
}

#[test]
fn try_into_rpc_script_and_tx_reject_invalid_hash_type_and_dep_type() {
    use ckb_types::{
        packed,
        prelude::{Builder, Entity, Pack},
    };

    let invalid_script = packed::Script::new_builder()
        .hash_type(packed::Byte::new(0xff))
        .build();
    assert!(super::validate_packed_script_hash_type(&invalid_script).is_err());
    assert!(super::try_into_rpc_script(invalid_script.clone()).is_err());

    let invalid_lock_tx = packed::Transaction::new_builder()
        .raw(
            packed::RawTransaction::new_builder()
                .outputs(
                    packed::CellOutputVec::new_builder()
                        .push(
                            packed::CellOutput::new_builder()
                                .lock(invalid_script.clone())
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .build();
    assert!(super::try_into_rpc_transaction(invalid_lock_tx).is_err());

    let invalid_type_tx = packed::Transaction::new_builder()
        .raw(
            packed::RawTransaction::new_builder()
                .outputs(
                    packed::CellOutputVec::new_builder()
                        .push(
                            packed::CellOutput::new_builder()
                                .type_(Some(invalid_script).pack())
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .build();
    assert!(super::try_into_rpc_transaction(invalid_type_tx).is_err());

    let invalid_dep_tx = packed::Transaction::new_builder()
        .raw(
            packed::RawTransaction::new_builder()
                .cell_deps(
                    packed::CellDepVec::new_builder()
                        .push(
                            packed::CellDep::new_builder()
                                .dep_type(packed::Byte::new(0xff))
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .build();
    assert!(super::try_into_rpc_transaction(invalid_dep_tx.clone()).is_err());

    let local = Privkey::from(&[1; 32]).pubkey();
    let remote = Privkey::from(&[2; 32]).pubkey();
    let public_nonce = musig2::SecNonce::build([3; 32]).build().public_nonce();
    let content = fiber_types::Musig2SigningContent {
        v2: Some(fiber_types::V2NonceBinding {
            channel_id: [4; 32].into(),
            owner: local,
            public_nonce: public_nonce.clone(),
        }),
        slot: fiber_types::NonceSlot {
            purpose: fiber_types::NoncePurpose::Closing,
            commitment_number: 0,
        },
        commitment_counter: None,
        key_agg_ctx: musig2::KeyAggContext::new([local, remote]).expect("valid key agg"),
        agg_nonce: musig2::AggNonce::sum([public_nonce]),
        content: fiber_types::Musig2SignableContent::CooperativeCloseTransaction(invalid_dep_tx),
    };
    let err = to_rpc_musig2_signing_content(content).unwrap_err();
    assert!(err.contains("invalid cell_dep dep_type"), "{err}");
}

#[test]
fn v2_signer_material_rpc_enforces_batch_bounds() {
    let pk = Privkey::from(&[1; 32]).pubkey();
    let nonce_bytes = musig2::SecNonce::build([2; 32])
        .build()
        .public_nonce()
        .serialize()
        .to_vec();

    let open_too_many_nonces = fiber_json_types::ChannelOpenSignerMaterial {
        v2_nonces: vec![
            nonce_bytes.clone();
            fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH + 1
        ],
        base_public_keys: fiber_json_types::ChannelBasePublicKeys {
            funding_pubkey: pk.into(),
            tlc_base_key: pk.into(),
        },
        first_commitment_point: pk.into(),
        second_commitment_point: pk.into(),
        commitment_nonce: nonce_bytes.clone(),
        next_commitment_nonce: nonce_bytes.clone(),
        revocation_nonce: nonce_bytes.clone(),
        channel_announcement_nonce: None,
    };
    let err = super::try_into_channel_open_signer_material(open_too_many_nonces).unwrap_err();
    assert!(err.contains("v2_nonces batch size"), "{err}");

    let next_too_many_nonces = fiber_json_types::NextChannelSignerMaterial {
        v2_nonces: vec![nonce_bytes; fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH + 1],
        commitment_points: vec![],
        next_commitment_point: None,
        next_commitment_nonce: None,
        next_revocation_nonce: None,
    };
    let err = try_into_next_channel_signer_material(next_too_many_nonces).unwrap_err();
    assert!(err.contains("v2_nonces batch size"), "{err}");

    let next_too_many_points = fiber_json_types::NextChannelSignerMaterial {
        v2_nonces: vec![],
        commitment_points: (0..=fiber_types::channel_v2_validation::MAX_V2_COMMITMENT_POINT_BATCH
            as u64)
            .map(|n| (n, pk.into()))
            .collect(),
        next_commitment_point: None,
        next_commitment_nonce: None,
        next_revocation_nonce: None,
    };
    let err = try_into_next_channel_signer_material(next_too_many_points).unwrap_err();
    assert!(err.contains("commitment_points batch size"), "{err}");
}
