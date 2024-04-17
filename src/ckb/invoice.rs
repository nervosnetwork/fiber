use super::gen::{
    invoice as molecule_invoice,
    invoice::{RawCkbInvoice, RawCkbInvoiceBuilder},
};
use crate::ckb::gen::invoice::*;
use ckb_types::prelude::{Pack, Unpack};
use core::time::Duration;
use molecule::prelude::{Builder, Entity};
use serde::{Deserialize, Serialize};
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

#[derive(Eq, PartialEq, Debug, Clone, Copy, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum SiPrefix {
    /// 10^-3
    Milli,
    /// 10^-6
    Micro,
    /// 10^-9
    Nano,
    /// 10^-12
    Pico,
}

impl From<u8> for SiPrefix {
    fn from(byte: u8) -> Self {
        match byte {
            0 => Self::Milli,
            1 => Self::Micro,
            2 => Self::Nano,
            3 => Self::Pico,
            _ => panic!("Invalid value for SiPrefix"),
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

    use bech32::{decode, encode, FromBase32, ToBase32, Variant};
    fn mock_invoice() -> CkbInvoice {
        CkbInvoice {
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Milli),
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
    fn test_ckb_invoice() {
        let ckb_invoice = mock_invoice();
        let ckb_invoice_backup = ckb_invoice.clone();

        let invoice_copy: RawCkbInvoice = ckb_invoice.into();
        let ckb_invoice_rec: CkbInvoice = invoice_copy.try_into().unwrap();

        assert_eq!(ckb_invoice_rec, ckb_invoice_backup);
    }

    #[test]
    fn test_invoice_bc32m() {
        let invoice = mock_invoice();
        let raw_invoice = RawCkbInvoice::from(invoice.clone());
        let slice = raw_invoice.as_slice();
        let mut base32 = Vec::with_capacity(slice.len());
        slice.write_base32(&mut base32).unwrap();
        let result = encode("hrp", base32, Variant::Bech32).unwrap();
        eprintln!("invoice: {}", result);
        eprintln!("invoice_len: {:?}", result.len());
        let (msg, data, _) = decode(&result).unwrap();
        assert_eq!(msg, "hrp".to_string());
        // convert from base32 to slice
        let data = Vec::<u8>::from_base32(&data).unwrap();
        let slice = RawCkbInvoice::from_slice(&data).unwrap();
        let decoded_invoice: CkbInvoice = slice.try_into().unwrap();
        assert_eq!(decoded_invoice, invoice);
    }
}
