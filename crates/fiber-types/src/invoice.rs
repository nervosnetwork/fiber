//! Invoice-related types: status, currency, hash algorithm, script wrapper, signature.

use crate::serde_utils::EntityHex;

use arcode::bitbit::{BitReader, BitWriter, MSB};
use arcode::{ArithmeticDecoder, ArithmeticEncoder, EOFKind, Model};
use bech32::{encode, u5, FromBase32, ToBase32, Variant, WriteBase32};
use bitcoin::hashes::{sha256::Hash as Sha256, Hash as _};
use ckb_hash::blake2b_256;
use ckb_types::packed::Script as PackedScript;
use molecule::prelude::Byte;
use nom::{branch::alt, combinator::opt};
use nom::{
    bytes::{complete::take_while1, streaming::tag},
    IResult,
};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::cmp::Ordering;
use std::fmt::Display;
use std::io::{Cursor, Result as IoResult};
use std::num::ParseIntError;
use std::str::FromStr;
use thiserror::Error;

// ============================================================
// Invoice error types
// ============================================================

/// Wrapper for molecule verification errors.
#[derive(Error, Debug)]
pub struct VerificationError(pub molecule::error::VerificationError);

impl PartialEq for VerificationError {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

impl Display for VerificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Errors that can occur when parsing or validating an invoice.
#[derive(Error, PartialEq, Debug)]
pub enum InvoiceError {
    /// Bech32 encoding/decoding error.
    #[error("Bech32 error: {0}")]
    Bech32Error(bech32::Error),
    /// Molecule serialization error.
    #[error("Molecule error: {0}")]
    MoleculeError(VerificationError),
    /// Failed to parse amount from HRP.
    #[error("Failed to parse amount: {0}")]
    ParseAmountError(ParseIntError),
    /// Unknown currency in HRP.
    #[error("Unknown currency: {0}")]
    UnknownCurrency(String),
    /// Unknown SI prefix in amount.
    #[error("Unknown si prefix: {0}")]
    UnknownSiPrefix(String),
    /// Malformed HRP.
    #[error("Parsing failed with malformed HRP: {0}")]
    MalformedHRP(String),
    /// Data part is too short.
    #[error("Too short data part")]
    TooShortDataPart,
    /// Unexpected end of tagged fields.
    #[error("Unexpected end of tagged fields")]
    UnexpectedEndOfTaggedFields,
    /// Integer overflow error.
    #[error("Integer overflow error")]
    IntegerOverflowError,
    /// Invalid recovery ID in signature.
    #[error("Invalid recovery id")]
    InvalidRecoveryId,
    /// Invalid slice length.
    #[error("Invalid slice length: {0}")]
    InvalidSliceLength(String),
    /// Invalid signature.
    #[error("Invalid signature")]
    InvalidSignature,
    /// Duplicated attribute key.
    #[error("Duplicated attribute key: {0}")]
    DuplicatedAttributeKey(String),
    /// Payment secret is required for MPP payments.
    #[error("Payment secret is required for MPP payments")]
    PaymentSecretRequiredForMpp,
    /// Both payment_hash and payment_preimage are set.
    #[error("Both payment_hash and payment_preimage are set")]
    BothPaymenthashAndPreimage,
    /// Neither payment_hash nor payment_preimage is set.
    #[error("Neither payment_hash nor payment_preimage is set")]
    NeitherPaymenthashNorPreimage,
    /// An error occurred during signing.
    #[error("Sign error")]
    SignError,
    /// Hex decode error.
    #[error("Hex decode error: {0}")]
    HexDecodeError(#[from] hex::FromHexError),
    /// Duplicated invoice found.
    #[error("Duplicated invoice found: {0}")]
    DuplicatedInvoice(String),
    /// Description is too long.
    #[error("Description with length of {0} is too long, max length is 639")]
    DescriptionTooLong(usize),
    /// Invoice not found.
    #[error("Invoice not found")]
    InvoiceNotFound,
    /// Invoice already exists.
    #[error("Invoice already exists")]
    InvoiceAlreadyExists,
    /// Deprecated attribute.
    #[error("Deprecated attribute: {0}")]
    DeprecatedAttribute(String),
}

// ============================================================
// Compression utilities
// ============================================================

/// Size of the signature in u5 encoding.
pub const SIGNATURE_U5_SIZE: usize = 104;

/// Maximum allowed length for an invoice description.
pub const MAX_DESCRIPTION_LENGTH: usize = 639;

/// Encodes bytes and returns the compressed form.
/// This is used for encoding the invoice data, to make the final Invoice encoded address shorter.
pub fn ar_encompress(data: &[u8]) -> IoResult<Vec<u8>> {
    let mut model = Model::builder().num_bits(8).eof(EOFKind::EndAddOne).build();
    let mut compressed_writer = BitWriter::new(Cursor::new(vec![]));
    let mut encoder = ArithmeticEncoder::new(48);
    for &sym in data {
        encoder.encode(sym as u32, &model, &mut compressed_writer)?;
        model.update_symbol(sym as u32);
    }

    encoder.encode(model.eof(), &model, &mut compressed_writer)?;
    encoder.finish_encode(&mut compressed_writer)?;
    compressed_writer.pad_to_byte()?;

    Ok(compressed_writer.get_ref().get_ref().clone())
}

/// Decompresses the data.
pub fn ar_decompress(data: &[u8]) -> IoResult<Vec<u8>> {
    let mut model = Model::builder().num_bits(8).eof(EOFKind::EndAddOne).build();
    let mut input_reader = BitReader::<_, MSB>::new(data);
    let mut decoder = ArithmeticDecoder::new(48);
    let mut decompressed_data = vec![];

    while !decoder.finished() {
        let sym = decoder.decode(&model, &mut input_reader)?;
        model.update_symbol(sym);
        decompressed_data.push(sym as u8);
    }

    decompressed_data.pop(); // remove the EOF
    Ok(decompressed_data)
}

/// Construct the invoice's HRP and signatureless data into a preimage to be hashed.
pub fn construct_invoice_preimage(hrp_bytes: &[u8], data_without_signature: &[u5]) -> Vec<u8> {
    let mut preimage = Vec::<u8>::from(hrp_bytes);

    let mut data_part = Vec::from(data_without_signature);
    let overhang = (data_part.len() * 5) % 8;
    if overhang > 0 {
        // add padding if data does not end at a byte boundary
        data_part.push(u5::try_from_u8(0).expect("u5 from u8"));

        // if overhang is in (1..3) we need to add u5(0) padding two times
        if overhang < 3 {
            data_part.push(u5::try_from_u8(0).expect("u5 from u8"));
        }
    }

    preimage.extend_from_slice(
        &Vec::<u8>::from_base32(&data_part)
            .expect("No padding error may occur due to appended zero above."),
    );
    preimage
}

fn nom_scan_hrp(input: &str) -> IResult<&str, (&str, Option<&str>)> {
    let (input, currency) = alt((tag("fibb"), tag("fibt"), tag("fibd")))(input)?;
    let (input, amount) = opt(take_while1(|c: char| c.is_numeric()))(input)?;
    Ok((input, (currency, amount)))
}

/// Parse the human-readable part of an invoice.
pub fn parse_hrp(input: &str) -> Result<(Currency, Option<u128>), InvoiceError> {
    match nom_scan_hrp(input) {
        Ok((left, (currency, amount))) => {
            if !left.is_empty() {
                return Err(InvoiceError::MalformedHRP(format!(
                    "{}, unexpected ending `{}`",
                    input, left
                )));
            }
            let currency =
                Currency::from_str(currency).map_err(|e| InvoiceError::UnknownCurrency(e.0))?;
            let amount = amount
                .map(|x| x.parse().map_err(InvoiceError::ParseAmountError))
                .transpose()?;
            Ok((currency, amount))
        }
        Err(_) => Err(InvoiceError::MalformedHRP(input.to_string())),
    }
}

// ============================================================
// Invoice status
// ============================================================

/// The currency of the invoice, can also used to represent the CKB network chain.
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

/// The currency of the invoice, can also used to represent the CKB network chain.
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

/// Recoverable signature
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

impl InvoiceSignature {
    /// Parse an `InvoiceSignature` from base32-encoded data, returning `InvoiceError` on failure.
    pub fn from_base32_checked(signature: &[u5]) -> Result<Self, InvoiceError> {
        if signature.len() != SIGNATURE_U5_SIZE {
            return Err(InvoiceError::InvalidSliceLength(
                "InvoiceSignature::from_base32_checked()".into(),
            ));
        }
        let recoverable_signature_bytes =
            Vec::<u8>::from_base32(signature).expect("bytes from base32");
        let sig = &recoverable_signature_bytes[0..64];
        let recovery_id = RecoveryId::try_from(recoverable_signature_bytes[64] as i32)
            .expect("Recovery ID from i32");

        Ok(InvoiceSignature(
            RecoverableSignature::from_compact(sig, recovery_id).expect("signature from compact"),
        ))
    }
}

// ============================================================
// Attribute
// ============================================================

use crate::protocol::FeatureVector;
use crate::serde_utils::{duration_hex, U128Hex, U64Hex};
use crate::Hash256;
use secp256k1::PublicKey;
use std::time::Duration;

/// The attributes of the invoice.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribute {
    /// This attribute is deprecated since v0.6.0. The final TLC timeout, in milliseconds.
    #[serde(with = "U64Hex")]
    FinalHtlcTimeout(u64),
    /// The final TLC minimum expiry delta, in milliseconds. Default is 160 minutes.
    #[serde(with = "U64Hex")]
    FinalHtlcMinimumExpiryDelta(u64),
    /// The expiry time of the invoice, in seconds.
    #[serde(with = "duration_hex")]
    ExpiryTime(Duration),
    /// The description of the invoice.
    Description(String),
    /// The fallback address of the invoice.
    FallbackAddr(String),
    /// The UDT type script of the invoice.
    UdtScript(CkbScript),
    /// The payee public key of the invoice.
    PayeePublicKey(PublicKey),
    /// The hash algorithm of the invoice.
    HashAlgorithm(HashAlgorithm),
    /// The feature flags of the invoice.
    Feature(FeatureVector),
    /// The payment secret of the invoice.
    PaymentSecret(Hash256),
}

// ============================================================
// InvoiceData
// ============================================================

/// The metadata of the invoice.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvoiceData {
    /// The timestamp of the invoice.
    #[serde_as(as = "U128Hex")]
    pub timestamp: u128,
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
    /// The attributes of the invoice, e.g. description, expiry time, etc.
    pub attrs: Vec<Attribute>,
}

// ============================================================
// CkbInvoice
// ============================================================

/// Represents a syntactically and semantically correct Fiber invoice.
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using `CkbInvoiceBuilder`
///  2. using `str::parse::<CkbInvoice>(&str)` (see `CkbInvoice::from_str`)
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbInvoice {
    /// The currency of the invoice.
    pub currency: Currency,
    /// The amount of the invoice.
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,
    /// The signature of the invoice.
    pub signature: Option<InvoiceSignature>,
    /// The invoice data, including the payment hash, timestamp and other attributes.
    pub data: InvoiceData,
}

impl CkbInvoice {
    fn hrp_part(&self) -> String {
        format!(
            "{}{}",
            self.currency,
            self.amount
                .map_or_else(|| "".to_string(), |x| x.to_string()),
        )
    }

    // Use the lossless compression algorithm to compress the invoice data.
    // To make sure the final encoded invoice address is shorter
    fn data_part(&self) -> Vec<u5> {
        let invoice_data = gen_invoice::RawInvoiceData::from(self.data.clone());
        let compressed = ar_encompress(invoice_data.as_slice()).expect("compress invoice data");
        let mut base32 = Vec::with_capacity(compressed.len());
        compressed
            .write_base32(&mut base32)
            .expect("encode in base32");
        base32
    }

    /// Check that the invoice is signed correctly and that key recovery works.
    pub fn check_signature(&self) -> Result<(), InvoiceError> {
        if self.signature.is_none() {
            return Ok(());
        }
        match self.recover_payee_pub_key() {
            Err(secp256k1::Error::InvalidRecoveryId) => {
                return Err(InvoiceError::InvalidRecoveryId)
            }
            Err(secp256k1::Error::InvalidSignature) => return Err(InvoiceError::InvalidSignature),
            Err(e) => panic!("no other error may occur, got {:?}", e),
            Ok(_) => {}
        }

        if !self.validate_signature() {
            return Err(InvoiceError::InvalidSignature);
        }

        Ok(())
    }

    fn validate_signature(&self) -> bool {
        if self.signature.is_none() {
            return true;
        }
        let signature = self.signature.as_ref().expect("expect signature");
        let included_pub_key = self.payee_pub_key();

        let mut recovered_pub_key = Option::None;
        if included_pub_key.is_none() {
            let recovered = match self.recover_payee_pub_key() {
                Ok(pk) => pk,
                Err(_) => return false,
            };
            recovered_pub_key = Some(recovered);
        }

        let pub_key = included_pub_key
            .or(recovered_pub_key.as_ref())
            .expect("One is always present");

        let hash = secp256k1::Message::from_digest_slice(&self.hash()[..])
            .expect("Hash is 32 bytes long, same as MESSAGE_SIZE");

        let verification_result =
            secp256k1::SECP256K1.verify_ecdsa(&hash, &signature.0.to_standard(), pub_key);
        match verification_result {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    fn hash(&self) -> [u8; 32] {
        let hrp = self.hrp_part();
        let data = self.data_part();
        let preimage = construct_invoice_preimage(hrp.as_bytes(), &data);
        let mut hash: [u8; 32] = Default::default();
        hash.copy_from_slice(&Sha256::hash(&preimage).to_byte_array());
        hash
    }

    /// Recovers the public key used for signing the invoice from the recoverable signature.
    pub fn recover_payee_pub_key(&self) -> Result<PublicKey, secp256k1::Error> {
        let hash = secp256k1::Message::from_digest_slice(&self.hash()[..])
            .expect("Hash is 32 bytes long, same as MESSAGE_SIZE");

        secp256k1::SECP256K1.recover_ecdsa(
            &hash,
            &self
                .signature
                .as_ref()
                .expect("signature must be present")
                .0,
        )
    }

    /// Returns the payee public key if set in the invoice attributes.
    pub fn payee_pub_key(&self) -> Option<&PublicKey> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::PayeePublicKey(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns whether the invoice has a signature.
    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }

    /// Returns the payment hash of the invoice.
    pub fn payment_hash(&self) -> &Hash256 {
        &self.data.payment_hash
    }

    /// Returns the amount of the invoice.
    pub fn amount(&self) -> Option<u128> {
        self.amount
    }

    /// Returns the UDT type script if set in the invoice attributes.
    pub fn udt_type_script(&self) -> Option<&PackedScript> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::UdtScript(script) => Some(&script.0),
                _ => None,
            })
            .next()
    }

    /// Returns the expiry time if set in the invoice attributes.
    pub fn expiry_time(&self) -> Option<&Duration> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::ExpiryTime(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns the description if set in the invoice attributes.
    pub fn description(&self) -> Option<&String> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::Description(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns the final TLC minimum expiry delta if set in the invoice attributes.
    pub fn final_tlc_minimum_expiry_delta(&self) -> Option<&u64> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::FinalHtlcMinimumExpiryDelta(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns the fallback address if set in the invoice attributes.
    pub fn fallback_address(&self) -> Option<&String> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::FallbackAddr(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns the hash algorithm if set in the invoice attributes.
    pub fn hash_algorithm(&self) -> Option<&HashAlgorithm> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::HashAlgorithm(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns the payment secret if set in the invoice attributes.
    pub fn payment_secret(&self) -> Option<&Hash256> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::PaymentSecret(val) => Some(val),
                _ => None,
            })
            .next()
    }

    /// Returns whether the invoice allows MPP (multi-part payments).
    pub fn allow_mpp(&self) -> bool {
        self.data
            .attrs
            .iter()
            .any(|attr| matches!(attr, Attribute::Feature(feature) if feature.supports_basic_mpp()))
    }

    /// Returns whether the invoice allows trampoline routing.
    pub fn allow_trampoline_routing(&self) -> bool {
        self.data
            .attrs
            .iter()
            .any(|attr| matches!(attr, Attribute::Feature(feature) if feature.supports_trampoline_routing()))
    }

    /// Returns whether the invoice has expired based on the current time.
    pub fn is_expired(&self) -> bool {
        self.expiry_time().is_some_and(|expiry| {
            self.data
                .timestamp
                .checked_add(expiry.as_millis())
                .is_some_and(|expiry_time| {
                    let now = crate::crate_time::UNIX_EPOCH
                        .elapsed()
                        .expect("Duration since unix epoch")
                        .as_millis();
                    expiry_time < now
                })
        })
    }

    /// Returns whether the TLC expiry is too soon for the given invoice.
    pub fn is_tlc_expire_too_soon(&self, tlc_expiry: u64) -> bool {
        let now = crate::crate_time::UNIX_EPOCH
            .elapsed()
            .expect("Duration since unix epoch")
            .as_millis();
        let required_expiry = now
            + (self
                .final_tlc_minimum_expiry_delta()
                .cloned()
                .unwrap_or_default() as u128);
        (tlc_expiry as u128) < required_expiry
    }

    /// Updates the invoice signature using the provided signing function.
    pub fn update_signature<F>(&mut self, sign_function: F) -> Result<(), InvoiceError>
    where
        F: FnOnce(&secp256k1::Message) -> RecoverableSignature,
    {
        let hash = self.hash();
        let message =
            secp256k1::Message::from_digest_slice(&hash).expect("message from digest slice");
        let signature = sign_function(&message);
        self.signature = Some(InvoiceSignature(signature));
        self.check_signature()?;
        Ok(())
    }
}

impl Display for CkbInvoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hrp = self.hrp_part();
        let mut data = self.data_part();
        data.insert(
            0,
            u5::try_from_u8(if self.signature.is_some() { 1 } else { 0 }).expect("u5 from u8"),
        );
        if let Some(signature) = &self.signature {
            data.extend_from_slice(&signature.to_base32());
        }
        write!(
            f,
            "{}",
            encode(&hrp, data, Variant::Bech32m).expect("encode invoice using Bech32m")
        )
    }
}

impl FromStr for CkbInvoice {
    type Err = InvoiceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hrp, data, var) = bech32::decode(s).map_err(InvoiceError::Bech32Error)?;

        if var == bech32::Variant::Bech32 {
            return Err(InvoiceError::Bech32Error(bech32::Error::InvalidChecksum));
        }

        if data.len() < SIGNATURE_U5_SIZE {
            return Err(InvoiceError::TooShortDataPart);
        }
        let (currency, amount) = parse_hrp(&hrp)?;
        let is_signed = data[0].to_u8() == 1;
        let data_end = if is_signed {
            data.len() - SIGNATURE_U5_SIZE
        } else {
            data.len()
        };
        let data_part =
            Vec::<u8>::from_base32(&data[1..data_end]).map_err(InvoiceError::Bech32Error)?;
        let data_part = ar_decompress(&data_part).expect("decompress invoice data");
        let invoice_data = gen_invoice::RawInvoiceData::from_slice(&data_part)
            .map_err(|err| InvoiceError::MoleculeError(VerificationError(err)))?;
        let signature = if is_signed {
            Some(InvoiceSignature::from_base32(
                &data[data.len() - SIGNATURE_U5_SIZE..],
            )?)
        } else {
            None
        };

        let invoice = CkbInvoice {
            currency,
            amount,
            signature,
            data: invoice_data.try_into().expect("pack invoice data"),
        };
        invoice.check_signature()?;
        Ok(invoice)
    }
}

// ============================================================
// Molecule conversions
// ============================================================

use crate::gen::invoice as gen_invoice;
use ckb_types::prelude::{Pack, Unpack};
use gen_invoice::{
    Description, ExpiryTime, FallbackAddr, Feature, FinalHtlcMinimumExpiryDelta, FinalHtlcTimeout,
    InvoiceAttr, InvoiceAttrUnion, InvoiceAttrsVec, PayeePublicKey, PaymentHash, PaymentSecret,
    RawInvoiceDataBuilder, UdtScript,
};
use molecule::prelude::{Builder, Entity};

/// Converts a `u8` to a molecule `Byte`.
#[allow(dead_code)]
fn u8_to_byte(u: u8) -> Byte {
    Byte::new(u)
}

/// Converts a `[u8]` slice to `[Byte; 32]`.
fn u8_slice_to_bytes(slice: &[u8]) -> Result<[Byte; 32], &'static str> {
    let vec: Vec<Byte> = slice.iter().map(|&x| Byte::new(x)).collect();
    let boxed_slice = vec.into_boxed_slice();
    let boxed_array: Box<[Byte; 32]> = match boxed_slice.try_into() {
        Ok(ba) => ba,
        Err(_) => return Err("Slice length doesn't match array length"),
    };
    Ok(*boxed_array)
}

/// Converts molecule bytes to `[u8; 32]`.
fn bytes_to_u8_array(array: &molecule::bytes::Bytes) -> [u8; 32] {
    let mut res = [0u8; 32];
    res.copy_from_slice(array);
    res
}

impl From<InvoiceData> for gen_invoice::RawInvoiceData {
    fn from(data: InvoiceData) -> Self {
        RawInvoiceDataBuilder::default()
            .timestamp(data.timestamp.pack())
            .payment_hash(
                PaymentHash::new_builder()
                    .set(
                        u8_slice_to_bytes(data.payment_hash.as_ref()).expect("bytes from u8 slice"),
                    )
                    .build(),
            )
            .attrs(
                InvoiceAttrsVec::new_builder()
                    .set(
                        data.attrs
                            .iter()
                            .map(|a| a.to_owned().into())
                            .collect::<Vec<InvoiceAttr>>(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<gen_invoice::RawInvoiceData> for InvoiceData {
    type Error = VerificationError;

    fn try_from(data: gen_invoice::RawInvoiceData) -> Result<Self, Self::Error> {
        Ok(InvoiceData {
            timestamp: data.timestamp().unpack(),
            payment_hash: bytes_to_u8_array(&data.payment_hash().as_bytes()).into(),
            attrs: data
                .attrs()
                .into_iter()
                .map(|a| a.into())
                .collect::<Vec<Attribute>>(),
        })
    }
}

impl From<Attribute> for InvoiceAttr {
    fn from(attr: Attribute) -> Self {
        let a = match attr {
            Attribute::ExpiryTime(x) => {
                let seconds = x.as_secs();
                let value = ExpiryTime::new_builder().value(seconds.pack()).build();
                InvoiceAttrUnion::ExpiryTime(value)
            }
            Attribute::Description(value) => InvoiceAttrUnion::Description(
                Description::new_builder().value(value.pack()).build(),
            ),
            Attribute::FinalHtlcTimeout(value) => InvoiceAttrUnion::FinalHtlcTimeout(
                FinalHtlcTimeout::new_builder().value(value.pack()).build(),
            ),
            Attribute::FinalHtlcMinimumExpiryDelta(value) => {
                InvoiceAttrUnion::FinalHtlcMinimumExpiryDelta(
                    FinalHtlcMinimumExpiryDelta::new_builder()
                        .value(value.pack())
                        .build(),
                )
            }
            Attribute::FallbackAddr(value) => InvoiceAttrUnion::FallbackAddr(
                FallbackAddr::new_builder().value(value.pack()).build(),
            ),
            Attribute::Feature(value) => InvoiceAttrUnion::Feature(
                Feature::new_builder().value(value.bytes().pack()).build(),
            ),
            Attribute::UdtScript(script) => {
                InvoiceAttrUnion::UdtScript(UdtScript::new_builder().value(script.0).build())
            }
            Attribute::PayeePublicKey(pubkey) => InvoiceAttrUnion::PayeePublicKey(
                PayeePublicKey::new_builder()
                    .value(pubkey.serialize().pack())
                    .build(),
            ),
            Attribute::HashAlgorithm(hash_algorithm) => InvoiceAttrUnion::HashAlgorithm(
                gen_invoice::HashAlgorithm::new_builder()
                    .value(Byte::new(hash_algorithm as u8))
                    .build(),
            ),
            Attribute::PaymentSecret(payment_secret) => InvoiceAttrUnion::PaymentSecret(
                PaymentSecret::new_builder()
                    .value(payment_secret.into())
                    .build(),
            ),
        };
        InvoiceAttr::new_builder().set(a).build()
    }
}

impl From<InvoiceAttr> for Attribute {
    fn from(attr: InvoiceAttr) -> Self {
        match attr.to_enum() {
            InvoiceAttrUnion::Description(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::Description(
                    String::from_utf8(value).expect("decode utf8 string from bytes"),
                )
            }
            InvoiceAttrUnion::ExpiryTime(x) => {
                let seconds: u64 = x.value().unpack();
                Attribute::ExpiryTime(Duration::from_secs(seconds))
            }

            InvoiceAttrUnion::FinalHtlcTimeout(x) => {
                // This attribute is deprecated since v0.6.0, but we still keep it in molecule for consistency
                Attribute::FinalHtlcTimeout(x.value().unpack())
            }
            InvoiceAttrUnion::FinalHtlcMinimumExpiryDelta(x) => {
                Attribute::FinalHtlcMinimumExpiryDelta(x.value().unpack())
            }
            InvoiceAttrUnion::FallbackAddr(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::FallbackAddr(
                    String::from_utf8(value).expect("decode utf8 string from bytes"),
                )
            }
            InvoiceAttrUnion::Feature(x) => {
                Attribute::Feature(FeatureVector::from(x.value().unpack()))
            }
            InvoiceAttrUnion::UdtScript(x) => Attribute::UdtScript(CkbScript(x.value())),
            InvoiceAttrUnion::PayeePublicKey(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::PayeePublicKey(
                    PublicKey::from_slice(&value).expect("Public key from slice"),
                )
            }
            InvoiceAttrUnion::HashAlgorithm(x) => {
                let value = x.value();
                // Consider unknown algorithm as the default one.
                let hash_algorithm = value.try_into().unwrap_or_default();
                Attribute::HashAlgorithm(hash_algorithm)
            }
            InvoiceAttrUnion::PaymentSecret(x) => Attribute::PaymentSecret(x.value().into()),
        }
    }
}

impl From<anyhow::Error> for InvoiceError {
    fn from(_err: anyhow::Error) -> Self {
        InvoiceError::InvalidSignature
    }
}

impl TryFrom<gen_invoice::RawCkbInvoice> for CkbInvoice {
    type Error = InvoiceError;

    fn try_from(invoice: gen_invoice::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(CkbInvoice {
            currency: (u8::from(invoice.currency()))
                .try_into()
                .map_err(|e: UnknownCurrencyError| InvoiceError::UnknownCurrency(e.0))?,
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            signature: invoice.signature().to_opt().map(|x| {
                InvoiceSignature::from_base32_checked(
                    &x.as_bytes()
                        .into_iter()
                        .map(|x| u5::try_from_u8(x).expect("u5 from u8"))
                        .collect::<Vec<u5>>(),
                )
                .expect("signature must be present")
            }),
            data: InvoiceData::try_from(invoice.data()).map_err(InvoiceError::MoleculeError)?,
        })
    }
}

impl From<CkbInvoice> for gen_invoice::RawCkbInvoice {
    fn from(invoice: CkbInvoice) -> Self {
        gen_invoice::RawCkbInvoiceBuilder::default()
            .currency((invoice.currency as u8).into())
            .amount(
                gen_invoice::AmountOpt::new_builder()
                    .set(invoice.amount.map(|x| x.pack()))
                    .build(),
            )
            .signature(
                gen_invoice::SignatureOpt::new_builder()
                    .set({
                        invoice.signature.map(|x| {
                            let bytes: [Byte; SIGNATURE_U5_SIZE] = x
                                .to_base32()
                                .iter()
                                .map(|x| u8_to_byte(x.to_u8()))
                                .collect::<Vec<_>>()
                                .as_slice()
                                .try_into()
                                .expect("[Byte; 104] from [Byte] slice");
                            gen_invoice::Signature::new_builder().set(bytes).build()
                        })
                    })
                    .build(),
            )
            .data(invoice.data.into())
            .build()
    }
}
