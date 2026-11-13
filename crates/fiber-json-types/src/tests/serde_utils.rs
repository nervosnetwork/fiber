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
