//! Layer 2: Protocol message types and supporting types.
//!
//! Contains gossip protocol types (ChannelAnnouncement, ChannelUpdate, NodeAnnouncement,
//! BroadcastMessage) and their supporting types (EcdsaSignature, AnnouncedNodeName,
//! FeatureVector, SchnorrSignature).

use crate::channel::{ChannelUpdateChannelFlags, ChannelUpdateMessageFlags};
use crate::gen::fiber as molecule_fiber;
use crate::gen::gossip as molecule_gossip;
use crate::primitives::u8_32_as_byte_32;
use crate::serde_utils::EntityHex;
use crate::UdtCfgInfos;
use crate::{Hash256, Privkey, Pubkey};
use ckb_types::packed::{BytesVec, OutPoint, Script};
use ckb_types::prelude::Pack;
use molecule::prelude::{Builder, Byte, Entity};
use musig2::LiftedSignature;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::serde_as;
use std::cmp::Ordering;

pub use feature_bits::*;

static CHAIN_HASH_INSTANCE: OnceCell<Hash256> = OnceCell::new();

pub fn init_chain_hash(chain_hash: Hash256) {
    CHAIN_HASH_INSTANCE
        .set(chain_hash)
        .expect("init_chain_hash should only be called once");
}

pub fn get_chain_hash() -> Hash256 {
    CHAIN_HASH_INSTANCE.get().cloned().unwrap_or_default()
}

pub fn check_chain_hash(chain_hash: &Hash256) -> Result<(), String> {
    let expected = get_chain_hash();
    if chain_hash == &expected {
        Ok(())
    } else {
        Err(format!(
            "chain hash mismatch: expected {:?}, got {:?}",
            expected, chain_hash
        ))
    }
}

// ============================================================
// EcdsaSignature
// ============================================================

type Secp256k1Signature = secp256k1::ecdsa::Signature;

/// A wrapper around secp256k1 ECDSA signature with serde and molecule support.
#[derive(Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct EcdsaSignature(pub Secp256k1Signature);

impl EcdsaSignature {
    pub fn verify(&self, pubkey: &Pubkey, message: &[u8; 32]) -> bool {
        let message = secp256k1::Message::from_digest(*message);
        let pk = secp256k1::PublicKey::from_slice(&pubkey.0)
            .expect("Pubkey should always contain valid serialized public key");
        secp256k1::SECP256K1
            .verify_ecdsa(&message, &self.0, &pk)
            .is_ok()
    }
}

impl From<EcdsaSignature> for Secp256k1Signature {
    fn from(sig: EcdsaSignature) -> Self {
        sig.0
    }
}

impl From<Secp256k1Signature> for EcdsaSignature {
    fn from(sig: Secp256k1Signature) -> Self {
        Self(sig)
    }
}

// EcdsaSignature <-> molecule_fiber::EcdsaSignature

impl From<EcdsaSignature> for molecule_fiber::EcdsaSignature {
    fn from(signature: EcdsaSignature) -> molecule_fiber::EcdsaSignature {
        molecule_fiber::EcdsaSignature::new_builder()
            .set(
                signature
                    .0
                    .serialize_compact()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Signature serialized to correct length"),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::EcdsaSignature> for EcdsaSignature {
    type Error = secp256k1::Error;

    fn try_from(signature: molecule_fiber::EcdsaSignature) -> Result<Self, Self::Error> {
        let signature = signature.raw_data();
        Secp256k1Signature::from_compact(&signature).map(Into::into)
    }
}

// ============================================================
// AnnouncedNodeName
// ============================================================

/// A node's announced name (up to 32 bytes, UTF-8 encoded).
/// If the length is less than 32 bytes, it will be padded with 0.
/// If the length is more than 32 bytes, it should be truncated.
#[derive(Eq, PartialEq, Copy, Clone, Default, Hash)]
pub struct AnnouncedNodeName(pub [u8; 32]);

impl AnnouncedNodeName {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self, String> {
        if slice.len() > 32 {
            return Err("Node Alias can not be longer than 32 bytes".to_string());
        }
        let mut bytes = [0; 32];
        bytes[..slice.len()].copy_from_slice(slice);
        Ok(Self(bytes))
    }

    pub fn from_string(value: &str) -> Result<Self, String> {
        let str_bytes = value.as_bytes();
        Self::from_slice(str_bytes)
    }

    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        if end == 0 {
            return "";
        }
        std::str::from_utf8(&self.0[..end]).expect("valid utf8 string")
    }
}

impl std::fmt::Display for AnnouncedNodeName {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::fmt::Debug for AnnouncedNodeName {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "AnnouncedNodeName({})", self)
    }
}

impl<'s> From<&'s str> for AnnouncedNodeName {
    fn from(value: &'s str) -> Self {
        Self::from_string(value).expect("Valid announced node name")
    }
}

impl Serialize for AnnouncedNodeName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(std::str::from_utf8(&self.0).expect("valid utf8 string"))
    }
}

impl<'de> Deserialize<'de> for AnnouncedNodeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_string(&s).map_err(serde::de::Error::custom)
    }
}

// ============================================================
// FeatureVector
// ============================================================

/// Feature bit type alias.
pub type FeatureBit = u16;

/// Macro for declaring feature bits and generating methods on FeatureVector.
///
/// Each feature is declared with a name and an odd bit number. The macro generates:
/// - `NAME_REQUIRED` (even bit) and `NAME_OPTIONAL` (odd bit) constants
/// - `MAX_FEATURE_BIT` constant
/// - `feature_bit_name()` function
/// - Setter/unsetter/query methods on `FeatureVector`
#[macro_export]
macro_rules! declare_feature_bits_and_methods {
    (
        $( $name:ident, $odd:expr; )*
    ) => {
        paste::paste! {
            $(
                /// Even bit, the bit used to signify that the feature is required.
                pub const [<$name _REQUIRED>]: u16 = $odd - 1;
                /// Odd bit, the bit used to signify that the feature is optional.
                pub const [<$name _OPTIONAL>]: u16 = $odd;
            )*

            pub const MAX_FEATURE_BIT: u16 = {
                let mut max = 0;
                $(
                    if $odd % 2 == 0 || $odd <= max {
                        panic!("feature base bit must be defined as increasing odd numbers");
                    }
                    max = $odd;
                )*
                max
            };

            pub fn feature_bit_name(bit: FeatureBit) -> &'static str {
                match bit {
                    $(
                        [<$name _REQUIRED>] => stringify!([<$name _REQUIRED>]),
                        [<$name _OPTIONAL>] => stringify!([<$name _OPTIONAL>]),
                    )*
                    _ => "Unknown Feature",
                }
            }

            impl FeatureVector {
                $(
                    pub fn [<set_ $name:lower _required>](&mut self) {
                        self.set([<$name _REQUIRED>], true);
                    }
                    pub fn [<set_ $name:lower _optional>](&mut self) {
                        self.set([<$name _OPTIONAL>], true);
                    }
                    pub fn [<unset_ $name:lower _required>](&mut self) {
                        self.set([<$name _REQUIRED>], false);
                    }
                    pub fn [<unset_ $name:lower _optional>](&mut self) {
                        self.set([<$name _OPTIONAL>], false);
                    }
                    pub fn [<requires_ $name:lower>](&self) -> bool {
                        self.requires_feature([<$name _REQUIRED>])
                    }
                    pub fn [<supports_ $name:lower>](&self) -> bool {
                        self.supports_feature([<$name _OPTIONAL>])
                    }
                )*
            }
        }
    };
}

/// Feature bits and methods for the Fiber protocol.
/// Pair bits: a feature can be introduced as optional (odd bits)
/// and later upgraded to be compulsory (even bits).
///   - Even bits are used to signify that the feature is required.
///   - Odd bits are used to signify that the feature is optional.
pub mod feature_bits {
    use super::*;
    declare_feature_bits_and_methods! {
        GOSSIP_QUERIES, 1;
        BASIC_MPP, 3;
        TRAMPOLINE_ROUTING, 5;
        // more features, please note that base bit must be defined as increasing odd numbers
    }
}

/// A compact bit-vector representing protocol feature flags.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FeatureVector {
    inner: Vec<u8>,
}

impl Default for FeatureVector {
    fn default() -> Self {
        let mut feature = Self::new();
        feature.set_gossip_queries_required();
        feature.set_basic_mpp_required();
        feature.set_trampoline_routing_required();

        // set other default features here
        // ...

        feature
    }
}

impl FeatureVector {
    pub fn new() -> Self {
        let len = (feature_bits::MAX_FEATURE_BIT / 8) as usize + 1;
        Self {
            inner: vec![0; len],
        }
    }

    pub fn from(bytes: Vec<u8>) -> Self {
        Self { inner: bytes }
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.inner.clone()
    }

    fn is_set(&self, bit: FeatureBit) -> bool {
        let idx = (bit / 8) as usize;
        if idx >= self.inner.len() {
            return false;
        }
        self.inner
            .get(idx)
            .map(|&byte| (byte >> (bit % 8)) & 1 == 1)
            .unwrap_or(false)
    }

    fn set(&mut self, bit: FeatureBit, set: bool) {
        let idx = (bit / 8) as usize;
        if self.inner.len() <= idx {
            self.inner.resize(idx + 1, 0);
        }
        let mask = 1 << (bit % 8);
        if set {
            self.inner[idx] |= mask;
        } else {
            self.inner[idx] &= !mask;
        }
    }

    pub fn enabled_features(&self) -> Vec<FeatureBit> {
        (0..(self.inner.len() * 8) as FeatureBit)
            .filter(|&bit| self.is_set(bit))
            .collect()
    }

    pub fn enabled_features_names(&self) -> Vec<String> {
        self.enabled_features()
            .into_iter()
            .map(feature_bits::feature_bit_name)
            .map(|name| name.to_string())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.iter().all(|&b| b == 0)
    }

    pub fn set_feature(&mut self, bit: FeatureBit) {
        self.set(bit, true);
    }

    pub fn unset_feature(&mut self, bit: FeatureBit) {
        self.set(bit, false);
    }

    pub fn requires_feature(&self, bit: FeatureBit) -> bool {
        self.is_set(bit) && bit.is_multiple_of(2)
    }

    pub fn supports_feature(&self, bit: FeatureBit) -> bool {
        self.is_set(bit) || self.is_set(bit ^ 1)
    }

    pub fn compatible_with(&self, other: &Self) -> bool {
        if self
            .enabled_features()
            .iter()
            .any(|&bit| self.requires_feature(bit) && !other.supports_feature(bit))
        {
            return false;
        }
        true
    }
}

impl std::fmt::Debug for FeatureVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureVector")
            .field("features", &self.enabled_features_names())
            .finish()
    }
}

// ============================================================
// SchnorrSignature
// ============================================================

/// A wrapper around secp256k1 Schnorr signature with serde and molecule support.
#[derive(Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct SchnorrSignature(pub secp256k1::schnorr::Signature);

impl SchnorrSignature {
    /// Returns a reference to the inner secp256k1 Schnorr signature.
    pub fn inner(&self) -> &secp256k1::schnorr::Signature {
        &self.0
    }

    /// Serializes the signature to a 64-byte array.
    pub fn to_byte_array(&self) -> [u8; 64] {
        self.0.to_byte_array()
    }

    /// Creates a SchnorrSignature from a byte slice.
    pub fn from_slice(data: &[u8]) -> Result<Self, secp256k1::Error> {
        secp256k1::schnorr::Signature::from_slice(data).map(SchnorrSignature)
    }
}

impl std::ops::Deref for SchnorrSignature {
    type Target = secp256k1::schnorr::Signature;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<SchnorrSignature> for secp256k1::schnorr::Signature {
    fn from(sig: SchnorrSignature) -> Self {
        sig.0
    }
}

impl From<secp256k1::schnorr::Signature> for SchnorrSignature {
    fn from(sig: secp256k1::schnorr::Signature) -> Self {
        Self(sig)
    }
}

impl From<LiftedSignature> for SchnorrSignature {
    fn from(sig: LiftedSignature) -> Self {
        Self(secp256k1::schnorr::Signature::from(sig))
    }
}

// SchnorrSignature <-> molecule_gossip::SchnorrSignature

impl From<SchnorrSignature> for molecule_gossip::SchnorrSignature {
    fn from(signature: SchnorrSignature) -> molecule_gossip::SchnorrSignature {
        molecule_gossip::SchnorrSignature::new_builder()
            .set(
                signature
                    .0
                    .to_byte_array()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Signature serialized to correct length"),
            )
            .build()
    }
}

impl TryFrom<molecule_gossip::SchnorrSignature> for SchnorrSignature {
    type Error = secp256k1::Error;

    fn try_from(signature: molecule_gossip::SchnorrSignature) -> Result<Self, Self::Error> {
        secp256k1::schnorr::Signature::from_slice(signature.as_slice()).map(Into::into)
    }
}

// ============================================================
// ChannelAnnouncement
// ============================================================

/// Announcement of a new channel in the network.
///
/// This message is broadcast to all nodes to inform them about a new channel.
/// It contains the channel's capacity, the nodes involved, and signatures.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct ChannelAnnouncement {
    /// Signature from node 1
    pub node1_signature: Option<EcdsaSignature>,
    /// Signature from node 2
    pub node2_signature: Option<EcdsaSignature>,
    /// Signature signed by the funding transaction output public key
    pub ckb_signature: Option<SchnorrSignature>,
    /// Feature flags for the channel
    pub features: u64,
    /// The chain hash this channel belongs to
    pub chain_hash: Hash256,
    /// The outpoint of the funding transaction
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// Public key of node 1
    pub node1_id: Pubkey,
    /// Public key of node 2
    pub node2_id: Pubkey,
    /// The aggregated public key of the funding transaction output
    pub ckb_key: secp256k1::XOnlyPublicKey,
    /// The total capacity of the channel
    pub capacity: u128,
    /// UDT type script if this is a UDT channel
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
}

impl ChannelAnnouncement {
    /// Check if the announcement is fully signed.
    pub fn is_signed(&self) -> bool {
        self.node1_signature.is_some()
            && self.node2_signature.is_some()
            && self.ckb_signature.is_some()
    }

    /// Get the channel outpoint.
    pub fn out_point(&self) -> &OutPoint {
        &self.channel_outpoint
    }

    /// Create an unsigned channel announcement with the given parameters.
    pub fn new_unsigned(
        node1_pubkey: &Pubkey,
        node2_pubkey: &Pubkey,
        channel_outpoint: OutPoint,
        ckb_pubkey: &secp256k1::XOnlyPublicKey,
        capacity: u128,
        udt_type_script: Option<Script>,
    ) -> Self {
        Self {
            node1_signature: None,
            node2_signature: None,
            ckb_signature: None,
            features: Default::default(),
            chain_hash: get_chain_hash(),
            channel_outpoint,
            node1_id: *node1_pubkey,
            node2_id: *node2_pubkey,
            ckb_key: *ckb_pubkey,
            capacity,
            udt_type_script,
        }
    }
}

// ============================================================
// ChannelUpdate
// ============================================================

/// Update to an existing channel's routing parameters.
///
/// This message is broadcast to update routing information for a channel,
/// such as fees, timelock requirements, or disabled status.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct ChannelUpdate {
    /// Signature of the node that wants to update the channel information
    pub signature: Option<EcdsaSignature>,
    /// The chain hash this channel belongs to
    pub chain_hash: Hash256,
    /// The outpoint of the funding transaction
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// Timestamp for this update
    pub timestamp: u64,
    /// Message flags (indicates which node this update is from)
    pub message_flags: ChannelUpdateMessageFlags,
    /// Channel flags (indicates if channel is disabled)
    pub channel_flags: ChannelUpdateChannelFlags,
    /// TLC expiry delta in blocks
    pub tlc_expiry_delta: u64,
    /// Minimum TLC value
    pub tlc_minimum_value: u128,
    /// Fee proportional millionths
    pub tlc_fee_proportional_millionths: u128,
}

impl ChannelUpdate {
    /// Create an unsigned channel update with the given parameters.
    ///
    /// To avoid having the same timestamp for both channel updates, an even
    /// timestamp is used for node1 and an odd timestamp for node2.
    pub fn new_unsigned(
        channel_outpoint: OutPoint,
        timestamp: u64,
        message_flags: ChannelUpdateMessageFlags,
        channel_flags: ChannelUpdateChannelFlags,
        tlc_expiry_delta: u64,
        tlc_minimum_value: u128,
        tlc_fee_proportional_millionths: u128,
    ) -> Self {
        let timestamp = if message_flags.contains(ChannelUpdateMessageFlags::UPDATE_OF_NODE2) {
            timestamp | 1u64
        } else {
            timestamp & !1u64
        };
        Self {
            signature: None,
            chain_hash: get_chain_hash(),
            channel_outpoint,
            timestamp,
            message_flags,
            channel_flags,
            tlc_expiry_delta,
            tlc_minimum_value,
            tlc_fee_proportional_millionths,
        }
    }

    /// Check if this update is from node 1.
    pub fn is_update_of_node_1(&self) -> bool {
        !self.is_update_of_node_2()
    }

    /// Check if this update is from node 2.
    pub fn is_update_of_node_2(&self) -> bool {
        self.message_flags
            .contains(ChannelUpdateMessageFlags::UPDATE_OF_NODE2)
    }

    /// Check if the channel is disabled.
    pub fn is_disabled(&self) -> bool {
        self.channel_flags
            .contains(ChannelUpdateChannelFlags::DISABLED)
    }

    /// Get the message bytes to sign (hash of unsigned molecule serialization).
    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_update = ChannelUpdate {
            signature: None,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            timestamp: self.timestamp,
            message_flags: self.message_flags,
            channel_flags: self.channel_flags,
            tlc_expiry_delta: self.tlc_expiry_delta,
            tlc_minimum_value: self.tlc_minimum_value,
            tlc_fee_proportional_millionths: self.tlc_fee_proportional_millionths,
        };
        deterministically_hash(&molecule_fiber::ChannelUpdate::from(unsigned_update))
    }

    /// Get the cursor for this channel update.
    pub fn cursor(&self) -> Cursor {
        Cursor::new(
            self.timestamp,
            BroadcastMessageID::ChannelUpdate(self.channel_outpoint.clone()),
        )
    }
}

impl ChannelAnnouncement {
    /// Get the message bytes to sign (hash of unsigned molecule serialization).
    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = Self {
            node1_signature: None,
            node2_signature: None,
            ckb_signature: None,
            features: self.features,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            node1_id: self.node1_id,
            node2_id: self.node2_id,
            ckb_key: self.ckb_key,
            capacity: self.capacity,
            udt_type_script: self.udt_type_script.clone(),
        };
        deterministically_hash(&molecule_gossip::ChannelAnnouncement::from(
            unsigned_announcement,
        ))
    }
}

impl NodeAnnouncement {
    /// Create an unsigned node announcement with the given parameters.
    ///
    /// The `udt_cfg_infos` and `version` are passed as parameters to avoid
    /// depending on runtime/global state from the caller's crate.
    #[allow(clippy::too_many_arguments)]
    pub fn new_unsigned(
        node_name: AnnouncedNodeName,
        features: FeatureVector,
        addresses: Vec<tentacle_multiaddr::Multiaddr>,
        node_id: Pubkey,
        timestamp: u64,
        auto_accept_min_ckb_funding_amount: u64,
        udt_cfg_infos: UdtCfgInfos,
        version: String,
    ) -> Self {
        Self {
            signature: None,
            features,
            timestamp,
            node_id,
            version,
            node_name,
            chain_hash: get_chain_hash(),
            addresses,
            auto_accept_min_ckb_funding_amount,
            udt_cfg_infos,
        }
    }

    /// Create a signed node announcement.
    ///
    /// Builds an unsigned announcement, signs it with the given private key,
    /// and returns the signed announcement.
    #[allow(clippy::too_many_arguments)]
    pub fn new_signed(
        node_name: AnnouncedNodeName,
        features: FeatureVector,
        addresses: Vec<tentacle_multiaddr::Multiaddr>,
        private_key: &Privkey,
        timestamp: u64,
        auto_accept_min_ckb_funding_amount: u64,
        udt_cfg_infos: UdtCfgInfos,
        version: String,
    ) -> Self {
        let mut unsigned = Self::new_unsigned(
            node_name,
            features,
            addresses,
            private_key.pubkey(),
            timestamp,
            auto_accept_min_ckb_funding_amount,
            udt_cfg_infos,
            version,
        );
        unsigned.signature = Some(private_key.sign(unsigned.message_to_sign()));
        unsigned
    }

    /// Get the message bytes to sign (hash of unsigned molecule serialization).
    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = NodeAnnouncement {
            signature: None,
            features: self.features.clone(),
            timestamp: self.timestamp,
            node_id: self.node_id,
            version: self.version.clone(),
            node_name: self.node_name,
            chain_hash: self.chain_hash,
            addresses: self.addresses.clone(),
            auto_accept_min_ckb_funding_amount: self.auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: self.udt_cfg_infos.clone(),
        };
        deterministically_hash(&molecule_gossip::NodeAnnouncement::from(
            unsigned_announcement,
        ))
    }

    /// Get the peer ID for this node.
    pub fn peer_id(&self) -> tentacle_secio::PeerId {
        tentacle_secio::PeerId::from_public_key(&self.node_id.into())
    }

    /// Get the cursor for this node announcement.
    pub fn cursor(&self) -> Cursor {
        Cursor::new(
            self.timestamp,
            BroadcastMessageID::NodeAnnouncement(self.node_id),
        )
    }

    /// Verify the signature on this announcement.
    pub fn verify(&self) -> bool {
        let message = self.message_to_sign();
        match self.signature {
            Some(ref signature) => signature.verify(&self.node_id, &message),
            _ => false,
        }
    }
}

/// Hash a molecule entity deterministically using blake2b-256.
pub(crate) fn deterministically_hash<T: Entity>(v: &T) -> [u8; 32] {
    ckb_hash::blake2b_256(v.as_slice())
}

// ============================================================
// BroadcastMessage
// ============================================================

/// A broadcast message in the gossip protocol.
///
/// This enum represents the different types of messages that can be broadcast
/// to all nodes in the network.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub enum BroadcastMessage {
    /// Node announcement message
    NodeAnnouncement(NodeAnnouncement),
    /// Channel announcement message
    ChannelAnnouncement(ChannelAnnouncement),
    /// Channel update message
    ChannelUpdate(ChannelUpdate),
}

impl BroadcastMessage {
    /// Returns the cursor for this broadcast message, if applicable.
    pub fn cursor(&self) -> Option<Cursor> {
        match self {
            BroadcastMessage::ChannelAnnouncement(_) => None,
            BroadcastMessage::ChannelUpdate(channel_update) => Some(channel_update.cursor()),
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                Some(node_announcement.cursor())
            }
        }
    }

    /// Returns the message ID for this broadcast message.
    pub fn message_id(&self) -> BroadcastMessageID {
        match self {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                BroadcastMessageID::NodeAnnouncement(node_announcement.node_id)
            }
            BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                BroadcastMessageID::ChannelAnnouncement(
                    channel_announcement.channel_outpoint.clone(),
                )
            }
            BroadcastMessage::ChannelUpdate(channel_update) => {
                BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone())
            }
        }
    }

    /// Returns the timestamp for this broadcast message, if applicable.
    pub fn timestamp(&self) -> Option<u64> {
        match self {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                Some(node_announcement.timestamp)
            }
            BroadcastMessage::ChannelAnnouncement(_) => None,
            BroadcastMessage::ChannelUpdate(channel_update) => Some(channel_update.timestamp),
        }
    }
}

// ============================================================
// NodeAnnouncement
// ============================================================

/// Announcement of a node's presence and capabilities.
///
/// This message is broadcast to inform other nodes about this node's
/// addresses, features, and other metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct NodeAnnouncement {
    /// Signature of this message
    pub signature: Option<EcdsaSignature>,
    /// Features supported by this node
    pub features: FeatureVector,
    /// Timestamp for this announcement
    pub timestamp: u64,
    /// The public key of the node
    pub node_id: Pubkey,
    /// Software version string
    pub version: String,
    /// Human-readable node name (up to 32 bytes)
    pub node_name: AnnouncedNodeName,
    /// Reachable addresses for this node
    pub addresses: Vec<tentacle_multiaddr::Multiaddr>,
    /// The chain hash this node is on
    pub chain_hash: Hash256,
    /// Minimum CKB funding amount for auto-accept
    pub auto_accept_min_ckb_funding_amount: u64,
    /// UDT configuration info
    pub udt_cfg_infos: UdtCfgInfos,
}

// ============================================================
// BroadcastMessageID
// ============================================================

/// The ID of a broadcast message.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum BroadcastMessageID {
    ChannelAnnouncement(OutPoint),
    ChannelUpdate(OutPoint),
    NodeAnnouncement(Pubkey),
}

impl Default for BroadcastMessageID {
    fn default() -> Self {
        BroadcastMessageID::ChannelAnnouncement(OutPoint::default())
    }
}

// We need to implement Ord for BroadcastMessageID to make sure that a ChannelUpdate message is always ordered after ChannelAnnouncement,
// so that we can use it as the sorting key in fn prune_messages_to_be_saved to simplify the logic.
// We need to implement Ord for BroadcastMessageID to make sure that a ChannelUpdate message is always ordered after ChannelAnnouncement,
// so that we can use it as the sorting key in fn prune_messages_to_be_saved to simplify the logic.
// Ordering: NodeAnnouncement < ChannelAnnouncement < ChannelUpdate
impl Ord for BroadcastMessageID {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (
                BroadcastMessageID::ChannelAnnouncement(outpoint1),
                BroadcastMessageID::ChannelAnnouncement(outpoint2),
            ) => outpoint1.cmp(outpoint2),
            (
                BroadcastMessageID::ChannelUpdate(outpoint1),
                BroadcastMessageID::ChannelUpdate(outpoint2),
            ) => outpoint1.cmp(outpoint2),
            (
                BroadcastMessageID::NodeAnnouncement(node1),
                BroadcastMessageID::NodeAnnouncement(node2),
            ) => node1.cmp(node2),
            (BroadcastMessageID::NodeAnnouncement(_), _) => Ordering::Less,
            (BroadcastMessageID::ChannelUpdate(_), _) => Ordering::Greater,
            (
                BroadcastMessageID::ChannelAnnouncement(_),
                BroadcastMessageID::NodeAnnouncement(_),
            ) => Ordering::Greater,
            (BroadcastMessageID::ChannelAnnouncement(_), BroadcastMessageID::ChannelUpdate(_)) => {
                Ordering::Less
            }
        }
    }
}

impl PartialOrd for BroadcastMessageID {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// 1 byte for message type, 36 bytes for message id
const MESSAGE_ID_SIZE: usize = 1 + 36;

impl BroadcastMessageID {
    pub fn to_bytes(&self) -> [u8; MESSAGE_ID_SIZE] {
        let mut result = [0u8; MESSAGE_ID_SIZE];
        match self {
            BroadcastMessageID::ChannelAnnouncement(channel_outpoint) => {
                result[0] = 0;
                result[1..].copy_from_slice(&channel_outpoint.as_bytes());
            }
            BroadcastMessageID::ChannelUpdate(channel_outpoint) => {
                result[0] = 1;
                result[1..].copy_from_slice(&channel_outpoint.as_bytes());
            }
            BroadcastMessageID::NodeAnnouncement(node_id) => {
                result[0] = 2;
                let node_id = node_id.serialize();
                result[1..1 + node_id.len()].copy_from_slice(&node_id);
            }
        };
        result
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        use molecule::prelude::Entity;
        if bytes.len() != MESSAGE_ID_SIZE {
            anyhow::bail!("Invalid message id size: {}", bytes.len());
        }
        match bytes[0] {
            0 => Ok(BroadcastMessageID::ChannelAnnouncement(
                OutPoint::from_slice(&bytes[1..])?,
            )),
            1 => Ok(BroadcastMessageID::ChannelUpdate(OutPoint::from_slice(
                &bytes[1..],
            )?)),
            2 => Ok(BroadcastMessageID::NodeAnnouncement(Pubkey::from_slice(
                &bytes[1..1 + Pubkey::serialization_len()],
            )?)),
            _ => anyhow::bail!("Invalid message id type: {}", bytes[0]),
        }
    }
}

// ============================================================
// Cursor
// ============================================================

use std::time::Duration;

/// The size of a serialized cursor in bytes.
pub const CURSOR_SIZE: usize = 45;

/// A cursor for paginating broadcast messages.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Default)]
pub struct Cursor {
    pub timestamp: u64,
    pub message_id: BroadcastMessageID,
}

impl Cursor {
    pub fn new(timestamp: u64, message_id: BroadcastMessageID) -> Self {
        Self {
            timestamp,
            message_id,
        }
    }

    /// Create a new cursor which is the same as the current cursor but with a smaller timestamp.
    /// This is useful when we want to query messages back from this cursor for a certain duration.
    pub fn go_back_for_some_time(&self, duration: Duration) -> Self {
        let current_timestamp = self.timestamp;
        let duration_millis = duration.as_millis() as u64;
        if current_timestamp > duration_millis {
            Self {
                timestamp: current_timestamp - duration_millis,
                message_id: self.message_id.clone(),
            }
        } else {
            Default::default()
        }
    }

    pub fn to_bytes(&self) -> [u8; 45] {
        self.timestamp
            .to_be_bytes()
            .into_iter()
            .chain(self.message_id.to_bytes())
            .collect::<Vec<_>>()
            .try_into()
            .expect("Must serialize cursor to 45 bytes")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        if bytes.len() != CURSOR_SIZE {
            anyhow::bail!("Invalid cursor size: {}, want {}", bytes.len(), CURSOR_SIZE);
        }
        let timestamp = u64::from_be_bytes(bytes[..8].try_into().expect("Cursor timestamp to u64"));
        let message_id = BroadcastMessageID::from_bytes(&bytes[8..])?;
        Ok(Cursor {
            timestamp,
            message_id,
        })
    }

    /// A dummy cursor with the maximum timestamp and a dummy message id.
    /// This is useful when we want to create a cursor after which none of the messages should be included.
    pub fn max() -> Self {
        Self {
            timestamp: u64::MAX,
            message_id: BroadcastMessageID::ChannelAnnouncement(OutPoint::default()),
        }
    }

    pub fn is_max(&self) -> bool {
        self.timestamp == u64::MAX
    }
}

// ============================================================
// Molecule Conversions
// ============================================================

// NodeAnnouncement molecule conversions

impl From<NodeAnnouncement> for molecule_gossip::NodeAnnouncement {
    fn from(node_announcement: NodeAnnouncement) -> Self {
        let builder = molecule_gossip::NodeAnnouncement::new_builder()
            .features(node_announcement.features.bytes().pack())
            .timestamp(node_announcement.timestamp.pack())
            .node_id(node_announcement.node_id.into())
            .version(node_announcement.version.pack())
            .node_name(u8_32_as_byte_32(&node_announcement.node_name.0))
            .chain_hash(node_announcement.chain_hash.into())
            .auto_accept_min_ckb_funding_amount(
                node_announcement.auto_accept_min_ckb_funding_amount.pack(),
            )
            .udt_cfg_infos(node_announcement.udt_cfg_infos.into())
            .address(
                BytesVec::new_builder()
                    .set(
                        node_announcement
                            .addresses
                            .into_iter()
                            .map(|address| address.to_vec().pack())
                            .collect(),
                    )
                    .build(),
            );

        let builder = if let Some(signature) = node_announcement.signature {
            builder.signature(signature.into())
        } else {
            builder
        };

        builder.build()
    }
}

impl TryFrom<molecule_gossip::NodeAnnouncement> for NodeAnnouncement {
    type Error = anyhow::Error;

    fn try_from(node_announcement: molecule_gossip::NodeAnnouncement) -> Result<Self, Self::Error> {
        use ckb_types::prelude::Unpack;
        Ok(NodeAnnouncement {
            signature: Some(
                node_announcement
                    .signature()
                    .try_into()
                    .map_err(|e: secp256k1::Error| anyhow::anyhow!(e))?,
            ),
            features: FeatureVector::from(node_announcement.features().unpack()),
            timestamp: node_announcement.timestamp().unpack(),
            node_id: node_announcement
                .node_id()
                .try_into()
                .map_err(|e: secp256k1::Error| anyhow::anyhow!(e))?,
            version: String::from_utf8(node_announcement.version().unpack()).unwrap_or_default(),
            chain_hash: node_announcement.chain_hash().into(),
            auto_accept_min_ckb_funding_amount: node_announcement
                .auto_accept_min_ckb_funding_amount()
                .unpack(),
            node_name: AnnouncedNodeName::from_slice(node_announcement.node_name().as_slice())
                .map_err(|e| anyhow::anyhow!("Invalid node_name: {}", e))?,
            udt_cfg_infos: node_announcement.udt_cfg_infos().try_into()?,
            addresses: node_announcement
                .address()
                .into_iter()
                .map(|bytes| {
                    tentacle_multiaddr::Multiaddr::try_from(bytes.raw_data().to_vec())
                        .map_err(Into::into)
                })
                .collect::<Result<Vec<_>, anyhow::Error>>()?,
        })
    }
}

// ============================================================
// Molecule Conversions for ChannelAnnouncement
// ============================================================

impl From<ChannelAnnouncement> for molecule_gossip::ChannelAnnouncement {
    fn from(channel_announcement: ChannelAnnouncement) -> Self {
        let builder = molecule_gossip::ChannelAnnouncement::new_builder()
            .features(channel_announcement.features.pack())
            .chain_hash(channel_announcement.chain_hash.into())
            .channel_outpoint(channel_announcement.channel_outpoint)
            .node1_id(channel_announcement.node1_id.into())
            .node2_id(channel_announcement.node2_id.into())
            .capacity(channel_announcement.capacity.pack())
            .udt_type_script(channel_announcement.udt_type_script.pack())
            .ckb_key(channel_announcement.ckb_key.into());

        let builder = if let Some(signature) = channel_announcement.node1_signature {
            builder.node1_signature(signature.into())
        } else {
            builder
        };

        let builder = if let Some(signature) = channel_announcement.node2_signature {
            builder.node2_signature(signature.into())
        } else {
            builder
        };

        let builder = if let Some(signature) = channel_announcement.ckb_signature {
            builder.ckb_signature(signature.into())
        } else {
            builder
        };

        builder.build()
    }
}

impl TryFrom<molecule_gossip::ChannelAnnouncement> for ChannelAnnouncement {
    type Error = anyhow::Error;

    fn try_from(
        channel_announcement: molecule_gossip::ChannelAnnouncement,
    ) -> Result<Self, Self::Error> {
        use ckb_types::prelude::Unpack;
        Ok(ChannelAnnouncement {
            node1_signature: Some(channel_announcement.node1_signature().try_into()?),
            node2_signature: Some(channel_announcement.node2_signature().try_into()?),
            ckb_signature: Some(channel_announcement.ckb_signature().try_into()?),
            features: channel_announcement.features().unpack(),
            capacity: channel_announcement.capacity().unpack(),
            chain_hash: channel_announcement.chain_hash().into(),
            channel_outpoint: channel_announcement.channel_outpoint(),
            udt_type_script: channel_announcement.udt_type_script().to_opt(),
            node1_id: channel_announcement.node1_id().try_into()?,
            node2_id: channel_announcement.node2_id().try_into()?,
            ckb_key: channel_announcement.ckb_key().try_into()?,
        })
    }
}

// ============================================================
// Molecule Conversions for ChannelUpdate
// ============================================================

impl From<ChannelUpdate> for molecule_fiber::ChannelUpdate {
    fn from(channel_update: ChannelUpdate) -> Self {
        let builder = molecule_fiber::ChannelUpdate::new_builder()
            .chain_hash(channel_update.chain_hash.into())
            .channel_outpoint(channel_update.channel_outpoint)
            .timestamp(channel_update.timestamp.pack())
            .message_flags(channel_update.message_flags.bits().pack())
            .channel_flags(channel_update.channel_flags.bits().pack())
            .tlc_expiry_delta(channel_update.tlc_expiry_delta.pack())
            .tlc_minimum_value(channel_update.tlc_minimum_value.pack())
            .tlc_fee_proportional_millionths(channel_update.tlc_fee_proportional_millionths.pack());

        let builder = if let Some(signature) = channel_update.signature {
            builder.signature(signature.into())
        } else {
            builder
        };

        builder.build()
    }
}

impl TryFrom<molecule_fiber::ChannelUpdate> for ChannelUpdate {
    type Error = anyhow::Error;

    fn try_from(channel_update: molecule_fiber::ChannelUpdate) -> Result<Self, Self::Error> {
        use ckb_types::prelude::Unpack;
        Ok(ChannelUpdate {
            signature: Some(channel_update.signature().try_into()?),
            chain_hash: channel_update.chain_hash().into(),
            channel_outpoint: channel_update.channel_outpoint(),
            timestamp: channel_update.timestamp().unpack(),
            message_flags: ChannelUpdateMessageFlags::from_bits_truncate(
                channel_update.message_flags().unpack(),
            ),
            channel_flags: ChannelUpdateChannelFlags::from_bits_truncate(
                channel_update.channel_flags().unpack(),
            ),
            tlc_expiry_delta: channel_update.tlc_expiry_delta().unpack(),
            tlc_minimum_value: channel_update.tlc_minimum_value().unpack(),
            tlc_fee_proportional_millionths: channel_update
                .tlc_fee_proportional_millionths()
                .unpack(),
        })
    }
}

// ============================================================
// BroadcastMessage Ord/PartialOrd and Molecule Conversions
// ============================================================

impl Ord for BroadcastMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        self.message_id()
            .cmp(&other.message_id())
            .then(self.timestamp().cmp(&other.timestamp()))
    }
}

impl PartialOrd for BroadcastMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<BroadcastMessage> for molecule_gossip::BroadcastMessageUnion {
    fn from(fiber_broadcast_message: BroadcastMessage) -> Self {
        match fiber_broadcast_message {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                molecule_gossip::BroadcastMessageUnion::NodeAnnouncement(node_announcement.into())
            }
            BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                molecule_gossip::BroadcastMessageUnion::ChannelAnnouncement(
                    channel_announcement.into(),
                )
            }
            BroadcastMessage::ChannelUpdate(channel_update) => {
                molecule_gossip::BroadcastMessageUnion::ChannelUpdate(channel_update.into())
            }
        }
    }
}

impl TryFrom<molecule_gossip::BroadcastMessageUnion> for BroadcastMessage {
    type Error = anyhow::Error;

    fn try_from(
        fiber_broadcast_message: molecule_gossip::BroadcastMessageUnion,
    ) -> Result<Self, Self::Error> {
        match fiber_broadcast_message {
            molecule_gossip::BroadcastMessageUnion::NodeAnnouncement(node_announcement) => Ok(
                BroadcastMessage::NodeAnnouncement(node_announcement.try_into()?),
            ),
            molecule_gossip::BroadcastMessageUnion::ChannelAnnouncement(channel_announcement) => {
                Ok(BroadcastMessage::ChannelAnnouncement(
                    channel_announcement.try_into()?,
                ))
            }
            molecule_gossip::BroadcastMessageUnion::ChannelUpdate(channel_update) => {
                Ok(BroadcastMessage::ChannelUpdate(channel_update.try_into()?))
            }
        }
    }
}

impl From<BroadcastMessage> for molecule_gossip::BroadcastMessage {
    fn from(fiber_broadcast_message: BroadcastMessage) -> Self {
        molecule_gossip::BroadcastMessage::new_builder()
            .set(fiber_broadcast_message)
            .build()
    }
}

impl TryFrom<molecule_gossip::BroadcastMessage> for BroadcastMessage {
    type Error = anyhow::Error;

    fn try_from(
        fiber_broadcast_message: molecule_gossip::BroadcastMessage,
    ) -> Result<Self, Self::Error> {
        fiber_broadcast_message.to_enum().try_into()
    }
}

// ============================================================
// Cursor Ord/PartialOrd and Molecule Conversions
// ============================================================

impl Ord for Cursor {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_bytes().cmp(&other.to_bytes())
    }
}

impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<Cursor> for molecule_gossip::Cursor {
    fn from(cursor: Cursor) -> Self {
        use molecule::prelude::{Builder, Byte};
        let serialized = cursor
            .timestamp
            .to_be_bytes()
            .into_iter()
            .chain(cursor.message_id.to_bytes())
            .map(Byte::new)
            .collect::<Vec<_>>()
            .try_into()
            .expect("Must serialize cursor to 45 bytes");

        molecule_gossip::Cursor::new_builder()
            .set(serialized)
            .build()
    }
}

impl TryFrom<molecule_gossip::Cursor> for Cursor {
    type Error = anyhow::Error;

    fn try_from(cursor: molecule_gossip::Cursor) -> Result<Self, Self::Error> {
        use molecule::prelude::Entity;
        let slice = cursor.as_slice();
        if slice.len() != CURSOR_SIZE {
            anyhow::bail!("Invalid cursor size: {}, want {}", slice.len(), CURSOR_SIZE);
        }
        let timestamp = u64::from_be_bytes(slice[..8].try_into().expect("Cursor timestamp to u64"));
        let message_id = BroadcastMessageID::from_bytes(&slice[8..])?;
        Ok(Cursor {
            timestamp,
            message_id,
        })
    }
}
