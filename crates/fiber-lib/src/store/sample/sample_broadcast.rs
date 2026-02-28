/// StoreSample implementation for `BroadcastMessage`.
use crate::ckb::config::UdtCfgInfos;
use crate::fiber::features::FeatureVector;
use crate::fiber::types::{
    BroadcastMessage, ChannelAnnouncement, ChannelUpdate, ChannelUpdateChannelFlags,
    ChannelUpdateMessageFlags, NodeAnnouncement, SchnorrSignature,
};
use fiber_types::protocol::AnnouncedNodeName;
use fiber_types::schema::BROADCAST_MESSAGE_PREFIX;

use super::{
    deterministic_ecdsa_signature, deterministic_hash256, deterministic_outpoint,
    deterministic_pubkey, deterministic_schnorr_signature, deterministic_xonly_pubkey, StoreSample,
};

impl StoreSample for BroadcastMessage {
    const STORE_PREFIX: u8 = BROADCAST_MESSAGE_PREFIX;
    const TYPE_NAME: &'static str = "BroadcastMessage";

    fn samples(seed: u64) -> Vec<Self> {
        vec![
            sample_node_announcement(seed),
            sample_channel_announcement(seed),
            sample_channel_update(seed),
        ]
    }
}

/// BroadcastMessage::NodeAnnouncement variant with all Options Some.
fn sample_node_announcement(seed: u64) -> BroadcastMessage {
    BroadcastMessage::NodeAnnouncement(NodeAnnouncement {
        signature: Some(deterministic_ecdsa_signature(seed, 0)),
        features: FeatureVector::default(),
        timestamp: 1_704_067_200,
        node_id: deterministic_pubkey(seed, 1),
        version: "0.7.0".to_string(),
        node_name: AnnouncedNodeName::from_string("test-node").expect("valid node name"),
        addresses: vec!["/ip4/127.0.0.1/tcp/8000".parse().expect("valid multiaddr")],
        chain_hash: deterministic_hash256(seed, 2),
        auto_accept_min_ckb_funding_amount: 100_000_000_000,
        udt_cfg_infos: UdtCfgInfos::default(),
    })
}

/// BroadcastMessage::ChannelAnnouncement variant with all Options Some.
fn sample_channel_announcement(seed: u64) -> BroadcastMessage {
    BroadcastMessage::ChannelAnnouncement(ChannelAnnouncement {
        node1_signature: Some(deterministic_ecdsa_signature(seed, 10)),
        node2_signature: Some(deterministic_ecdsa_signature(seed, 11)),
        ckb_signature: Some(SchnorrSignature(deterministic_schnorr_signature(seed, 12))),
        features: 0,
        chain_hash: deterministic_hash256(seed, 13),
        channel_outpoint: deterministic_outpoint(seed, 14),
        node1_id: deterministic_pubkey(seed, 15),
        node2_id: deterministic_pubkey(seed, 16),
        ckb_key: deterministic_xonly_pubkey(seed, 17),
        capacity: 800_000_000_000,
        udt_type_script: Some(ckb_types::packed::Script::default()),
    })
}

/// BroadcastMessage::ChannelUpdate variant with signature Some.
fn sample_channel_update(seed: u64) -> BroadcastMessage {
    BroadcastMessage::ChannelUpdate(ChannelUpdate {
        signature: Some(deterministic_ecdsa_signature(seed, 20)),
        chain_hash: deterministic_hash256(seed, 21),
        channel_outpoint: deterministic_outpoint(seed, 22),
        timestamp: 1_704_070_800,
        message_flags: ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
        channel_flags: ChannelUpdateChannelFlags::empty(),
        tlc_expiry_delta: 40,
        tlc_minimum_value: 1000,
        tlc_fee_proportional_millionths: 500,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_broadcast_message_samples_roundtrip() {
        BroadcastMessage::verify_samples_roundtrip(42);
    }

    #[test]
    fn test_broadcast_message_samples_deterministic() {
        let bytes_a = BroadcastMessage::sample_bytes(42);
        let bytes_b = BroadcastMessage::sample_bytes(42);
        assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
    }

    #[test]
    fn test_broadcast_message_different_seeds() {
        let bytes_42 = BroadcastMessage::sample_bytes(42);
        let bytes_99 = BroadcastMessage::sample_bytes(99);
        assert_ne!(
            bytes_42, bytes_99,
            "Different seeds should produce different bytes"
        );
    }

    #[test]
    fn test_broadcast_message_sample_count() {
        let samples = BroadcastMessage::samples(42);
        assert_eq!(samples.len(), 3, "Should produce all 3 enum variants");
    }

    #[test]
    fn test_broadcast_message_options_populated() {
        let samples = BroadcastMessage::samples(42);
        // NodeAnnouncement
        if let BroadcastMessage::NodeAnnouncement(na) = &samples[0] {
            assert!(na.signature.is_some());
        } else {
            panic!("Expected NodeAnnouncement");
        }
        // ChannelAnnouncement
        if let BroadcastMessage::ChannelAnnouncement(ca) = &samples[1] {
            assert!(ca.node1_signature.is_some());
            assert!(ca.node2_signature.is_some());
            assert!(ca.ckb_signature.is_some());
            assert!(ca.udt_type_script.is_some());
        } else {
            panic!("Expected ChannelAnnouncement");
        }
        // ChannelUpdate
        if let BroadcastMessage::ChannelUpdate(cu) = &samples[2] {
            assert!(cu.signature.is_some());
        } else {
            panic!("Expected ChannelUpdate");
        }
    }
}
