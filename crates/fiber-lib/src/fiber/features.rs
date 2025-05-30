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

/// Feature bits and methods for the Fiber protocol
/// Pair bits, ideally, a feature can be introduced as optional (odd bits)
/// and later upgraded to be compulsory (even bits)
///   - Even bits are used to signify that the feature is required,
///   - Odd bits are used to signify that the feature is optional.
pub mod feature_bits {
    use super::*;
    declare_feature_bits_and_methods! {
        GOSSIP_QUERIES, 1;
        BASIC_MPP, 3;
        TLV_ONION_PAYLOAD, 5;
        CHANNEL_REBALANCE, 7;
        HTLC_RESOLUTION, 9;
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

    pub fn enabled_features_names(&self) -> Vec<&'static str> {
        self.enabled_features()
            .into_iter()
            .map(feature_bits::feature_bit_name)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.iter().all(|&b| b == 0)
    }

    #[cfg(test)]
    pub fn set_feature(&mut self, bit: FeatureBit) {
        self.set(bit, true);
    }

    #[cfg(test)]
    pub fn unset_feature(&mut self, bit: FeatureBit) {
        self.set(bit, false);
    }

    pub fn requires_feature(&self, bit: FeatureBit) -> bool {
        self.is_set(bit) && bit % 2 == 0
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
