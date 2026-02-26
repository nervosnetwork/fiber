//! Invoice-related types: status, currency, hash algorithm, script wrapper, signature.

use crate::serde_utils::EntityHex;

use bech32::{u5, FromBase32, ToBase32, WriteBase32};
use bitcoin::hashes::{sha256::Hash as Sha256, Hash as _};
use ckb_hash::blake2b_256;
use ckb_types::packed::Script as PackedScript;
use molecule::prelude::Byte;
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::cmp::Ordering;
use std::fmt::Display;
use std::str::FromStr;

// ============================================================
// Invoice status
// ============================================================

/// The status of a CKB invoice.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum CkbInvoiceStatus {
    /// The invoice is open and can be paid.
    Open,
    /// The invoice is cancelled.
    Cancelled,
    /// The invoice is expired.
    Expired,
    /// The invoice is received, but not settled yet.
    Received,
    /// The invoice is paid.
    Paid,
}

impl Display for CkbInvoiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CkbInvoiceStatus::Open => write!(f, "Open"),
            CkbInvoiceStatus::Cancelled => write!(f, "Cancelled"),
            CkbInvoiceStatus::Expired => write!(f, "Expired"),
            CkbInvoiceStatus::Received => write!(f, "Received"),
            CkbInvoiceStatus::Paid => write!(f, "Paid"),
        }
    }
}

// ============================================================
// Currency
// ============================================================

/// The currency of the invoice, can also represent the CKB network chain.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, Default)]
pub enum Currency {
    /// The mainnet currency of CKB.
    Fibb,
    /// The testnet currency of the CKB network.
    Fibt,
    /// The devnet currency of the CKB network.
    #[default]
    Fibd,
}

impl Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Currency::Fibb => write!(f, "fibb"),
            Currency::Fibt => write!(f, "fibt"),
            Currency::Fibd => write!(f, "fibd"),
        }
    }
}

/// Error for unknown currency
#[derive(thiserror::Error, Debug)]
#[error("Unknown currency: {0}")]
pub struct UnknownCurrencyError(pub String);

impl FromStr for Currency {
    type Err = UnknownCurrencyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fibb" => Ok(Self::Fibb),
            "fibt" => Ok(Self::Fibt),
            "fibd" => Ok(Self::Fibd),
            _ => Err(UnknownCurrencyError(s.to_string())),
        }
    }
}

impl TryFrom<u8> for Currency {
    type Error = UnknownCurrencyError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            0 => Ok(Self::Fibb),
            1 => Ok(Self::Fibt),
            2 => Ok(Self::Fibd),
            _ => Err(UnknownCurrencyError(byte.to_string())),
        }
    }
}

// ============================================================
// Hash algorithm
// ============================================================

/// HashAlgorithm is the hash algorithm used in the hash lock.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    /// The default hash algorithm, CkbHash
    #[default]
    CkbHash = 0,
    /// The sha256 hash algorithm
    Sha256 = 1,
}

/// Error for unknown hash algorithm
#[derive(thiserror::Error, Debug)]
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

impl HashAlgorithm {
    pub fn supported_algorithms() -> Vec<HashAlgorithm> {
        vec![HashAlgorithm::CkbHash, HashAlgorithm::Sha256]
    }

    pub fn hash<T: AsRef<[u8]>>(&self, s: T) -> [u8; 32] {
        match self {
            HashAlgorithm::CkbHash => blake2b_256(s),
            HashAlgorithm::Sha256 => sha256(s),
        }
    }
}

/// SHA-256 hash helper function.
pub fn sha256<T: AsRef<[u8]>>(s: T) -> [u8; 32] {
    Sha256::hash(s.as_ref()).to_byte_array()
}

impl TryFrom<Byte> for HashAlgorithm {
    type Error = UnknownHashAlgorithmError;

    fn try_from(value: Byte) -> Result<Self, Self::Error> {
        let value: u8 = value.into();
        value.try_into()
    }
}

// ============================================================
// CkbScript wrapper
// ============================================================

/// A wrapper around `ckb_types::packed::Script` with hex serialization.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbScript(#[serde_as(as = "EntityHex")] pub PackedScript);

// ============================================================
// InvoiceSignature
// ============================================================

/// Recoverable signature for an invoice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvoiceSignature(pub RecoverableSignature);

impl PartialOrd for InvoiceSignature {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InvoiceSignature {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .serialize_compact()
            .1
            .cmp(&other.0.serialize_compact().1)
    }
}

impl Serialize for InvoiceSignature {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> Result<<S as serde::Serializer>::Ok, <S as serde::Serializer>::Error>
    where
        S: serde::Serializer,
    {
        let base32: Vec<u8> = self.to_base32().iter().map(|x| x.to_u8()).collect();
        let hex_str = hex::encode(base32);
        hex_str.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InvoiceSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, <D as serde::Deserializer<'de>>::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let signature_hex: String = String::deserialize(deserializer)?;
        let signature_bytes = hex::decode(signature_hex).map_err(serde::de::Error::custom)?;
        let base32_values = signature_bytes
            .iter()
            .map(|x| u5::try_from_u8(*x))
            .collect::<Result<Vec<u5>, _>>()
            .map_err(serde::de::Error::custom)?;
        InvoiceSignature::from_base32(&base32_values).map_err(serde::de::Error::custom)
    }
}

struct BytesToBase32<'a, W: WriteBase32 + 'a> {
    writer: &'a mut W,
    buffer: u8,
    buffer_bits: u8,
}

impl<'a, W: WriteBase32> BytesToBase32<'a, W> {
    fn new(writer: &'a mut W) -> Self {
        BytesToBase32 {
            writer,
            buffer: 0,
            buffer_bits: 0,
        }
    }

    fn append(&mut self, byte: u8) -> Result<(), <W as WriteBase32>::Err> {
        let mut bits_remaining = 8;
        while bits_remaining > 0 {
            let bits_to_take = std::cmp::min(5 - self.buffer_bits, bits_remaining);
            self.buffer <<= bits_to_take;
            self.buffer |= (byte >> (bits_remaining - bits_to_take)) & ((1 << bits_to_take) - 1);
            self.buffer_bits += bits_to_take;
            bits_remaining -= bits_to_take;

            if self.buffer_bits == 5 {
                self.writer
                    .write_u5(u5::try_from_u8(self.buffer).expect("buffer is 5 bits"))?;
                self.buffer = 0;
                self.buffer_bits = 0;
            }
        }
        Ok(())
    }

    fn finalize(mut self) -> Result<(), <W as WriteBase32>::Err> {
        if self.buffer_bits > 0 {
            self.buffer <<= 5 - self.buffer_bits;
            self.writer
                .write_u5(u5::try_from_u8(self.buffer).expect("buffer is at most 5 bits"))?;
        }
        Ok(())
    }
}

impl ToBase32 for InvoiceSignature {
    fn write_base32<W: WriteBase32>(&self, writer: &mut W) -> Result<(), <W as WriteBase32>::Err> {
        let mut converter = BytesToBase32::new(writer);
        let (recovery_id, signature) = self.0.serialize_compact();
        for v in signature
            .iter()
            .chain(std::iter::once(&(i32::from(recovery_id) as u8)))
        {
            converter.append(*v)?;
        }
        converter.finalize()
    }
}

impl FromBase32 for InvoiceSignature {
    type Err = anyhow::Error;

    fn from_base32(field_data: &[u5]) -> Result<InvoiceSignature, Self::Err> {
        if field_data.len() < 104 {
            return Err(anyhow::anyhow!(
                "InvoiceSignature TryFrom<[u5]> failed: unexpected length {}",
                field_data.len()
            ));
        }

        let raw_bytes = Vec::<u8>::from_base32(field_data)?;
        if raw_bytes.len() != 65 {
            return Err(anyhow::anyhow!(
                "InvoiceSignature TryFrom<[u5]> failed: unexpected byte length {}",
                raw_bytes.len()
            ));
        }
        let recovery_id = RecoveryId::try_from(raw_bytes[64] as i32)?;
        let signature = RecoverableSignature::from_compact(&raw_bytes[0..64], recovery_id)?;
        Ok(InvoiceSignature(signature))
    }
}
