use molecule::prelude::Entity;
use serde::{Deserialize, Deserializer, Serializer};
use serde_with::{DeserializeAs, SerializeAs};

pub fn from_base64<'de, D, E>(deserializer: D) -> Result<E, D::Error>
where
    D: Deserializer<'de>,
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    use serde::de::Error;

    String::deserialize(deserializer)
        .and_then(|string| {
            base64::decode(string)
                .map_err(|err| Error::custom(format!("failed to decode base64: {:?}", err)))
        })
        .and_then(|vec| {
            vec.try_into().map_err(|err| {
                serde::de::Error::custom(format!("failed to convert vector into type: {:?}", err))
            })
        })
}

pub fn to_base64<E, S>(e: E, serializer: S) -> Result<S::Ok, S::Error>
where
    E: AsRef<[u8]>,
    S: Serializer,
{
    serializer.serialize_str(&base64::encode(e.as_ref()))
}

pub fn from_hex<'de, D, E>(deserializer: D) -> Result<E, D::Error>
where
    D: Deserializer<'de>,
    E: TryFrom<Vec<u8>>,
    E::Error: core::fmt::Debug,
{
    use serde::de::Error;

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
                serde::de::Error::custom(format!("failed to convert vector into type: {:?}", err))
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
        T::from_slice(&v).map_err(serde::de::Error::custom)
    }
}
