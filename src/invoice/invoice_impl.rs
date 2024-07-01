use super::errors::VerificationError;
use super::utils::*;
use crate::ckb::gen::invoice::{self as gen_invoice, *};
use crate::ckb::hash_algorithm::HashAlgorithm;
use crate::ckb::serde_utils::EntityHex;
use crate::ckb::serde_utils::U128Hex;
use crate::ckb::types::Hash256;
use crate::invoice::InvoiceError;
use bech32::{encode, u5, FromBase32, ToBase32, Variant, WriteBase32};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::{
    key::Secp256k1,
    secp256k1::{
        self,
        ecdsa::{RecoverableSignature, RecoveryId},
        Message, PublicKey,
    },
};
use ckb_types::{
    packed::{Byte, Script},
    prelude::{Pack, Unpack},
};
use core::time::Duration;
use molecule::prelude::{Builder, Entity};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::{cmp::Ordering, str::FromStr};

const SIGNATURE_U5_SIZE: usize = 104;

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
    FinalHtlcMinimumCltvExpiry(u64),
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
        let compressed = ar_encompress(invoice_data.as_slice()).unwrap();
        let mut base32 = Vec::with_capacity(compressed.len());
        compressed.write_base32(&mut base32).unwrap();
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

    /// Checks if the signature is valid for the included payee public key or if none exists if it's
    /// valid for the recovered signature (which should always be true?).
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

        let hash = Message::from_slice(&self.hash()[..])
            .expect("Hash is 32 bytes long, same as MESSAGE_SIZE");

        let secp_context = Secp256k1::new();
        let verification_result =
            secp_context.verify_ecdsa(&hash, &signature.0.to_standard(), pub_key);
        match verification_result {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    fn update_signature<F>(&mut self, sign_function: F) -> Result<(), InvoiceError>
    where
        F: FnOnce(&Message) -> RecoverableSignature,
    {
        let hash = self.hash();
        let message = Message::from_slice(&hash).unwrap();
        let signature = sign_function(&message);
        self.signature = Some(InvoiceSignature(signature));
        self.check_signature()?;
        Ok(())
    }

    /// Recovers the public key used for signing the invoice from the recoverable signature.
    pub fn recover_payee_pub_key(&self) -> Result<PublicKey, secp256k1::Error> {
        let hash = Message::from_slice(&self.hash()[..])
            .expect("Hash is 32 bytes long, same as MESSAGE_SIZE");

        let res = secp256k1::Secp256k1::new()
            .recover_ecdsa(&hash, &self.signature.as_ref().unwrap().0)
            .unwrap();
        Ok(res)
    }

    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }

    pub fn payment_hash(&self) -> &Hash256 {
        &self.data.payment_hash
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
    attr_getter!(final_htlc_timeout, FinalHtlcTimeout, u64);
    attr_getter!(
        final_htlc_minimum_cltv_expiry,
        FinalHtlcMinimumCltvExpiry,
        u64
    );
    attr_getter!(fallback_address, FallbackAddr, String);
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
                .map(|x| u5::try_from_u8(*x).unwrap())
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
    fn from_base32(signature: &[u5]) -> Result<Self, InvoiceError> {
        if signature.len() != SIGNATURE_U5_SIZE {
            return Err(InvoiceError::InvalidSliceLength(
                "InvoiceSignature::from_base32()".into(),
            ));
        }
        let recoverable_signature_bytes = Vec::<u8>::from_base32(signature).unwrap();
        let signature = &recoverable_signature_bytes[0..64];
        let recovery_id = RecoveryId::from_i32(recoverable_signature_bytes[64] as i32).unwrap();

        Ok(InvoiceSignature(
            RecoverableSignature::from_compact(signature, recovery_id).unwrap(),
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
            u5::try_from_u8(if self.signature.is_some() { 1 } else { 0 }).unwrap(),
        );
        if let Some(signature) = &self.signature {
            data.extend_from_slice(&signature.to_base32());
        }
        encode(&hrp, data, Variant::Bech32m).unwrap()
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
        let data_part = ar_decompress(&data_part).unwrap();
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
            data: invoice_data.try_into().unwrap(),
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
            Attribute::FinalHtlcMinimumCltvExpiry(value) => {
                InvoiceAttrUnion::FinalHtlcMinimumCltvExpiry(
                    FinalHtlcMinimumCltvExpiry::new_builder()
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
                Attribute::Description(String::from_utf8(value).unwrap())
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
            InvoiceAttrUnion::FinalHtlcMinimumCltvExpiry(x) => {
                Attribute::FinalHtlcMinimumCltvExpiry(x.value().unpack())
            }
            InvoiceAttrUnion::FallbackAddr(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::FallbackAddr(String::from_utf8(value).unwrap())
            }
            InvoiceAttrUnion::Feature(x) => Attribute::Feature(x.value().unpack()),
            InvoiceAttrUnion::UdtScript(x) => Attribute::UdtScript(CkbScript(x.value())),
            InvoiceAttrUnion::PayeePublicKey(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::PayeePublicKey(PublicKey::from_slice(&value).unwrap())
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

    fn add_attr(mut self, attr: Attribute) -> Self {
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
    attr_setter!(final_cltv, FinalHtlcMinimumCltvExpiry, u64);

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

        self.check_duplicated_attrs()?;
        let timestamp = std::time::UNIX_EPOCH.elapsed().unwrap().as_millis();
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

    fn check_duplicated_attrs(&self) -> Result<(), InvoiceError> {
        // check is there any duplicate attribute key set
        for (i, attr) in self.attrs.iter().enumerate() {
            for other in self.attrs.iter().skip(i + 1) {
                if std::mem::discriminant(attr) == std::mem::discriminant(other) {
                    return Err(InvoiceError::DuplicatedAttributeKey(format!("{:?}", attr)));
                }
            }
        }
        Ok(())
    }
}

impl TryFrom<gen_invoice::RawCkbInvoice> for CkbInvoice {
    type Error = InvoiceError;

    fn try_from(invoice: gen_invoice::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(CkbInvoice {
            currency: (u8::from(invoice.currency())).try_into().unwrap(),
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            signature: invoice.signature().to_opt().map(|x| {
                InvoiceSignature::from_base32(
                    &x.as_bytes()
                        .into_iter()
                        .map(|x| u5::try_from_u8(x).unwrap())
                        .collect::<Vec<u5>>(),
                )
                .unwrap()
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
                                .unwrap();
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
                    .set(u8_slice_to_bytes(data.payment_hash.as_ref()).unwrap())
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        key::{KeyPair, Secp256k1},
        secp256k1::SecretKey,
    };
    use ckb_hash::blake2b_256;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn gen_rand_public_key() -> PublicKey {
        let secp = Secp256k1::new();
        let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());
        PublicKey::from_keypair(&key_pair)
    }

    fn gen_rand_private_key() -> SecretKey {
        let secp = Secp256k1::new();
        let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());
        SecretKey::from_keypair(&key_pair)
    }

    fn gen_rand_keypair() -> (PublicKey, SecretKey) {
        let secp = Secp256k1::new();
        let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());
        (
            PublicKey::from_keypair(&key_pair),
            SecretKey::from_keypair(&key_pair),
        )
    }

    fn mock_invoice() -> CkbInvoice {
        let (public_key, private_key) = gen_rand_keypair();
        let mut invoice = CkbInvoice {
            currency: Currency::Fibb,
            amount: Some(1280),
            signature: None,
            data: InvoiceData {
                payment_hash: rand_sha256_hash(),
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
                attrs: vec![
                    Attribute::FinalHtlcTimeout(5),
                    Attribute::FinalHtlcMinimumCltvExpiry(12),
                    Attribute::Description("description".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(1024)),
                    Attribute::FallbackAddr("address".to_string()),
                    Attribute::UdtScript(CkbScript(Script::default())),
                    Attribute::PayeePublicKey(public_key),
                ],
            },
        };
        invoice
            .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();
        invoice
    }

    #[test]
    fn test_signature() {
        let private_key = gen_rand_private_key();
        let signature = Secp256k1::new()
            .sign_ecdsa_recoverable(&Message::from_slice(&[0u8; 32]).unwrap(), &private_key);
        let signature = InvoiceSignature(signature);
        let base32 = signature.to_base32();
        assert_eq!(base32.len(), SIGNATURE_U5_SIZE);

        let decoded_signature = InvoiceSignature::from_base32(&base32).unwrap();
        assert_eq!(decoded_signature, signature);
    }

    #[test]
    fn test_ckb_invoice() {
        let ckb_invoice = mock_invoice();
        let ckb_invoice_clone = ckb_invoice.clone();
        let raw_invoice: RawCkbInvoice = ckb_invoice.into();
        let decoded_invoice: CkbInvoice = raw_invoice.try_into().unwrap();
        assert_eq!(decoded_invoice, ckb_invoice_clone);
        let address = ckb_invoice_clone.to_string();
        assert!(address.starts_with("fibb1280"));
    }

    #[test]
    fn test_invoice_bc32m() {
        let invoice = mock_invoice();
        assert!(invoice.is_signed());
        assert_eq!(invoice.check_signature(), Ok(()));

        let address = invoice.to_string();
        assert!(address.starts_with("fibb1280"));

        let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
        assert_eq!(decoded_invoice, invoice);
        assert!(decoded_invoice.is_signed());
        assert_eq!(decoded_invoice.amount(), Some(1280));
    }

    #[test]
    fn test_invoice_from_str_err() {
        let invoice = mock_invoice();

        let address = invoice.to_string();
        assert!(address.starts_with("fibb1280"));

        let mut wrong = address.clone();
        wrong.push('1');
        let decoded_invoice = wrong.parse::<CkbInvoice>();
        assert_eq!(
            decoded_invoice.err(),
            Some(InvoiceError::Bech32Error(bech32::Error::InvalidLength))
        );

        let mut wrong = address.clone();
        // modify the values of wrong
        wrong.replace_range(10..12, "hi");
        let decoded_invoice = wrong.parse::<CkbInvoice>();
        assert_eq!(
            decoded_invoice.err(),
            Some(InvoiceError::Bech32Error(bech32::Error::InvalidChar('i')))
        );

        let mut wrong = address;
        // modify the values of wrong
        wrong.replace_range(10..12, "aa");
        let decoded_invoice = wrong.parse::<CkbInvoice>();
        assert_eq!(
            decoded_invoice.err(),
            Some(InvoiceError::Bech32Error(bech32::Error::InvalidChecksum))
        );

        wrong = wrong.replace("1280", "1281");
        let decoded_invoice = wrong.parse::<CkbInvoice>();
        assert_eq!(
            decoded_invoice.err(),
            Some(InvoiceError::Bech32Error(bech32::Error::InvalidChecksum))
        );
    }

    #[test]
    fn test_invoice_bc32m_not_same() {
        let private_key = gen_rand_private_key();
        let signature = Secp256k1::new()
            .sign_ecdsa_recoverable(&Message::from_slice(&[0u8; 32]).unwrap(), &private_key);
        let invoice = CkbInvoice {
            currency: Currency::Fibb,
            amount: Some(1280),
            signature: Some(InvoiceSignature(signature)),
            data: InvoiceData {
                payment_hash: [0u8; 32].into(),
                timestamp: 0,
                attrs: vec![
                    Attribute::FinalHtlcTimeout(5),
                    Attribute::FinalHtlcMinimumCltvExpiry(12),
                    Attribute::Description("description hello".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(1024)),
                    Attribute::FallbackAddr("address".to_string()),
                ],
            },
        };

        let address = invoice.to_string();
        let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
        assert_eq!(decoded_invoice, invoice);

        let mock_invoice = mock_invoice();
        let mock_address = mock_invoice.to_string();
        assert_ne!(mock_address, address);
    }

    #[test]
    fn test_compress() {
        let input = "hrp1gyqsqqq5qqqqq9gqqqqp6qqqqq0qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq2qqqqqqqqqqqyvqsqqqsqqqqqvqqqqq8";
        let bytes = input.as_bytes();
        let compressed = ar_encompress(input.as_bytes()).unwrap();

        let decompressed = ar_decompress(&compressed).unwrap();
        let decompressed_str = std::str::from_utf8(&decompressed).unwrap();
        assert_eq!(input, decompressed_str);
        assert!(compressed.len() < bytes.len());
    }

    #[test]
    fn test_invoice_builder() {
        let gen_payment_hash = rand_sha256_hash();
        let (public_key, private_key) = gen_rand_keypair();

        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(gen_payment_hash)
            .fallback_address("address".to_string())
            .expiry_time(Duration::from_secs(1024))
            .payee_pub_key(public_key)
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
            .add_attr(Attribute::Description("description".to_string()))
            .add_attr(Attribute::UdtScript(CkbScript(Script::default())))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();

        let address = invoice.to_string();

        assert_eq!(invoice, address.parse::<CkbInvoice>().unwrap());

        assert_eq!(invoice.currency, Currency::Fibb);
        assert_eq!(invoice.amount, Some(1280));
        assert_eq!(invoice.payment_hash(), &gen_payment_hash);
        assert_eq!(invoice.data.attrs.len(), 7);
    }

    #[test]
    fn test_invoice_signature_check() {
        let gen_payment_hash = rand_sha256_hash();
        let (_, private_key) = gen_rand_keypair();
        let public_key = gen_rand_public_key();

        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(gen_payment_hash)
            .fallback_address("address".to_string())
            .expiry_time(Duration::from_secs(1024))
            .payee_pub_key(public_key)
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
            .add_attr(Attribute::Description("description".to_string()))
            .add_attr(Attribute::UdtScript(CkbScript(Script::default())))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(invoice.err(), Some(InvoiceError::InvalidSignature));
    }

    #[test]
    fn test_invoice_builder_duplicated_attr() {
        let gen_payment_hash = rand_sha256_hash();
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(gen_payment_hash)
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcTimeout(6))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(
            invoice.err(),
            Some(InvoiceError::DuplicatedAttributeKey(format!(
                "{:?}",
                Attribute::FinalHtlcTimeout(5)
            )))
        );
    }

    #[test]
    fn test_invoice_builder_missing() {
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_preimage(rand_sha256_hash())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(invoice.err(), None);

        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(rand_sha256_hash())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(invoice.err(), None);
    }

    #[test]
    fn test_invoice_builder_preimage() {
        let preimage = rand_sha256_hash();
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_preimage(preimage)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();
        let clone_invoice = invoice.clone();
        assert_eq!(hex::encode(invoice.payment_hash()).len(), 64);

        let raw_invoice: RawCkbInvoice = invoice.into();
        let decoded_invoice: CkbInvoice = raw_invoice.try_into().unwrap();
        assert_eq!(decoded_invoice, clone_invoice);
    }

    #[test]
    fn test_invoice_builder_both_payment_hash_preimage() {
        let preimage = rand_sha256_hash();
        let payment_hash = rand_sha256_hash();
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(payment_hash)
            .payment_preimage(preimage)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(
            invoice.err(),
            Some(InvoiceError::BothPaymenthashAndPreimage)
        );
    }

    #[test]
    fn test_invoice_serialize() {
        let invoice = mock_invoice();
        let res = serde_json::to_string(&invoice);
        assert!(res.is_ok());
        let decoded = serde_json::from_str::<CkbInvoice>(&res.unwrap()).unwrap();
        assert_eq!(decoded, invoice);
    }

    #[test]
    fn test_invoice_timestamp() {
        let payment_hash = rand_sha256_hash();
        let private_key = gen_rand_private_key();
        let invoice1 = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(payment_hash)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();

        let invoice2 = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(payment_hash)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();

        assert_ne!(invoice1.data.timestamp, invoice2.data.timestamp);
        assert_ne!(invoice1.to_string(), invoice2.to_string());
    }

    #[test]
    fn test_invoice_gen_payment_hash() {
        let private_key = gen_rand_private_key();
        let payment_preimage = rand_sha256_hash();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_preimage(payment_preimage)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();
        let payment_hash = invoice.payment_hash();
        let expected_hash: Hash256 = blake2b_256(payment_preimage.as_ref()).into();
        assert_eq!(expected_hash, *payment_hash);
    }

    #[test]
    fn test_invoice_rand_payment_hash() {
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));
        assert!(invoice.is_ok());
    }

    #[test]
    fn test_invoice_udt_script() {
        let script = Script::default();
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new(Currency::Fibb)
            .amount(Some(1280))
            .payment_hash(rand_sha256_hash())
            .udt_type_script(script.clone())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();
        assert_eq!(invoice.udt_type_script().unwrap(), &script);

        let res = serde_json::to_string(&invoice);
        assert!(res.is_ok());
        let decoded = serde_json::from_str::<CkbInvoice>(&res.unwrap()).unwrap();
        assert_eq!(decoded, invoice);
    }
}
