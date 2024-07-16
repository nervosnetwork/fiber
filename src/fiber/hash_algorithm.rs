use bitcoin::hashes::{sha256::Hash as Sha256, Hash as _};
use ckb_hash::blake2b_256;
use ckb_types::packed;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    #[default]
    CkbHash = 0,
    Sha256 = 1,
}

impl HashAlgorithm {
    pub fn hash<T: AsRef<[u8]>>(&self, s: T) -> [u8; 32] {
        match self {
            HashAlgorithm::CkbHash => blake2b_256(s),
            HashAlgorithm::Sha256 => sha256(s),
        }
    }

    pub fn supported_algorithms() -> Vec<HashAlgorithm> {
        vec![HashAlgorithm::CkbHash, HashAlgorithm::Sha256]
    }
}

/// The error type wrap various ser/de errors.
#[derive(Error, Debug)]
#[error("Unknown Hash Algorithm: {0}")]
pub struct UnknownHashAlgorithmError(pub u8);

impl TryFrom<u8> for HashAlgorithm {
    type Error = UnknownHashAlgorithmError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(HashAlgorithm::CkbHash),
            1 => Ok(HashAlgorithm::Sha256),
            _ => Err(UnknownHashAlgorithmError(value)),
        }
    }
}

impl TryFrom<packed::Byte> for HashAlgorithm {
    type Error = UnknownHashAlgorithmError;

    fn try_from(value: packed::Byte) -> Result<Self, Self::Error> {
        let value: u8 = value.into();
        value.try_into()
    }
}

pub fn sha256<T: AsRef<[u8]>>(s: T) -> [u8; 32] {
    Sha256::hash(s.as_ref()).to_byte_array()
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_hash_algorithm_serialization_sha256() {
        let algorithm = super::HashAlgorithm::Sha256;
        let serialized = serde_json::to_string(&algorithm).unwrap();
        assert_eq!(serialized, r#""sha256""#);
        let deserialized: super::HashAlgorithm = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, algorithm);
    }

    #[test]
    fn test_hash_algorithm_serialization_ckb_hash() {
        let algorithm = super::HashAlgorithm::CkbHash;
        let serialized = serde_json::to_string(&algorithm).unwrap();
        assert_eq!(serialized, r#""ckb_hash""#);
        let deserialized: super::HashAlgorithm = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, algorithm);
    }
}
