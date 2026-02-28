use fiber_types::HashAlgorithm;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_hash_algorithm_serialization_sha256() {
    let algorithm = HashAlgorithm::Sha256;
    let serialized = serde_json::to_string(&algorithm).expect("hash algorithm to json");
    assert_eq!(serialized, r#""sha256""#);
    let deserialized: HashAlgorithm =
        serde_json::from_str(&serialized).expect("hash algorithm from json");
    assert_eq!(deserialized, algorithm);
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_hash_algorithm_serialization_ckb_hash() {
    let algorithm = HashAlgorithm::CkbHash;
    let serialized = serde_json::to_string(&algorithm).expect("hash algorithm to json");
    assert_eq!(serialized, r#""ckb_hash""#);
    let deserialized: HashAlgorithm =
        serde_json::from_str(&serialized).expect("hash algorithm from json");
    assert_eq!(deserialized, algorithm);
}
