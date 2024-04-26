use super::utils::*;
use crate::ckb::gen::invoice::{self as gen_invoice, *};
use bech32::{encode, u5, FromBase32, ToBase32, Variant, WriteBase32};
use bitcoin::{
    hashes::{sha256, Hash},
    key::Secp256k1,
    secp256k1,
};

use bitcoin::secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, PublicKey,
};
use ckb_types::{
    packed::{Byte, Script},
    prelude::{Pack, Unpack},
};
use core::time::Duration;
use molecule::prelude::{Builder, Entity};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, num::ParseIntError, str::FromStr};

const SIGNATURE_U5_SIZE: usize = 104;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum Currency {
    Ckb,
    CkbTestNet,
}

impl TryFrom<u8> for Currency {
    type Error = InvoiceError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            0 => Ok(Self::Ckb),
            1 => Ok(Self::CkbTestNet),
            _ => Err(InvoiceError::UnknownCurrency),
        }
    }
}

impl ToString for Currency {
    fn to_string(&self) -> String {
        match self {
            Currency::Ckb => "ckb".to_string(),
            Currency::CkbTestNet => "ckt".to_string(),
        }
    }
}

impl FromStr for Currency {
    type Err = InvoiceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ckb" => Ok(Self::Ckb),
            "ckt" => Ok(Self::CkbTestNet),
            _ => Err(InvoiceError::UnknownCurrency),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Clone, Copy, Ord, PartialOrd, Serialize, Deserialize)]
pub enum SiPrefix {
    /// 10^-3
    Milli,
    /// 10^-6
    Micro,
    /// 10^3
    Kilo,
}

impl ToString for SiPrefix {
    fn to_string(&self) -> String {
        match self {
            SiPrefix::Milli => "m".to_string(),
            SiPrefix::Micro => "u".to_string(),
            SiPrefix::Kilo => "k".to_string(),
        }
    }
}

impl From<u8> for SiPrefix {
    fn from(byte: u8) -> Self {
        match byte {
            0 => Self::Milli,
            1 => Self::Micro,
            2 => Self::Kilo,
            _ => panic!("Invalid value for SiPrefix"),
        }
    }
}

impl FromStr for SiPrefix {
    type Err = InvoiceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "m" => Ok(Self::Milli),
            "u" => Ok(Self::Micro),
            "k" => Ok(Self::Kilo),
            _ => Err(InvoiceError::UnknownSiPrefix),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Attribute {
    FinalHtlcTimeout(u64),
    FinalHtlcMinimumCltvExpiry(u64),
    ExpiryTime(Duration),
    Description(String),
    FallbackAddr(String),
    UdtScript(Script),
    PayeePublicKey(PublicKey),
    Feature(u64),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InvoiceData {
    pub payment_hash: [u8; 32],
    pub payment_secret: [u8; 32],
    pub attrs: Vec<Attribute>,
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice.
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using [`CkbInvoiceBuilder`]
///  2. using `str::parse::<CkbInvoice>(&str)` (see [`CkbInvoice::from_str`])
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CkbInvoice {
    pub currency: Currency,
    pub amount: Option<u64>,
    pub prefix: Option<SiPrefix>,
    pub signature: Option<InvoiceSignature>,
    pub data: InvoiceData,
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

impl CkbInvoice {
    fn hrp_part(&self) -> String {
        format!(
            "ln{}{}{}",
            self.currency.to_string(),
            self.amount
                .map_or_else(|| "".to_string(), |x| x.to_string()),
            self.prefix
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

    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }

    pub fn hash(&self) -> [u8; 32] {
        let hrp = self.hrp_part();
        let data = self.data_part();
        let preimage = construct_invoice_preimage(hrp.as_bytes(), &data);
        let mut hash: [u8; 32] = Default::default();
        hash.copy_from_slice(&sha256::Hash::hash(&preimage)[..]);
        hash
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

    pub fn payee_pub_key(&self) -> Option<&PublicKey> {
        self.data
            .attrs
            .iter()
            .filter_map(|attr| match attr {
                Attribute::PayeePublicKey(pk) => Some(pk),
                _ => None,
            })
            .next()
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
    ///   hrp: ln{currency}{amount}{prefix}
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
        let (currency, amount, prefix) = parse_hrp(&hrp)?;
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
            prefix,
            signature,
            data: invoice_data.try_into().unwrap(),
        };
        invoice.check_signature()?;
        Ok(invoice)
    }
}

#[derive(Debug)]
pub struct VerificationError(molecule::error::VerificationError);

impl PartialEq for VerificationError {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
impl Eq for VerificationError {}

#[derive(PartialEq, Eq, Debug)]
pub enum InvoiceError {
    Bech32Error(bech32::Error),
    MoleculeError(VerificationError),
    ParseAmountError(ParseIntError),
    BadPrefix,
    UnknownCurrency,
    UnknownSiPrefix,
    MalformedHRP,
    TooShortDataPart,
    UnexpectedEndOfTaggedFields,
    PaddingError,
    IntegerOverflowError,
    InvalidSegWitProgramLength,
    InvalidPubKeyHashLength,
    InvalidScriptHashLength,
    InvalidRecoveryId,
    InvalidSliceLength(String),
    InvalidSignature,
    /// Duplicated attribute key
    DuplicatedAttributeKey(String),
    /// No payment hash
    NoPaymentHash,
    /// No payment secret
    NoPaymentSecret,
    /// An error occurred during signing
    SignError,
    /// according to BOLT11
    Skip,
}

fn parse_hrp(input: &str) -> Result<(Currency, Option<u64>, Option<SiPrefix>), InvoiceError> {
    match nom_scan_hrp(input) {
        Ok((left, (currency, amount, si_prefix))) => {
            if !left.is_empty() {
                return Err(InvoiceError::MalformedHRP);
            }
            let currency =
                Currency::from_str(currency).map_err(|_| InvoiceError::UnknownCurrency)?;
            let amount = amount
                .map(|x| x.parse().map_err(InvoiceError::ParseAmountError))
                .transpose()?;
            let si_prefix = si_prefix
                .map(|x| SiPrefix::from_str(x).map_err(|_| InvoiceError::UnknownSiPrefix))
                .transpose()?;
            Ok((currency, amount, si_prefix))
        }
        Err(_) => Err(InvoiceError::MalformedHRP),
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
                InvoiceAttrUnion::UdtScript(UdtScript::new_builder().value(script).build())
            }
            Attribute::PayeePublicKey(pubkey) => InvoiceAttrUnion::PayeePublicKey(
                PayeePublicKey::new_builder()
                    .value(pubkey.serialize().pack())
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
            InvoiceAttrUnion::UdtScript(x) => Attribute::UdtScript(x.value()),
            InvoiceAttrUnion::PayeePublicKey(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::PayeePublicKey(PublicKey::from_slice(&value).unwrap())
            }
        }
    }
}

pub struct InvoiceBuilder {
    currency: Currency,
    amount: Option<u64>,
    prefix: Option<SiPrefix>,
    payment_hash: Option<[u8; 32]>,
    payment_secret: Option<[u8; 32]>,
    attrs: Vec<Attribute>,
}

impl Default for InvoiceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl InvoiceBuilder {
    pub fn new() -> Self {
        Self {
            currency: Currency::Ckb,
            amount: None,
            prefix: None,
            payment_hash: None,
            payment_secret: None,
            attrs: Vec::new(),
        }
    }

    pub fn currency(mut self, currency: Currency) -> Self {
        self.currency = currency;
        self
    }

    pub fn amount(mut self, amount: Option<u64>) -> Self {
        self.amount = amount;
        self
    }

    pub fn prefix(mut self, prefix: Option<SiPrefix>) -> Self {
        self.prefix = prefix;
        self
    }

    pub fn payment_hash(mut self, payment_hash: [u8; 32]) -> Self {
        self.payment_hash = Some(payment_hash);
        self
    }

    pub fn payment_secret(mut self, payment_secret: [u8; 32]) -> Self {
        self.payment_secret = Some(payment_secret);
        self
    }

    fn add_attr(mut self, attr: Attribute) -> Self {
        self.attrs.push(attr);
        self
    }

    /// Sets the payee's public key.
    pub fn payee_pub_key(self, pub_key: PublicKey) -> Self {
        self.add_attr(Attribute::PayeePublicKey(pub_key))
    }

    /// Sets the expiry time
    /// dropping the subsecond part (which is not representable in BOLT 11 invoices).
    pub fn expiry_time(self, expiry_time: Duration) -> Self {
        self.add_attr(Attribute::ExpiryTime(expiry_time))
    }

    /// Adds a fallback address.
    pub fn fallback(self, fallback: String) -> Self {
        self.add_attr(Attribute::FallbackAddr(fallback))
    }

    pub fn build(self) -> Result<CkbInvoice, InvoiceError> {
        self.check_duplicated_attrs()?;
        Ok(CkbInvoice {
            currency: self.currency,
            amount: self.amount,
            prefix: self.prefix,
            signature: None,
            data: InvoiceData {
                payment_hash: self.payment_hash.ok_or(InvoiceError::NoPaymentHash)?,

                payment_secret: self.payment_secret.ok_or(InvoiceError::NoPaymentSecret)?,
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
    type Error = molecule::error::VerificationError;

    fn try_from(invoice: gen_invoice::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(CkbInvoice {
            currency: (u8::from(invoice.currency())).try_into().unwrap(),
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            prefix: invoice.prefix().to_opt().map(|x| u8::from(x).into()),
            signature: invoice.signature().to_opt().map(|x| {
                InvoiceSignature::from_base32(
                    &x.as_bytes()
                        .into_iter()
                        .map(|x| u5::try_from_u8(x).unwrap())
                        .collect::<Vec<u5>>(),
                )
                .unwrap()
            }),
            data: invoice.data().try_into()?,
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
            .prefix(
                SiPrefixOpt::new_builder()
                    .set(invoice.prefix.map(|x| (x as u8).into()))
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
            .payment_hash(
                PaymentHash::new_builder()
                    .set(u8_slice_to_bytes(&data.payment_hash).unwrap())
                    .build(),
            )
            .payment_secret(
                PaymentSecret::new_builder()
                    .set(u8_slice_to_bytes(&data.payment_secret).unwrap())
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
    type Error = molecule::error::VerificationError;

    fn try_from(data: gen_invoice::RawInvoiceData) -> Result<Self, Self::Error> {
        Ok(InvoiceData {
            payment_hash: bytes_to_u8_array(&data.payment_hash().as_bytes()),
            payment_secret: bytes_to_u8_array(&data.payment_secret().as_bytes()),
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
    use bitcoin::{
        key::{KeyPair, Secp256k1},
        secp256k1::SecretKey,
    };

    use super::*;

    fn random_u8_array(num: usize) -> Vec<u8> {
        (0..num).map(|_| rand::random::<u8>()).collect()
    }

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
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Kilo),
            signature: None,
            data: InvoiceData {
                payment_hash: random_u8_array(32).try_into().unwrap(),
                payment_secret: random_u8_array(32).try_into().unwrap(),
                attrs: vec![
                    Attribute::FinalHtlcTimeout(5),
                    Attribute::FinalHtlcMinimumCltvExpiry(12),
                    Attribute::Description("description".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(1024)),
                    Attribute::FallbackAddr("address".to_string()),
                    Attribute::UdtScript(Script::default()),
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
    fn test_parse_hrp() {
        let res = parse_hrp("lnckb1280k");
        assert_eq!(res, Ok((Currency::Ckb, Some(1280), Some(SiPrefix::Kilo))));

        let res = parse_hrp("lnckb");
        assert_eq!(res, Ok((Currency::Ckb, None, None)));

        let res = parse_hrp("lnckt1023");
        assert_eq!(res, Ok((Currency::CkbTestNet, Some(1023), None)));

        let res = parse_hrp("lnckt1023u");
        assert_eq!(
            res,
            Ok((Currency::CkbTestNet, Some(1023), Some(SiPrefix::Micro)))
        );

        let res = parse_hrp("lncktk");
        assert_eq!(res, Ok((Currency::CkbTestNet, None, Some(SiPrefix::Kilo))));

        let res = parse_hrp("xnckb");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));

        let res = parse_hrp("lxckb");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));

        let res = parse_hrp("lnckt");
        assert_eq!(res, Ok((Currency::CkbTestNet, None, None)));

        let res = parse_hrp("lnxkt");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));

        let res = parse_hrp("lncktt");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));

        let res = parse_hrp("lnckt1x24");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));

        let res = parse_hrp("lnckt000k");
        assert_eq!(
            res,
            Ok((Currency::CkbTestNet, Some(0), Some(SiPrefix::Kilo)))
        );

        let res =
            parse_hrp("lnckt1024444444444444444444444444444444444444444444444444444444444444");
        assert!(matches!(res, Err(InvoiceError::ParseAmountError(_))));

        let res = parse_hrp("lnckt0x");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));

        let res = parse_hrp("");
        assert_eq!(res, Err(InvoiceError::MalformedHRP));
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
        assert!(address.starts_with("lnckb1280k1"));
    }

    #[test]
    fn test_invoice_bc32m() {
        let invoice = mock_invoice();
        assert_eq!(invoice.is_signed(), true);
        assert_eq!(invoice.check_signature(), Ok(()));

        let address = invoice.to_string();
        assert!(address.starts_with("lnckb1280k1"));

        let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
        assert_eq!(decoded_invoice, invoice);
        assert_eq!(decoded_invoice.is_signed(), true);
    }

    #[test]
    fn test_invoice_from_str_err() {
        let invoice = mock_invoice();

        let address = invoice.to_string();
        assert!(address.starts_with("lnckb1280k1"));

        let mut wrong = address.clone();
        wrong.push_str("1");
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

        let mut wrong = address.clone();
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
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Kilo),
            signature: Some(InvoiceSignature(signature)),
            data: InvoiceData {
                payment_hash: [0u8; 32],
                payment_secret: [0u8; 32],
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
        let gen_payment_hash = random_u8_array(32).try_into().unwrap();
        let gen_payment_secret = random_u8_array(32).try_into().unwrap();
        let (public_key, private_key) = gen_rand_keypair();

        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(gen_payment_hash)
            .payment_secret(gen_payment_secret)
            .fallback("address".to_string())
            .expiry_time(Duration::from_secs(1024))
            .payee_pub_key(public_key)
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
            .add_attr(Attribute::Description("description".to_string()))
            .add_attr(Attribute::UdtScript(Script::default()))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();

        let address = invoice.to_string();

        assert_eq!(invoice, address.parse::<CkbInvoice>().unwrap());

        assert_eq!(invoice.currency, Currency::Ckb);
        assert_eq!(invoice.amount, Some(1280));
        assert_eq!(invoice.prefix, Some(SiPrefix::Kilo));
        assert_eq!(invoice.data.payment_hash, gen_payment_hash);
        assert_eq!(invoice.data.payment_secret, gen_payment_secret);
        assert_eq!(invoice.data.payment_hash, gen_payment_hash);
        assert_eq!(invoice.data.payment_secret, gen_payment_secret);
        assert_eq!(invoice.data.attrs.len(), 7);
    }

    #[test]
    fn test_invoice_signature_check() {
        let gen_payment_hash = random_u8_array(32).try_into().unwrap();
        let gen_payment_secret = random_u8_array(32).try_into().unwrap();
        let (_, private_key) = gen_rand_keypair();
        let public_key = gen_rand_public_key();

        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(gen_payment_hash)
            .payment_secret(gen_payment_secret)
            .fallback("address".to_string())
            .expiry_time(Duration::from_secs(1024))
            .payee_pub_key(public_key)
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
            .add_attr(Attribute::Description("description".to_string()))
            .add_attr(Attribute::UdtScript(Script::default()))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(invoice.err(), Some(InvoiceError::InvalidSignature));
    }

    #[test]
    fn test_invoice_builder_duplicated_attr() {
        let gen_payment_hash = random_u8_array(32).try_into().unwrap();
        let gen_payment_secret = random_u8_array(32).try_into().unwrap();
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(gen_payment_hash)
            .payment_secret(gen_payment_secret)
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
        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_secret(random_u8_array(32).try_into().unwrap())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(invoice.err(), Some(InvoiceError::NoPaymentHash));

        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(random_u8_array(32).try_into().unwrap())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(invoice.err(), Some(InvoiceError::NoPaymentSecret));
    }
}
