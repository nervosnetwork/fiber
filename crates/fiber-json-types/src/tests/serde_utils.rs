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
