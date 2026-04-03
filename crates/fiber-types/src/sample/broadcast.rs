use crate::channel::{ChannelUpdateChannelFlags, ChannelUpdateMessageFlags};
use crate::config::UdtCfgInfos;
use crate::protocol::{
    AnnouncedNodeName, BroadcastMessage, ChannelAnnouncement, ChannelUpdate, FeatureVector,
    NodeAnnouncement, SchnorrSignature,
};
use crate::schema::BROADCAST_MESSAGE_PREFIX;
use crate::Hash256;

use super::{
    deterministic_ecdsa_signature, deterministic_pubkey, deterministic_schnorr_signature,
    deterministic_xonly_pubkey, StoreSample,
};

impl StoreSample for BroadcastMessage {
    const STORE_PREFIX: u8 = BROADCAST_MESSAGE_PREFIX;
    const TYPE_NAME: &'static str = "BroadcastMessage";

    fn samples(seed: u64) -> Vec<Self> {
        vec![
            BroadcastMessage::ChannelAnnouncement(ChannelAnnouncement {
                node1_id: deterministic_pubkey(seed, 1),
                node2_id: deterministic_pubkey(seed, 2),
                node1_signature: Some(deterministic_ecdsa_signature(seed, 3)),
                node2_signature: Some(deterministic_ecdsa_signature(seed, 4)),
                ckb_signature: Some(SchnorrSignature(deterministic_schnorr_signature(seed, 5))),
                features: 0,
                chain_hash: Hash256::default(),
                channel_outpoint: super::deterministic_outpoint(seed, 7),
                ckb_key: deterministic_xonly_pubkey(seed, 8),
                capacity: 1_000_000,
                udt_type_script: None,
            }),
            BroadcastMessage::ChannelUpdate(ChannelUpdate {
                signature: Some(deterministic_ecdsa_signature(seed, 10)),
                chain_hash: Hash256::default(),
                channel_outpoint: super::deterministic_outpoint(seed, 11),
                timestamp: 1_704_067_200_000,
                message_flags: ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
                channel_flags: ChannelUpdateChannelFlags::empty(),
                tlc_expiry_delta: 144,
                tlc_minimum_value: 1000,
                tlc_fee_proportional_millionths: 1000,
            }),
            BroadcastMessage::NodeAnnouncement(NodeAnnouncement {
                signature: Some(deterministic_ecdsa_signature(seed, 20)),
                features: FeatureVector::default(),
                timestamp: 1_704_067_200_000,
                node_id: deterministic_pubkey(seed, 21),
                version: "0.7.0".to_string(),
                node_name: AnnouncedNodeName::from_slice(&[0u8; 32]).expect("valid name"),
                addresses: vec![],
                chain_hash: Hash256::default(),
                auto_accept_min_ckb_funding_amount: 62_00000000,
                udt_cfg_infos: UdtCfgInfos::default(),
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_broadcast_message_samples_roundtrip() {
        BroadcastMessage::verify_samples_roundtrip(42);
    }
}
