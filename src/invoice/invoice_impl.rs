use super::errors::VerificationError;
use super::utils::*;
use crate::fiber::gen::invoice::{self as gen_invoice, *};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::U128Hex;
use crate::fiber::types::Hash256;
use crate::invoice::InvoiceError;
use bech32::{encode, u5, FromBase32, ToBase32, Variant, WriteBase32};
use bitcoin::hashes::{sha256::Hash as Sha256, Hash as _};
use ckb_types::{
    packed::{Byte, Script},
    prelude::{Pack, Unpack},
};
use core::time::Duration;
use molecule::prelude::{Builder, Entity};
use secp256k1::{
    self,
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, PublicKey, Secp256k1,
};
use std::fmt::Display;

use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::{cmp::Ordering, str::FromStr};

pub(crate) const SIGNATURE_U5_SIZE: usize = 104;
pub(crate) const MAX_DESCRIPTION_LENGTH: usize = 639;

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

/// The currency of the invoice, can also used to represent the CKB network chain.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum Currency {
    Fibb,
    Fibt,
    Fibd,
}

impl TryFrom<u8> for Currency {
    type Error = InvoiceError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            0 => Ok(Self::Fibb),
            1 => Ok(Self::Fibt),
            2 => Ok(Self::Fibd),
            _ => Err(InvoiceError::UnknownCurrency(byte.to_string())),
        }
    }
}

impl ToString for Currency {
    fn to_string(&self) -> String {
        match self {
            Currency::Fibb => "fibb".to_string(),
            Currency::Fibt => "fibt".to_string(),
            Currency::Fibd => "fibd".to_string(),
        }
    }
}

impl FromStr for Currency {
    type Err = InvoiceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fibb" => Ok(Self::Fibb),
            "fibt" => Ok(Self::Fibt),
            "fibd" => Ok(Self::Fibd),
            _ => Err(InvoiceError::UnknownCurrency(s.to_string())),
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbScript(#[serde_as(as = "EntityHex")] pub Script);

#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum Attribute {
    FinalHtlcTimeout(u64),
    FinalHtlcMinimumExpiryDelta(u64),
    ExpiryTime(Duration),
    Description(String),
    FallbackAddr(String),
    UdtScript(CkbScript),
    PayeePublicKey(PublicKey),
    HashAlgorithm(HashAlgorithm),
    Feature(u64),
}

#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvoiceData {
    #[serde_as(as = "U128Hex")]
    pub timestamp: u128,
    pub payment_hash: Hash256,
    pub attrs: Vec<Attribute>,
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using [`CkbInvoiceBuilder`]
///  2. using `str::parse::<CkbInvoice>(&str)` (see [`CkbInvoice::from_str`])
///
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbInvoice {
    pub currency: Currency,
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,
    pub signature: Option<InvoiceSignature>,
    pub data: InvoiceData,
}

macro_rules! attr_getter {
    ($name:ident, $attr_name:ident, $attr:ty) => {
        pub fn $name(&self) -> Option<&$attr> {
            self.data
                .attrs
                .iter()
                .filter_map(|attr| match attr {
                    Attribute::$attr_name(val) => Some(val),
                    _ => None,
                })
                .next()
        }
    };
}

impl CkbInvoice {
    fn hrp_part(&self) -> String {
        format!(
            "{}{}",
            self.currency.to_string(),
            self.amount
                .map_or_else(|| "".to_string(), |x| x.to_string()),
        )
    }

    // Use the lostless compression algorithm to compress the invoice data.
    // To make sure the final encoded invoice address is shorter
    fn data_part(&self) -> Vec<u5> {
        let invoice_data = RawInvoiceData::from(self.data.clone());
        let compressed = ar_encompress(invoice_data.as_slice()).expect("compress invoice data");
        let mut base32 = Vec::with_capacity(compressed.len());
        compressed
            .write_base32(&mut base32)
            .expect("encode in base32");
        base32
    }

    fn hash(&self) -> [u8; 32] {
        let hrp = self.hrp_part();
        let data = self.data_part();
        let preimage = construct_invoice_preimage(hrp.as_bytes(), &data);
        let mut hash: [u8; 32] = Default::default();
        hash.copy_from_slice(&Sha256::hash(&preimage).to_byte_array());
        hash
    }

    /// Checks if the signature is valid for the included payee public key
    /// and also check the invoice data is consistent with the signature
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

        let hash = Message::from_digest_slice(&self.hash()[..])
            .expect("Hash is 32 bytes long, same as MESSAGE_SIZE");

        let secp_context = Secp256k1::new();
        let verification_result =
            secp_context.verify_ecdsa(&hash, &signature.0.to_standard(), pub_key);
        match verification_result {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    pub(crate) fn update_signature<F>(&mut self, sign_function: F) -> Result<(), InvoiceError>
    where
        F: FnOnce(&Message) -> RecoverableSignature,
    {
        let hash = self.hash();
        let message = Message::from_digest_slice(&hash).expect("message from digest slice");
        let signature = sign_function(&message);
        self.signature = Some(InvoiceSignature(signature));
        self.check_signature()?;
        Ok(())
    }

    /// Recovers the public key used for signing the invoice from the recoverable signature.
    pub fn recover_payee_pub_key(&self) -> Result<PublicKey, secp256k1::Error> {
        let hash = Message::from_digest_slice(&self.hash()[..])
            .expect("Hash is 32 bytes long, same as MESSAGE_SIZE");

        let res = secp256k1::Secp256k1::new()
            .recover_ecdsa(
                &hash,
                &self
                    .signature
                    .as_ref()
                    .expect("signature must be present")
                    .0,
            )
            .expect("payee pub key recovered");
        Ok(res)
    }

    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }

    pub fn payment_hash(&self) -> &Hash256 {
        &self.data.payment_hash
    }

    pub fn is_expired(&self) -> bool {
        self.expiry_time().map_or(false, |expiry| {
            self.data.timestamp + expiry.as_millis()
                < std::time::UNIX_EPOCH
                    .elapsed()
                    .expect("Duration since unix epoch")
                    .as_millis()
        })
    }

    /// Check that the invoice is signed correctly and that key recovery works
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

    pub fn amount(&self) -> Option<u128> {
        self.amount
    }

    pub fn udt_type_script(&self) -> Option<&Script> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::UdtScript(script) => Some(&script.0),
                _ => None,
            })
            .next()
    }

    attr_getter!(payee_pub_key, PayeePublicKey, PublicKey);
    attr_getter!(expiry_time, ExpiryTime, Duration);
    attr_getter!(description, Description, String);
    attr_getter!(
        final_tlc_minimum_expiry_delta,
        FinalHtlcMinimumExpiryDelta,
        u64
    );
    attr_getter!(fallback_address, FallbackAddr, String);
    attr_getter!(hash_algorithm, HashAlgorithm, HashAlgorithm);
}

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
        let hex = hex::encode(base32);
        hex.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InvoiceSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, <D as serde::Deserializer<'de>>::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let signature_hex: String = String::deserialize(deserializer)?;
        let signature_bytes = hex::decode(signature_hex).map_err(serde::de::Error::custom)?;
        let signature = InvoiceSignature::from_base32(
            &signature_bytes
                .iter()
                .map(|x| u5::try_from_u8(*x).expect("u5 from u8"))
                .collect::<Vec<u5>>(),
        );
        signature.map_err(serde::de::Error::custom)
    }
}

impl ToBase32 for InvoiceSignature {
    fn write_base32<W: WriteBase32>(&self, writer: &mut W) -> Result<(), <W as WriteBase32>::Err> {
        let mut converter = BytesToBase32::new(writer);
        let (recovery_id, signature) = self.0.serialize_compact();
        converter.append(&signature[..])?;
        converter.append_u8(recovery_id.to_i32() as u8)?;
        converter.finalize()
    }
}

impl InvoiceSignature {
    pub(crate) fn from_base32(signature: &[u5]) -> Result<Self, InvoiceError> {
        if signature.len() != SIGNATURE_U5_SIZE {
            return Err(InvoiceError::InvalidSliceLength(
                "InvoiceSignature::from_base32()".into(),
            ));
        }
        let recoverable_signature_bytes =
            Vec::<u8>::from_base32(signature).expect("bytes from base32");
        let signature = &recoverable_signature_bytes[0..64];
        let recovery_id = RecoveryId::from_i32(recoverable_signature_bytes[64] as i32)
            .expect("Recovery ID from i32");

        Ok(InvoiceSignature(
            RecoverableSignature::from_compact(signature, recovery_id)
                .expect("signature from compact"),
        ))
    }
}

impl ToString for CkbInvoice {
    ///   hrp: fib{currency}{amount}{prefix}
    ///   data: compressed(InvoiceData) + signature
    ///   signature: 64 bytes + 1 byte recovery id = Vec<u8>
    ///     if signature is present: bech32m(hrp, 1 + data + signature)
    ///     else if signature is not present: bech32m(hrp, 0 + data)
    fn to_string(&self) -> String {
        let hrp = self.hrp_part();
        let mut data = self.data_part();
        data.insert(
            0,
            u5::try_from_u8(if self.signature.is_some() { 1 } else { 0 }).expect("u5 from u8"),
        );
        if let Some(signature) = &self.signature {
            data.extend_from_slice(&signature.to_base32());
        }
        encode(&hrp, data, Variant::Bech32m).expect("encode invoice using Bech32m")
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
        let invoice_data = RawInvoiceData::from_slice(&data_part)
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

impl From<Attribute> for InvoiceAttr {
    fn from(attr: Attribute) -> Self {
        let a = match attr {
            Attribute::ExpiryTime(x) => {
                let seconds = x.as_secs();
                let nanos = x.subsec_nanos() as u64;
                let value = gen_invoice::Duration::new_builder()
                    .seconds(seconds.pack())
                    .nanos(nanos.pack())
                    .build();
                InvoiceAttrUnion::ExpiryTime(ExpiryTime::new_builder().value(value).build())
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
            Attribute::Feature(value) => {
                InvoiceAttrUnion::Feature(Feature::new_builder().value(value.pack()).build())
            }
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
                let seconds: u64 = x.value().seconds().unpack();
                let nanos: u64 = x.value().nanos().unpack();
                Attribute::ExpiryTime(
                    Duration::from_secs(seconds).saturating_add(Duration::from_nanos(nanos)),
                )
            }
            InvoiceAttrUnion::FinalHtlcTimeout(x) => {
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
            InvoiceAttrUnion::Feature(x) => Attribute::Feature(x.value().unpack()),
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
        }
    }
}

pub struct InvoiceBuilder {
    currency: Currency,
    amount: Option<u128>,
    payment_hash: Option<Hash256>,
    payment_preimage: Option<Hash256>,
    attrs: Vec<Attribute>,
}

impl Default for InvoiceBuilder {
    fn default() -> Self {
        Self::new(Currency::Fibb)
    }
}

macro_rules! attr_setter {
    ($name:ident, $attr:ident, $param:ty) => {
        pub fn $name(self, value: $param) -> Self {
            self.add_attr(Attribute::$attr(value))
        }
    };
}

impl InvoiceBuilder {
    pub fn new(currency: Currency) -> Self {
        Self {
            currency,
            amount: None,
            payment_hash: None,
            payment_preimage: None,
            attrs: Vec::new(),
        }
    }

    pub fn currency(mut self, currency: Currency) -> Self {
        self.currency = currency;
        self
    }

    pub fn amount(mut self, amount: Option<u128>) -> Self {
        self.amount = amount;
        self
    }

    pub fn add_attr(mut self, attr: Attribute) -> Self {
        self.attrs.push(attr);
        self
    }

    pub fn payment_hash(mut self, payment_hash: Hash256) -> Self {
        self.payment_hash = Some(payment_hash);
        self
    }

    pub fn payment_preimage(mut self, payment_preimage: Hash256) -> Self {
        self.payment_preimage = Some(payment_preimage);
        self
    }

    pub fn udt_type_script(self, script: Script) -> Self {
        self.add_attr(Attribute::UdtScript(CkbScript(script)))
    }

    pub fn hash_algorithm(self, algorithm: HashAlgorithm) -> Self {
        self.add_attr(Attribute::HashAlgorithm(algorithm))
    }

    attr_setter!(description, Description, String);
    attr_setter!(payee_pub_key, PayeePublicKey, PublicKey);
    attr_setter!(expiry_time, ExpiryTime, Duration);
    attr_setter!(fallback_address, FallbackAddr, String);
    attr_setter!(final_expiry_delta, FinalHtlcMinimumExpiryDelta, u64);

    pub fn build(self) -> Result<CkbInvoice, InvoiceError> {
        let preimage = self.payment_preimage;

        if self.payment_hash.is_some() && preimage.is_some() {
            return Err(InvoiceError::BothPaymenthashAndPreimage);
        }
        let payment_hash: Hash256 = if let Some(preimage) = preimage {
            let algo = self
                .attrs
                .iter()
                .find_map(|attr| match attr {
                    Attribute::HashAlgorithm(algo) => Some(algo),
                    _ => None,
                })
                .copied()
                .unwrap_or_default();
            algo.hash(preimage.as_ref()).into()
        } else if let Some(payment_hash) = self.payment_hash {
            payment_hash
        } else {
            // generate a random payment hash if not provided
            rand_sha256_hash()
        };

        self.check_attrs_valid()?;
        let timestamp = std::time::UNIX_EPOCH
            .elapsed()
            .expect("Duration since unix epoch")
            .as_millis();
        Ok(CkbInvoice {
            currency: self.currency,
            amount: self.amount,
            signature: None,
            data: InvoiceData {
                timestamp,
                payment_hash,
                attrs: self.attrs,
            },
        })
    }

    pub fn build_with_sign<F>(self, sign_function: F) -> Result<CkbInvoice, InvoiceError>
    where
        F: FnOnce(&Message) -> RecoverableSignature,
    {
        let mut invoice = self.build()?;
        invoice.update_signature(sign_function)?;
        Ok(invoice)
    }

    fn check_attrs_valid(&self) -> Result<(), InvoiceError> {
        // check is there any duplicate attribute key set
        for (i, attr) in self.attrs.iter().enumerate() {
            for other in self.attrs.iter().skip(i + 1) {
                if std::mem::discriminant(attr) == std::mem::discriminant(other) {
                    return Err(InvoiceError::DuplicatedAttributeKey(format!("{:?}", attr)));
                }
            }
        }

        if let Some(len) = self.attrs.iter().find_map(|attr| match attr {
            Attribute::Description(desc) if desc.len() > MAX_DESCRIPTION_LENGTH => Some(desc.len()),
            _ => None,
        }) {
            return Err(InvoiceError::DescriptionTooLong(len));
        }

        Ok(())
    }
}

impl TryFrom<gen_invoice::RawCkbInvoice> for CkbInvoice {
    type Error = InvoiceError;

    fn try_from(invoice: gen_invoice::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(CkbInvoice {
            currency: (u8::from(invoice.currency()))
                .try_into()
                .expect("currency from u8"),
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            signature: invoice.signature().to_opt().map(|x| {
                InvoiceSignature::from_base32(
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

impl From<CkbInvoice> for RawCkbInvoice {
    fn from(invoice: CkbInvoice) -> Self {
        RawCkbInvoiceBuilder::default()
            .currency((invoice.currency as u8).into())
            .amount(
                AmountOpt::new_builder()
                    .set(invoice.amount.map(|x| x.pack()))
                    .build(),
            )
            .signature(
                SignatureOpt::new_builder()
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
                            Signature::new_builder().set(bytes).build()
                        })
                    })
                    .build(),
            )
            .data(invoice.data.into())
            .build()
    }
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
