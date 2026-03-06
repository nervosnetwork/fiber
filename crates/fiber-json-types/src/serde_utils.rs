//! Serde utilities for hex serialization of types used in JSON RPC.
//!
//! These are self-contained copies of the helpers from fiber-types, so that
//! fiber-json-types can be compiled without depending on fiber-types.

use molecule::prelude::Entity;
use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{serde_as, serde_conv, DeserializeAs, SerializeAs};

pub fn from_hex<'de, D, E>(deserializer: D) -> Result<E, D::Error>
where
    D: Deserializer<'de>,
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    String::deserialize(deserializer)
        .and_then(|string| {
            let hex_str = string
                .strip_prefix("0x")
                .or_else(|| string.strip_prefix("0X"))
                .unwrap_or(&string);
            hex::decode(hex_str).map_err(|err| {
                Error::custom(format!(
                    "failed to decode hex string {}: {:?}",
                    &string, err
                ))
            })
        })
        .and_then(|vec| {
            vec.try_into().map_err(|err| {
                Error::custom(format!("failed to convert vector into type: {:?}", err))
            })
        })
}

fn to_hex_with_prefix<E, S>(e: E, serializer: S, with_prefix: bool) -> Result<S::Ok, S::Error>
where
    E: AsRef<[u8]>,
    S: Serializer,
{
    let hex_str = hex::encode(e.as_ref());
    let prefix = if with_prefix { "0x" } else { "" };
    serializer.serialize_str(&format!("{}{}", prefix, hex_str))
}

pub struct SliceHex;

impl<T> SerializeAs<T> for SliceHex
where
    T: AsRef<[u8]>,
{
    fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        to_hex_with_prefix(source, serializer, true)
    }
}

impl<'de, T> DeserializeAs<'de, T> for SliceHex
where
    T: TryFrom<Vec<u8>>,
    T::Error: core::fmt::Debug,
{
    fn deserialize_as<D>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        from_hex(deserializer)
    }
}

pub struct EntityHex;

impl<T> SerializeAs<T> for EntityHex
where
    T: Entity,
{
    fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        to_hex_with_prefix(source.as_slice(), serializer, true)
    }
}

impl<'de, T> DeserializeAs<'de, T> for EntityHex
where
    T: Entity,
{
    fn deserialize_as<D>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v: Vec<u8> = from_hex(deserializer)?;
        T::from_slice(&v).map_err(Error::custom)
    }
}

macro_rules! uint_as_hex {
    ($name:ident, $ty:ty) => {
        serde_conv!(
            pub $name,
            $ty,
            |u: &$ty| format!("0x{:x}", u),
            |hex: String| -> Result<$ty, String> {
                let bytes = hex.as_bytes();
                if bytes.len() < 3 || &bytes[..2] != b"0x" {
                    return Err(format!("uint hex string does not start with 0x: {}", hex));
                }
                if bytes.len() > 3 && &bytes[2..3] == b"0" {
                    return Err(format!(
                        "uint hex string starts with redundant leading zeros: {}",
                        hex
                    ));
                };
                <$ty>::from_str_radix(&hex[2..], 16)
                    .map_err(|err| format!("failed to parse uint hex {}: {:?}", hex, err))
            }
        );
    };
}

uint_as_hex!(U128Hex, u128);
uint_as_hex!(U64Hex, u64);
uint_as_hex!(U32Hex, u32);

/// A u32 wrapper that serializes/deserializes as a hex string ("0x...").
/// Unlike `U32Hex` (a serde_conv helper for use with `#[serde_as]`), this type
/// implements Serialize/Deserialize directly, making it suitable for use inside
/// adjacently-tagged enums and other contexts where serde_as doesn't apply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HexU32(pub u32);

impl serde::Serialize for HexU32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{:x}", self.0))
    }
}

impl<'de> serde::Deserialize<'de> for HexU32 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex: String = String::deserialize(deserializer)?;
        let bytes = hex.as_bytes();
        if bytes.len() < 3 || &bytes[..2] != b"0x" {
            return Err(D::Error::custom(format!(
                "uint hex string does not start with 0x: {}",
                hex
            )));
        }
        if bytes.len() > 3 && &bytes[2..3] == b"0" {
            return Err(D::Error::custom(format!(
                "uint hex string starts with redundant leading zeros: {}",
                hex
            )));
        }
        u32::from_str_radix(&hex[2..], 16)
            .map(HexU32)
            .map_err(|err| D::Error::custom(format!("failed to parse uint hex {}: {:?}", hex, err)))
    }
}

impl From<u32> for HexU32 {
    fn from(v: u32) -> Self {
        HexU32(v)
    }
}

impl From<HexU32> for u32 {
    fn from(v: HexU32) -> Self {
        v.0
    }
}

pub struct SliceHexNoPrefix;

impl<T> SerializeAs<T> for SliceHexNoPrefix
where
    T: AsRef<[u8]>,
{
    fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        to_hex_with_prefix(source, serializer, false)
    }
}

impl<'de, T> DeserializeAs<'de, T> for SliceHexNoPrefix
where
    T: TryFrom<Vec<u8>>,
    T::Error: core::fmt::Debug,
{
    fn deserialize_as<D>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        from_hex(deserializer)
    }
}

/// A compressed public key (33 bytes), serialized as hex without `0x` prefix.
///
/// On deserialization, only hex format and 33-byte length are checked (no secp256k1 validation).
/// Both `0x`-prefixed and non-prefixed hex strings are accepted on input.
/// Cryptographic validation is left to the RPC layer's conversion to internal `Pubkey`.
#[serde_as]
#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pubkey(#[serde_as(as = "SliceHexNoPrefix")] pub [u8; 33]);

impl Pubkey {
    /// Create a `Pubkey` from a 33-byte slice (no cryptographic validation).
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 33 {
            return Err(format!(
                "invalid pubkey length: expected 33 bytes, got {}",
                bytes.len()
            ));
        }
        let mut arr = [0u8; 33];
        arr.copy_from_slice(bytes);
        Ok(Pubkey(arr))
    }

    /// Return the underlying 33 bytes.
    pub fn as_bytes(&self) -> &[u8; 33] {
        &self.0
    }
}

impl core::fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Pubkey({})", hex::encode(self.0))
    }
}

impl core::fmt::Display for Pubkey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl core::str::FromStr for Pubkey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex_str = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        let bytes =
            hex::decode(hex_str).map_err(|e| format!("invalid pubkey hex '{}': {}", s, e))?;
        Pubkey::from_slice(&bytes)
    }
}

/// A 256-bit hash (32 bytes), serialized as `0x`-prefixed hex string.
///
/// On deserialization, both `0x`-prefixed and non-prefixed hex strings are accepted.
/// No domain-specific validation is performed — the only check is hex format and 32-byte length.
#[serde_as]
#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Hash256(#[serde_as(as = "SliceHex")] pub [u8; 32]);

impl Hash256 {
    /// Create a `Hash256` from a 32-byte slice.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 32 {
            return Err(format!(
                "invalid hash256 length: expected 32 bytes, got {}",
                bytes.len()
            ));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(Hash256(arr))
    }

    /// Return the underlying 32 bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Hash256(0x{})", hex::encode(self.0))
    }
}

impl core::fmt::Display for Hash256 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl core::str::FromStr for Hash256 {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex_str = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        let bytes =
            hex::decode(hex_str).map_err(|e| format!("invalid hash256 hex '{}': {}", s, e))?;
        Hash256::from_slice(&bytes)
    }
}

/// Module for hex serialization of Duration
pub mod duration_hex {
    use core::time::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let nanos = duration.as_secs();
        serializer.serialize_str(&format!("0x{:x}", nanos))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex_str = String::deserialize(deserializer)?;
        let seconds = u64::from_str_radix(&hex_str[2..], 16).map_err(|err| {
            serde::de::Error::custom(format!(
                "failed to parse duration hex {}: {:?}",
                hex_str, err
            ))
        })?;

        Ok(Duration::from_secs(seconds))
    }
}
