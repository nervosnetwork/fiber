use super::*;

#[test]
fn v2_followup_material_preserves_fresh_nonce_pool_and_point_indices() {
    let material = NextChannelSignerMaterial {
        v2_nonces: vec![vec![7; 66], vec![8; 66]],
        commitment_points: vec![(42, Pubkey([2; 33]))],
        ..Default::default()
    };
    let value = serde_json::to_value(&material).expect("serialize V2 material");
    assert_eq!(value["v2_nonces"][0], format!("0x{}", "07".repeat(66)));
    assert_eq!(value["commitment_points"][0][0], "0x2a");
    let decoded: NextChannelSignerMaterial =
        serde_json::from_value(value).expect("deserialize V2 material");
    assert_eq!(decoded.v2_nonces, material.v2_nonces);
    assert_eq!(decoded.commitment_points, material.commitment_points);
}

#[test]
fn opening_material_requires_v2_pool_without_old_lsp_fallback() {
    let material = ChannelOpenSignerMaterial {
        v2_nonces: vec![vec![7; 66]],
        base_public_keys: ChannelBasePublicKeys {
            funding_pubkey: Pubkey([2; 33]),
            tlc_base_key: Pubkey([3; 33]),
        },
        first_commitment_point: Pubkey([2; 33]),
        second_commitment_point: Pubkey([3; 33]),
        commitment_nonce: vec![4; 66],
        next_commitment_nonce: vec![5; 66],
        revocation_nonce: vec![6; 66],
        channel_announcement_nonce: None,
    };
    let mut value = serde_json::to_value(&material).expect("serialize opening material");
    let decoded: ChannelOpenSignerMaterial =
        serde_json::from_value(value.clone()).expect("deserialize V2 opening material");
    assert_eq!(decoded.v2_nonces, material.v2_nonces);
    value.as_object_mut().unwrap().remove("v2_nonces");
    assert!(serde_json::from_value::<ChannelOpenSignerMaterial>(value).is_err());
}

#[test]
fn v2_nonce_binding_preserves_channel_owner_and_public_nonce() {
    let binding = V2NonceBinding {
        channel_id: Hash256([1; 32]),
        owner: Pubkey([2; 33]),
        public_nonce: vec![3; 66],
    };
    let encoded = serde_json::to_value(&binding).expect("serialize V2 binding");
    assert_eq!(encoded["public_nonce"], format!("0x{}", "03".repeat(66)));
    assert_eq!(
        serde_json::from_value::<V2NonceBinding>(encoded).expect("deserialize V2 binding"),
        binding
    );
    assert_eq!(
        serde_json::to_value(NoncePurpose::Closing).expect("serialize closing domain"),
        "Closing"
    );
}

#[test]
fn ordinary_external_funding_result_omits_opening_context() {
    let result = OpenChannelWithExternalFundingResult {
        channel_id: Hash256([0; 32]),
        unsigned_funding_tx: Transaction::default(),
        opening_context: None,
    };
    let value = serde_json::to_value(result).expect("serialize ordinary result");
    assert!(value.get("opening_context").is_none());
}
