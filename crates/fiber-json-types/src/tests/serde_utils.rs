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
fn commitment_contract_features_roundtrip_as_hex() {
    use crate::watchtower::CommitmentContractFeatures;

    for (hex, bits) in [("0x0", 0), ("0x1", 1)] {
        let features: CommitmentContractFeatures =
            serde_json::from_str(&format!("\"{hex}\"")).unwrap();
        assert_eq!(features.0, bits);
        assert_eq!(
            serde_json::to_string(&features).unwrap(),
            format!("\"{hex}\"")
        );
    }
    assert!(serde_json::from_str::<CommitmentContractFeatures>("\"0xz\"").is_err());
}
