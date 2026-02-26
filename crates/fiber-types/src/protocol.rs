//! Layer 2: Protocol message types and supporting types.
//!
//! Contains gossip protocol types (ChannelAnnouncement, ChannelUpdate, NodeAnnouncement,
//! BroadcastMessage) and their supporting types (EcdsaSignature, AnnouncedNodeName,
//! FeatureVector, SchnorrSignature).

use crate::channel::{ChannelUpdateChannelFlags, ChannelUpdateMessageFlags};
use crate::gen::fiber as molecule_fiber;
use crate::gen::gossip as molecule_gossip;
use crate::serde_utils::EntityHex;
use crate::{Hash256, Pubkey};
use ckb_types::packed::{OutPoint, Script};
use molecule::prelude::{Builder, Byte, Entity};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::serde_as;

pub use feature_bits::*;

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
// UDT Config Types (for NodeAnnouncement)
// ============================================================

// Serde converters for CKB types
serde_with::serde_conv!(
    ScriptHashTypeWrapper,
    ckb_types::core::ScriptHashType,
    |s: &ckb_types::core::ScriptHashType| -> String {
        use ckb_types::core::ScriptHashType;
        let v = match s {
            ScriptHashType::Type => "type",
            ScriptHashType::Data => "data",
            ScriptHashType::Data1 => "data1",
            ScriptHashType::Data2 => "data2",
        };
        v.to_string()
    },
    |s: String| {
        use ckb_types::core::ScriptHashType;
        let v = match s.to_lowercase().as_str() {
            "type" => ScriptHashType::Type,
            "data" => ScriptHashType::Data,
            "data1" => ScriptHashType::Data1,
            "data2" => ScriptHashType::Data2,
            _ => return Err("invalid hash type"),
        };
        Ok(v)
    }
);

serde_with::serde_conv!(
    DepTypeWrapper,
    ckb_types::core::DepType,
    |s: &ckb_types::core::DepType| -> String {
        use ckb_types::core::DepType;
        let v = match s {
            DepType::Code => "code",
            DepType::DepGroup => "dep_group",
        };
        v.to_string()
    },
    |s: String| {
        let v = match s.to_lowercase().as_str() {
            "code" => ckb_types::core::DepType::Code,
            "dep_group" => ckb_types::core::DepType::DepGroup,
            _ => return Err("invalid dep type"),
        };
        Ok(v)
    }
);

/// UDT argument information.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtArgInfo {
    /// Name of the UDT
    pub name: String,
    /// UDT script configuration
    pub script: UdtScript,
    /// Auto-accept amount for this UDT
    pub auto_accept_amount: Option<u128>,
    /// Cell dependencies for this UDT
    pub cell_deps: Vec<UdtDep>,
}

/// UDT script configuration.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtScript {
    /// Code hash of the UDT script
    pub code_hash: ckb_types::H256,
    /// Hash type of the UDT script
    #[serde_as(as = "ScriptHashTypeWrapper")]
    pub hash_type: ckb_types::core::ScriptHashType,
    /// Arguments for the UDT script
    pub args: String,
}

/// UDT cell dependency configuration.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtCellDep {
    /// The outpoint of the cell dependency
    pub out_point: ckb_jsonrpc_types::OutPoint,
    /// The dependency type
    #[serde_as(as = "DepTypeWrapper")]
    pub dep_type: ckb_types::core::DepType,
}

/// UDT dependency (either cell dep or type ID).
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtDep {
    /// Cell dependency (if using direct reference)
    #[serde(default)]
    pub cell_dep: Option<UdtCellDep>,
    /// Type ID script (if using type ID reference)
    #[serde(default)]
    pub type_id: Option<ckb_jsonrpc_types::Script>,
}

/// Collection of UDT configuration information.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Default)]
pub struct UdtCfgInfos(pub Vec<UdtArgInfo>);
