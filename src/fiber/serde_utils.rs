use anyhow::anyhow;
use molecule::prelude::Entity;
use musig2::{BinaryEncoding, CompactSignature, PubNonce, SCHNORR_SIGNATURE_SIZE};
use serde::{de::Error, Deserialize, Deserializer, Serializer};
use serde_with::{serde_conv, DeserializeAs, SerializeAs};

pub fn deserialize_entity_from_hex_str<E>(str: &str) -> Result<E, anyhow::Error>
where
    E: Entity,
{
    let v: Vec<u8> = deserialize_from_hex_str(str)?;
    E::from_slice(&v).map_err(|err| anyhow!("failed to convert slice into entity: {:?}", err))
}

pub fn serialize_entity_to_hex_string<E>(entity: &E) -> String
where
    E: Entity,
{
    serialize_to_hex_string(entity.as_slice())
}

pub fn deserialize_from_hex_str<E>(string: &str) -> Result<E, anyhow::Error>
where
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    if string.len() < 2 {
        return Err(anyhow!("hex string too short: {}", &string));
    };
    let start = if &string[..2].to_lowercase() == "0x" {
        2
    } else {
        0
    };
    let vec = hex::decode(&string[start..])
        .map_err(|err| anyhow!("failed to decode hex string {}: {:?}", &string, err))?;
    vec.try_into()
        .map_err(|err| anyhow!("failed to convert vector into type: {:?}", err))
}

pub fn serialize_to_hex_string<E>(e: E) -> String
where
    E: AsRef<[u8]>,
{
    format!("0x{}", &hex::encode(e.as_ref()))
}

pub fn deserialize_from_hex<'de, D, E>(deserializer: D) -> Result<E, D::Error>
where
    D: Deserializer<'de>,
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    String::deserialize(deserializer)
        .and_then(|string| deserialize_from_hex_str(&string).map_err(Error::custom))
}

pub fn serialize_to_hex<E, S>(e: E, serializer: S) -> Result<S::Ok, S::Error>
where
    E: AsRef<[u8]>,
    S: Serializer,
{
    serializer.serialize_str(&serialize_to_hex_string(e))
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
        serialize_to_hex(source, serializer)
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
        deserialize_from_hex(deserializer)
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
        serialize_to_hex(source.as_slice(), serializer)
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
        let v: Vec<u8> = deserialize_from_hex(deserializer)?;
        T::from_slice(&v).map_err(Error::custom)
    }
}

macro_rules! uint_as_hex {
    ($name:ident, $ty:ty) => {
        serde_conv!(
            pub $name,
            $ty,
            |u: &$ty| format!("0x{:x}", u),
            |hex: &str| -> Result<$ty, String> {
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
