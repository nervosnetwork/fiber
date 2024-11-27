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
            if string.len() < 2 || &string[..2].to_lowercase() != "0x" {
                return Err(Error::custom("hex string should start with 0x"));
            };
            hex::decode(&string[2..])
                .map_err(|err| Error::custom(format!("failed to decode hex: {:?}", err)))
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
            |hex: &str| -> Result<$ty, String> {
                let bytes = hex.as_bytes();
                if bytes.len() < 3 || &bytes[..2] != b"0x" {
                    return Err("hex string should start with 0x".to_string());
                }
                if bytes.len() > 3 && &bytes[2..3] == b"0" {
                    return Err("hex string should not start with redundant leading zeros".to_string());
                };
                <$ty>::from_str_radix(&hex[2..], 16)
                    .map_err(|err| format!("failed to parse hex: {:?}", err))
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
        let bytes: &[u8] = Deserialize::deserialize(deserializer)?;
        if bytes.len() != SCHNORR_SIGNATURE_SIZE {
            return Err(serde::de::Error::custom("expected 64 bytes"));
        }
        let mut array = [0u8; SCHNORR_SIGNATURE_SIZE];
        array.copy_from_slice(bytes);
        CompactSignature::from_bytes(&array).map_err(serde::de::Error::custom)
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
        let bytes: &[u8] = Deserialize::deserialize(deserializer)?;
        if bytes.len() != 66 {
            return Err(serde::de::Error::custom("expected 66 bytes"));
        }
        let mut array = [0u8; 66];
        array.copy_from_slice(bytes);
        PubNonce::from_bytes(&array).map_err(serde::de::Error::custom)
    }
}
