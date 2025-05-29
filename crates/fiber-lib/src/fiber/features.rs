use paste::paste;
use serde::{Deserialize, Serialize};
use std::fmt;

pub type FeatureBit = u16;

#[macro_export]
macro_rules! declare_feature_bits_and_methods {
    (
        $( $name:ident, $odd:expr; )*
    ) => {
        paste! {
            $(
                /// Even bit, the bit used to signify that the feature is required.
                pub const [<$name _REQUIRED>]: u16 = $odd - 1;
                /// Odd bit, the bit used to signify that the feature is optional.
                pub const [<$name _OPTIONAL>]: u16 = $odd;
            )*

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
                        self.set_feature([<$name _REQUIRED>]);
                    }
                    pub fn [<set_ $name:lower _optional>](&mut self) {
                        self.set_feature([<$name _OPTIONAL>]);
                    }
                    pub fn [<unset_ $name:lower _required>](&mut self) {
                        self.unset_feature([<$name _REQUIRED>]);
                    }
                    pub fn [<unset_ $name:lower _optional>](&mut self) {
                        self.unset_feature([<$name _OPTIONAL>]);
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

/// Feature bits and methods for the Fiber protocol
/// Pair bits:
///   - Each pair consists of a required and an optional bit.
///   - Even bits are used to signify that the feature is required,
///   - Odd bits are used to signify that the feature is optional.
///   - Ideally, a feature can be introduced as optional (odd bits) and later upgraded to be compulsory (even bits)
pub mod feature_bits {
    use super::*;
    declare_feature_bits_and_methods! {
        GOSSIP_QUERIES, 1;
        BASIC_MPP, 3;
        TLV_ONION_PAYLOAD, 5;
        CHANNEL_REBALANCE, 9;
        // more features ...
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureVector {
    inner: Vec<u8>,
}

impl Default for FeatureVector {
    fn default() -> Self {
        let mut feature = Self::new();
        feature.set_gossip_queries_required();
        feature.set_basic_mpp_optional();
        feature.set_tlv_onion_payload_required();

        // set other default features here
        // ...

        feature
    }
}

impl FeatureVector {
    pub fn new() -> Self {
        Self { inner: vec![] }
    }

    pub fn from(bytes: Vec<u8>) -> Self {
        Self { inner: bytes }
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.inner.clone()
    }

    fn index(bit: FeatureBit, len: usize) -> Option<(usize, u16)> {
        let byte_idx = (bit / 8) as usize;
        let bit_idx = bit % 8;
        (byte_idx < len).then(|| (len - byte_idx - 1, bit_idx))
    }

    pub fn is_set(&self, bit: FeatureBit) -> bool {
        Self::index(bit, self.inner.len())
            .is_some_and(|(idx, bit_idx)| (self.inner[idx] >> bit_idx) & 1 == 1)
    }

    fn set(&mut self, bit: FeatureBit, set: bool) {
        let len = ((bit / 8) + 1) as usize;
        if self.inner.len() < len {
            self.inner.resize(len, 0);
        }
        if let Some((idx, bit_idx)) = Self::index(bit, self.inner.len()) {
            let mask = 1 << bit_idx;
            if set {
                self.inner[idx] |= mask;
            } else {
                self.inner[idx] &= !mask;
            }
        }
    }

    pub fn enabled_features(&self) -> Vec<FeatureBit> {
        self.inner
            .iter()
            .enumerate()
            .flat_map(|(byte_idx, &byte)| {
                (0..8).filter_map(move |bit_idx| {
                    if (byte >> bit_idx) & 1 == 1 {
                        let bit = ((self.inner.len() - byte_idx - 1) * 8 + bit_idx) as FeatureBit;
                        Some(bit)
                    } else {
                        None
                    }
                })
            })
            .collect()
    }

    pub fn enabled_features_names(&self) -> Vec<&'static str> {
        self.enabled_features()
            .into_iter()
            .map(feature_bits::feature_bit_name)
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
        self.is_set(bit)
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

impl fmt::Debug for FeatureVector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeatureVector")
            .field("features", &self.enabled_features_names())
            .finish()
    }
}
