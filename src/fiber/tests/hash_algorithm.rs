use crate::fiber::hash_algorithm::HashAlgorithm;

#[test]
fn test_hash_algorithm_serialization_sha256() {
    let algorithm = HashAlgorithm::Sha256;
    let serialized = serde_json::to_string(&algorithm).unwrap();
    assert_eq!(serialized, r#""sha256""#);
    let deserialized: HashAlgorithm = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized, algorithm);
}

#[test]
fn test_hash_algorithm_serialization_ckb_hash() {
    let algorithm = HashAlgorithm::CkbHash;
    let serialized = serde_json::to_string(&algorithm).unwrap();
    assert_eq!(serialized, r#""ckb_hash""#);
    let deserialized: HashAlgorithm = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized, algorithm);
}
