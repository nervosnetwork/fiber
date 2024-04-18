use super::gen::{
    invoice as molecule_invoice,
    invoice::{RawCkbInvoice, RawCkbInvoiceBuilder},
};
use crate::ckb::gen::invoice::*;
use crate::ckb::utils::{ar_decode, ar_encode};
use bech32::{encode, FromBase32, ToBase32, Variant};
use ckb_types::prelude::{Pack, Unpack};
use core::time::Duration;
use molecule::prelude::{Builder, Entity};
use nom::{branch::alt, combinator::opt};
use nom::{
    bytes::{complete::take_while1, streaming::tag},
    IResult,
};
use serde::{Deserialize, Serialize};
use std::{num::ParseIntError, str::FromStr};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Molecule error: {0}")]
    Molecule(#[from] molecule::error::VerificationError),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum Currency {
    Ckb,
    CkbTestNet,
}

impl From<u8> for Currency {
    fn from(byte: u8) -> Self {
        match byte {
            0 => Self::Ckb,
            1 => Self::CkbTestNet,
            _ => panic!("Invalid value for Currency"),
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
    type Err = InvoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ckb" => Ok(Self::Ckb),
            "ckt" => Ok(Self::CkbTestNet),
            _ => Err(InvoiceParseError::UnknownCurrency),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Clone, Copy, Hash, Ord, PartialOrd, Serialize, Deserialize)]
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
    type Err = InvoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "m" => Ok(Self::Milli),
            "u" => Ok(Self::Micro),
            "k" => Ok(Self::Kilo),
            _ => Err(InvoiceParseError::UnknownSiPrefix),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum Attribute {
    FinalHtlcTimeout(u64),
    FinalHtlcMinimumCltvExpiry(u64),
    ExpiryTime(Duration),
    Description(String),
    FallbackAddr(String),
    Feature(u64),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InvoiceData {
    pub payment_hash: [u8; 32],
    pub signature: [u8; 64],
    pub attrs: Vec<Attribute>,
}
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CkbInvoice {
    pub currency: Currency,
    pub amount: Option<u64>,
    pub prefix: Option<SiPrefix>,
    pub data: InvoiceData,
}

impl ToString for CkbInvoice {
    fn to_string(&self) -> String {
        let hrp = format!(
            "ln{}{}{}",
            self.currency.to_string(),
            self.amount
                .map_or_else(|| "".to_string(), |x| x.to_string()),
            self.prefix
                .map_or_else(|| "".to_string(), |x| x.to_string()),
        );
        let invoice_data = RawInvoiceData::from(self.clone().data);
        let compressd = ar_encode(invoice_data.as_slice()).unwrap();
        let mut base32 = Vec::with_capacity(compressd.len());
        compressd.write_base32(&mut base32).unwrap();
        encode(&hrp, base32, Variant::Bech32m).unwrap()
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum InvoiceParseError {
    Bech32Error(bech32::Error),
    ParseAmountError(ParseIntError),
    //MalformedSignature(secp256k1::Error),
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
    /// Not an error, but used internally to signal that a part of the invoice should be ignored
    /// according to BOLT11
    Skip,
}

impl FromStr for CkbInvoice {
    type Err = InvoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hrp, data, var) = bech32::decode(s).unwrap();

        if var == bech32::Variant::Bech32 {
            return Err(InvoiceParseError::Bech32Error(
                bech32::Error::InvalidChecksum,
            ));
        }

        if data.len() < 104 {
            return Err(InvoiceParseError::TooShortDataPart);
        }
        let (currency, amount, prefix) = parse_hrp(&hrp)?;
        let data = Vec::<u8>::from_base32(&data).unwrap();
        let decoded_data = ar_decode(&data).unwrap();
        let invoice_data = RawInvoiceData::from_slice(&decoded_data).unwrap();
        let invoice = CkbInvoice {
            currency,
            amount,
            prefix,
            data: invoice_data.try_into().unwrap(),
        };
        Ok(invoice)
    }
}

fn nom_scan_hrp(input: &str) -> IResult<&str, (&str, Option<&str>, Option<&str>)> {
    let (input, _) = tag("ln")(input)?;
    let (input, currency) = alt((tag("ckb"), tag("ckt")))(input)?;
    let (input, amount) = opt(take_while1(|c: char| c.is_numeric()))(input)?;
    let (input, si) = opt(take_while1(|c: char| ['m', 'u', 'k'].contains(&c)))(input)?;
    Ok((input, (currency, amount, si)))
}

fn parse_hrp(input: &str) -> Result<(Currency, Option<u64>, Option<SiPrefix>), InvoiceParseError> {
    match nom_scan_hrp(input) {
        Ok((left, (currency, amount, si_prefix))) => {
            if !left.is_empty() {
                return Err(InvoiceParseError::MalformedHRP);
            }
            let currency =
                Currency::from_str(currency).map_err(|_| InvoiceParseError::UnknownCurrency)?;
            let amount = amount
                .map(|x| {
                    x.parse()
                        .map_err(|err| InvoiceParseError::ParseAmountError(err))
                })
                .transpose()?;
            let si_prefix = si_prefix
                .map(|x| SiPrefix::from_str(x).map_err(|_| InvoiceParseError::UnknownSiPrefix))
                .transpose()?;
            Ok((currency, amount, si_prefix))
        }
        Err(_) => Err(InvoiceParseError::MalformedHRP),
    }
}

impl From<Attribute> for InvoiceAttr {
    fn from(attr: Attribute) -> Self {
        let a = match attr {
            Attribute::ExpiryTime(x) => {
                let seconds = x.as_secs();
                let nanos = x.subsec_nanos() as u64;
                let value = molecule_invoice::Duration::new_builder()
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
        }
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
            .data(InvoiceData::from(invoice.data).into())
            .build()
    }
}

impl TryFrom<molecule_invoice::RawCkbInvoice> for CkbInvoice {
    type Error = Error;

    fn try_from(invoice: molecule_invoice::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(CkbInvoice {
            currency: (u8::from(invoice.currency())).into(),
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            prefix: invoice.prefix().to_opt().map(|x| u8::from(x).into()),
            data: invoice.data().try_into()?,
        })
    }
}

impl From<InvoiceData> for molecule_invoice::RawInvoiceData {
    fn from(data: InvoiceData) -> Self {
        RawInvoiceDataBuilder::default()
            .payment_hash(PaymentHash::from(data.payment_hash))
            .signature(Signature::from(data.signature))
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

impl TryFrom<molecule_invoice::RawInvoiceData> for InvoiceData {
    type Error = Error;

    fn try_from(data: molecule_invoice::RawInvoiceData) -> Result<Self, Self::Error> {
        Ok(InvoiceData {
            payment_hash: data.payment_hash().into(),
            signature: data.signature().into(),
            attrs: data
                .attrs()
                .into_iter()
                .map(|a| a.into())
                .collect::<Vec<Attribute>>(),
        })
    }
}

// write test for invoices
#[cfg(test)]
mod tests {
    use super::*;

    fn mock_invoice() -> CkbInvoice {
        CkbInvoice {
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Kilo),
            data: InvoiceData {
                payment_hash: [0u8; 32],
                signature: [0u8; 64],
                attrs: vec![
                    Attribute::FinalHtlcTimeout(5),
                    Attribute::FinalHtlcMinimumCltvExpiry(12),
                    Attribute::Description("description".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(1024)),
                    Attribute::FallbackAddr("address".to_string()),
                ],
            },
        }
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
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lxckb");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lnckt");
        assert_eq!(res, Ok((Currency::CkbTestNet, None, None)));

        let res = parse_hrp("lnxkt");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lncktt");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lnckt1x24");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lnckt000k");
        assert_eq!(
            res,
            Ok((Currency::CkbTestNet, Some(0), Some(SiPrefix::Kilo)))
        );

        let res =
            parse_hrp("lnckt1024444444444444444444444444444444444444444444444444444444444444");
        assert!(matches!(res, Err(InvoiceParseError::ParseAmountError(_))));

        let res = parse_hrp("lnckt0x");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));
    }

    #[test]
    fn test_ckb_invoice() {
        let ckb_invoice = mock_invoice();
        let ckb_invoice_clone = ckb_invoice.clone();
        let raw_invoice: RawCkbInvoice = ckb_invoice.into();
        let decoded_invoice: CkbInvoice = raw_invoice.try_into().unwrap();
        assert_eq!(decoded_invoice, ckb_invoice_clone);
    }

    #[test]
    fn test_invoice_bc32m() {
        let invoice = mock_invoice();

        let address = invoice.to_string();
        assert!(address.starts_with("lnckb1280k1"));

        let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
        assert_eq!(decoded_invoice, invoice);
    }

    #[test]
    fn test_invoice_bc32m_not_same() {
        let invoice = CkbInvoice {
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Kilo),
            data: InvoiceData {
                payment_hash: [0u8; 32],
                signature: [0u8; 64],
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
        let compressed = ar_encode(input.as_bytes()).unwrap();

        let decompressed = ar_decode(&compressed).unwrap();
        let decompressed_str = std::str::from_utf8(&decompressed).unwrap();
        assert_eq!(input, decompressed_str);
        assert!(compressed.len() < bytes.len());
    }
}
