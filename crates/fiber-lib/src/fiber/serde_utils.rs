use molecule::prelude::Entity;
use musig2::{
    BinaryEncoding, CompactSignature, PartialSignature, PubNonce, SCHNORR_SIGNATURE_SIZE,
};
use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{serde_conv, DeserializeAs, SerializeAs};

pub fn from_hex<'de, D, E>(deserializer: D) -> Result<E, D::Error>
where
    D: Deserializer<'de>,
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    String::deserialize(deserializer)
        .and_then(|string| {
            if string.len() < 2 || &string[..2].to_lowercase() != "0x" {
                return Err(Error::custom(format!(
                    "hex string does not start with 0x: {}",
                    &string
                )));
            };
            hex::decode(&string[2..]).map_err(|err| {
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
    serializer.serialize_str(&format!("0x{}", &hex::encode(e.as_ref())))
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
        to_hex(source, serializer)
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

/// A custom serde wrapper for `musig2::PartialSignature` that serializes as a fixed-size
/// `[u8; 32]` array (no length prefix in bincode). This is compatible with the format
/// used by musig2 0.0.11's derive(Serialize), ensuring backward compatibility with
/// data stored before the musig2 0.2.4 upgrade.
pub struct PartialSignatureAsBytes;

impl SerializeAs<PartialSignature> for PartialSignatureAsBytes {
    fn serialize_as<S>(sig: &PartialSignature, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes: [u8; 32] = sig.serialize();
        bytes.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, PartialSignature> for PartialSignatureAsBytes {
    fn deserialize_as<D>(deserializer: D) -> Result<PartialSignature, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = <[u8; 32]>::deserialize(deserializer)?;
        PartialSignature::from_slice(&bytes).map_err(serde::de::Error::custom)
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
