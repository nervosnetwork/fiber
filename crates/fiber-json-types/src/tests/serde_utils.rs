use crate::serde_utils::Privkey;

#[test]
fn privkey_debug_is_redacted() {
    let raw = [7u8; 32];
    let raw_hex = hex::encode(raw);
    let debug = format!("{:?}", Privkey(raw));

    assert_eq!(debug, "Privkey(<redacted>)");
    assert!(!debug.contains(&raw_hex));
}

#[test]
#[cfg(feature = "watchtower")]
fn channel_features_roundtrip_as_hex() {
    use crate::watchtower::ChannelFeatures;

    let features: ChannelFeatures = serde_json::from_str("\"0x1\"").unwrap();
    assert_eq!(features.0, 1);
    assert_eq!(ChannelFeatures::default().0, 1);
    assert_eq!(serde_json::to_string(&features).unwrap(), "\"0x1\"");
    assert!(serde_json::from_str::<ChannelFeatures>("\"0xz\"").is_err());
}

#[test]
#[cfg(feature = "watchtower")]
fn create_watch_channel_requires_explicit_features() {
    use crate::watchtower::CreateWatchChannelParams;
    use serde_json::json;

    let key = "0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    let mut params = json!({
        "channel_id": format!("0x{}", "00".repeat(32)),
        "funding_udt_type_script": null,
        "local_settlement_key": format!("0x{}01", "00".repeat(31)),
        "remote_settlement_key": key,
        "local_funding_pubkey": key,
        "remote_funding_pubkey": key,
        "settlement_data": {"local_amount": "0x0", "remote_amount": "0x0", "tlcs": []},
        "channel_features": "0x1"
    });
    let decoded: CreateWatchChannelParams = serde_json::from_value(params.clone()).unwrap();
    assert_eq!(decoded.channel_features.0, 1);
    params.as_object_mut().unwrap().remove("channel_features");
    for old_field in [false, true] {
        if old_field {
            params["commitment_contract_features"] = json!("0x0");
        }
        let error = serde_json::from_value::<CreateWatchChannelParams>(params.clone()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing field `channel_features`"),
            "{error}"
        );
    }
}

#[test]
#[cfg(feature = "watchtower")]
fn watchtower_external_funding_status_and_submit_params_serde_and_schema() {
    use crate::channel::{
        OnchainKeyPurpose, OnchainSigningContent, SubmitWatchtowerSignatureParams,
        WatchtowerSigningStatus,
    };
    use serde_json::json;

    let req_id = crate::Hash256([0x11; 32]);
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: ckb_jsonrpc_types::Transaction::default(),
    };
    // 1. `required_expiry_since: None` is omitted from JSON and round-trips cleanly.
    let without_expiry = WatchtowerSigningStatus::ExternalFundingRequired {
        request_id: req_id,
        content: content.clone(),
        expires_at_ms: 0x1b7740,
        required_expiry_since: None,
    };
    let encoded_none = serde_json::to_value(&without_expiry).unwrap();
    assert_eq!(encoded_none["type"], "ExternalFundingRequired");
    assert_eq!(encoded_none["expires_at_ms"], "0x1b7740");
    assert!(encoded_none.get("required_expiry_since").is_none());
    assert_eq!(
        serde_json::from_value::<WatchtowerSigningStatus>(encoded_none).unwrap(),
        without_expiry
    );

    // 2. `required_expiry_since: Some(...)` serializes as U64Hex and round-trips cleanly.
    let expiry_since = 0x4000_0000_0000_0064u64;
    let with_expiry = WatchtowerSigningStatus::ExternalFundingRequired {
        request_id: req_id,
        content,
        expires_at_ms: 0x1b7740,
        required_expiry_since: Some(expiry_since),
    };
    let encoded_some = serde_json::to_value(&with_expiry).unwrap();
    assert_eq!(encoded_some["required_expiry_since"], "0x4000000000000064");
    assert_eq!(
        serde_json::from_value::<WatchtowerSigningStatus>(encoded_some).unwrap(),
        with_expiry
    );

    // 3. SubmitWatchtowerSignatureParams requires `signed_transaction` (rejects missing or null).
    let missing_tx = json!({
        "channel_id": format!("0x{}", "22".repeat(32)),
        "request_id": format!("0x{}", "11".repeat(32)),
        "signature": format!("0x{}", "aa".repeat(65)),
    });
    assert!(serde_json::from_value::<SubmitWatchtowerSignatureParams>(missing_tx).is_err());

    // 4. Schema generation succeeds and exposes ExternalFundingRequired without SignatureRequired.
    let status_schema =
        serde_json::to_string(&schemars::schema_for!(WatchtowerSigningStatus)).unwrap();
    assert!(status_schema.contains("\"ExternalFundingRequired\""));
    assert!(status_schema.contains("\"required_expiry_since\""));
    assert!(!status_schema.contains("\"SignatureRequired\""));
}
