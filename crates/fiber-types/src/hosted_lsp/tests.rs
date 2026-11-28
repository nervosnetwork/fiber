use super::*;

fn root_pubkey() -> Pubkey {
    secp256k1::SecretKey::from_byte_array(&[42; 32])
        .unwrap()
        .public_key(SECP256K1)
        .into()
}

fn lsp_pubkey() -> Pubkey {
    secp256k1::SecretKey::from_byte_array(&[7; 32])
        .unwrap()
        .public_key(SECP256K1)
        .into()
}

#[test]
fn tenant_id_derivation_has_a_fixed_vector() {
    assert_eq!(
        TenantId::from_root_signer_pubkey(&root_pubkey()).as_str(),
        "5feabe8ac4480f8979ecbf3fb43618dd0d3472eb140235a35ee643d4346b862f"
    );
}

#[test]
fn registry_payload_encoding_and_digest_have_fixed_vectors() {
    let payload =
        TenantRegistryPayload::new(lsp_pubkey(), root_pubkey(), [3; 32], 1_700_000_000_123);
    assert_eq!(
        hex::encode(payload.canonical_bytes()),
        "66696265722d686f737465642d6c73702d74656e616e742d72656769737472792f763202989c0b76cb563971fdc9bef31ec06c3560f3249d6ee9e5d83c57625596e05f6f035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c03030303030303030303030303030303030303030303030303030303030303030000018bcfe5687b"
    );
    assert_eq!(
        hex::encode(payload.digest()),
        "0119b795a736bcbb4d6d9f255500d0584f8702133e6d61c04bfc0a42081e3375"
    );
}

#[test]
fn signed_at_validation_handles_boundaries_and_u64_extremes() {
    let mut payload = TenantRegistryPayload::new(lsp_pubkey(), root_pubkey(), [3; 32], 1_000_000);
    payload
        .validate_signed_at(1_000_000 + TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS)
        .unwrap();
    payload
        .validate_signed_at(1_000_000 - TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS)
        .unwrap();
    assert!(payload
        .validate_signed_at(1_000_000 + TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS + 1)
        .is_err());
    assert!(payload
        .validate_signed_at(1_000_000 - TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS - 1)
        .is_err());

    payload.signed_at = u64::MAX;
    assert!(payload.validate_signed_at(0).is_err());
    payload.signed_at = 0;
    assert!(payload.validate_signed_at(u64::MAX).is_err());
}
