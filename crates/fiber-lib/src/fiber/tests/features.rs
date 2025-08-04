use crate::fiber::features::{
    feature_bits::{GOSSIP_QUERIES_OPTIONAL, GOSSIP_QUERIES_REQUIRED},
    *,
};

#[test]
fn test_feature_bits() {
    let mut vector = FeatureVector::new();

    assert!(vector.is_empty());
    // Set some feature bits
    vector.set_feature(GOSSIP_QUERIES_REQUIRED);

    vector.set_feature(GOSSIP_QUERIES_OPTIONAL);

    // Check if the bits are set correctly
    assert!(vector.requires_feature(GOSSIP_QUERIES_REQUIRED));
    assert!(!vector.requires_feature(GOSSIP_QUERIES_OPTIONAL));

    assert!(vector.supports_feature(GOSSIP_QUERIES_REQUIRED));
    assert!(vector.supports_feature(GOSSIP_QUERIES_OPTIONAL));

    vector.set_basic_mpp_optional();

    assert!(vector.supports_basic_mpp());
    assert!(!vector.requires_basic_mpp());

    vector.set_basic_mpp_required();
    assert!(vector.supports_basic_mpp());
    assert!(vector.requires_basic_mpp());
}

#[test]
fn test_feature_support_and_requires() {
    let mut vector = FeatureVector::new();

    vector.set_basic_mpp_optional();

    assert!(vector.supports_basic_mpp());
    assert!(!vector.requires_basic_mpp());

    vector.set_basic_mpp_required();
    assert!(vector.supports_basic_mpp());
    assert!(vector.requires_basic_mpp());

    vector.unset_basic_mpp_optional();
    assert!(vector.supports_basic_mpp());
    assert!(vector.requires_basic_mpp());

    vector.unset_basic_mpp_required();
    assert!(!vector.supports_basic_mpp());
    assert!(!vector.requires_basic_mpp());
}

#[test]
fn test_feature_vector_names() {
    let mut vector = FeatureVector::new();
    let debug_str = format!("{:?}", vector);
    assert!(debug_str.contains("FeatureVector"));
    assert!(debug_str.contains("features"));

    vector.set_basic_mpp_optional();
    assert_eq!(
        vector.enabled_features_names(),
        ["BASIC_MPP_OPTIONAL".to_string()]
    );
    vector.set_basic_mpp_required();
    assert_eq!(
        vector.enabled_features_names(),
        [
            "BASIC_MPP_REQUIRED".to_string(),
            "BASIC_MPP_OPTIONAL".to_string(),
        ]
    );

    vector.set_gossip_queries_required();
    assert_eq!(
        vector.enabled_features_names(),
        [
            "GOSSIP_QUERIES_REQUIRED".to_string(),
            "BASIC_MPP_REQUIRED".to_string(),
            "BASIC_MPP_OPTIONAL".to_string(),
        ]
    );
    vector.set_gossip_queries_optional();
    assert_eq!(
        vector.enabled_features_names(),
        [
            "GOSSIP_QUERIES_REQUIRED".to_string(),
            "GOSSIP_QUERIES_OPTIONAL".to_string(),
            "BASIC_MPP_REQUIRED".to_string(),
            "BASIC_MPP_OPTIONAL".to_string(),
        ]
    );

    vector.unset_gossip_queries_required();
    assert_eq!(
        vector.enabled_features_names(),
        [
            "GOSSIP_QUERIES_OPTIONAL".to_string(),
            "BASIC_MPP_REQUIRED".to_string(),
            "BASIC_MPP_OPTIONAL".to_string(),
        ]
    );
}

#[test]
fn test_serialize() {
    let mut vector = FeatureVector::new();
    vector.set_basic_mpp_optional();
    vector.set_basic_mpp_required();
    vector.set_gossip_queries_required();
    vector.set_gossip_queries_optional();

    let serialized = bincode::serialize(&vector).expect("Failed to serialize FeatureVector");
    let deserialized: FeatureVector =
        bincode::deserialize(&serialized).expect("Failed to deserialize FeatureVector");

    assert_eq!(vector, deserialized);
}

#[test]
fn test_feature_default() {
    let vector = FeatureVector::default();
    assert!(vector.requires_gossip_queries());
    assert!(vector.supports_gossip_queries());
    assert!(vector.supports_basic_mpp());
    assert!(vector.requires_basic_mpp());
}

#[test]
fn test_feature_random() {
    let mut vector = FeatureVector::new();

    for i in 0..512 {
        vector.set_feature(i as u16);
    }

    for i in 0..512 {
        assert!(vector.supports_feature(i as u16));
        if i % 2 == 0 {
            assert!(vector.requires_feature(i as u16));
        } else {
            assert!(!vector.requires_feature(i as u16));
        }
    }

    let features = vector.enabled_features_names();
    assert_eq!(
        features.last().unwrap().to_string(),
        "Unknown Feature".to_string()
    );

    for i in 0..512 {
        vector.unset_feature(i as u16);
    }

    for i in 0..512 {
        assert!(!vector.requires_feature(i as u16));
        assert!(!vector.supports_feature(i as u16));
    }
    assert!(vector.is_empty());
}

#[test]
fn test_featuer_compatibility() {
    let mut vector = FeatureVector::new();
    let mut vector2 = FeatureVector::new();

    assert!(vector.compatible_with(&vector2));

    vector.set_gossip_queries_required();
    assert!(!vector.compatible_with(&vector2));

    vector2.set_gossip_queries_optional();
    assert!(vector.compatible_with(&vector2));

    vector2.unset_gossip_queries_optional();
    assert!(!vector.compatible_with(&vector2));

    vector2.set_gossip_queries_required();
    assert!(vector.compatible_with(&vector2));
    vector2.unset_gossip_queries_required();
    assert!(!vector.compatible_with(&vector2));
}

#[test]
fn test_feature_serialize_and_deserialize() {
    let mut vector = FeatureVector::new();
    vector.set_gossip_queries_required();
    vector.set_basic_mpp_optional();

    let serialized = bincode::serialize(&vector).expect("Failed to serialize FeatureVector");
    let deserialized: FeatureVector =
        bincode::deserialize(&serialized).expect("Failed to deserialize FeatureVector");

    assert_eq!(vector, deserialized);
    assert!(deserialized.supports_gossip_queries());
    assert!(deserialized.requires_gossip_queries());
    assert!(deserialized.supports_basic_mpp());
}
