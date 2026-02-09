use molecule::prelude::Entity;
use musig2::{BinaryEncoding, CompactSignature, PubNonce, SCHNORR_SIGNATURE_SIZE};
use serde::{de::Error, Deserialize, Deserializer, Serializer};
use serde_with::{serde_conv, DeserializeAs, SerializeAs};

pub fn from_hex<'de, D, E>(deserializer: D) -> Result<E, D::Error>
where
    D: Deserializer<'de>,
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    String::deserialize(deserializer)
        .and_then(|string| {
            // Accept both "0x" prefixed and non-prefixed hex strings for backward compatibility
            // with secp256k1::PublicKey's serde format which doesn't use "0x" prefix
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

pub fn to_hex<E, S>(e: E, serializer: S) -> Result<S::Ok, S::Error>
where
    E: AsRef<[u8]>,
    S: Serializer,
{
    to_hex_with_prefix(e, serializer, true)
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

pub struct EntityHex;

impl<T> SerializeAs<T> for EntityHex
where
    T: Entity,
{
    fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        to_hex(source.as_slice(), serializer)
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
                    return Err(format!("uint hex string starts with redundant leading zeros: {}", hex));
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
uint_as_hex!(U16Hex, u16);

pub struct CompactSignatureAsBytes;

impl SerializeAs<CompactSignature> for CompactSignatureAsBytes {
    fn serialize_as<S>(signature: &CompactSignature, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&signature.to_bytes())
    }
}

impl<'de> DeserializeAs<'de, CompactSignature> for CompactSignatureAsBytes {
    fn deserialize_as<D>(deserializer: D) -> Result<CompactSignature, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() != SCHNORR_SIGNATURE_SIZE {
            return Err(serde::de::Error::custom("expected 64 bytes"));
        }
        CompactSignature::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

pub struct PubNonceAsBytes;

impl SerializeAs<PubNonce> for PubNonceAsBytes {
    fn serialize_as<S>(nonce: &PubNonce, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&nonce.to_bytes())
    }
}

impl<'de> DeserializeAs<'de, PubNonce> for PubNonceAsBytes {
    fn deserialize_as<D>(deserializer: D) -> Result<PubNonce, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() != 66 {
            return Err(serde::de::Error::custom("expected 66 bytes"));
        }
        PubNonce::from_bytes(&bytes).map_err(serde::de::Error::custom)
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

pub struct SliceBase58;

impl<T> SerializeAs<T> for SliceBase58
where
    T: AsRef<[u8]>,
{
    fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&bs58::encode(source).into_string())
    }
}

impl<'de, T> DeserializeAs<'de, T> for SliceBase58
where
    T: TryFrom<Vec<u8>>,
    T::Error: core::fmt::Debug,
{
    fn deserialize_as<D>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|s| {
                bs58::decode(&s).into_vec().map_err(|err| {
                    Error::custom(format!("failed to decode base58 string {}: {:?}", &s, err))
                })
            })
            .and_then(|vec| {
                vec.try_into().map_err(|err| {
                    Error::custom(format!("failed to convert vector into type: {:?}", err))
                })
            })
    }
}
