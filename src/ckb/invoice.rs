use ckb_types::packed::{BytesOpt, BytesOptBuilder};
use molecule::prelude::{Builder, Entity};

use super::gen::pcn::{self as molecule_pcn};
use crate::ckb::gen::pcn::*;

use core::time::Duration;

use serde::{Deserialize, Serialize};
use serde_with::base64::Base64;
use serde_with::serde_as;
use thiserror::Error;

use ckb_types::{
    packed::{Byte32, BytesVec, Script, Transaction},
    prelude::{Pack, Unpack},
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Molecule error: {0}")]
    Molecule(#[from] molecule::error::VerificationError),
}

#[derive(Debug, Clone)]
pub enum Currency {
    Ckb,
    CkbTestNet,
}

impl From<u8> for Currency {
    fn from(byte: u8) -> Self {
        match byte {
            1 => Self::Ckb,
            2 => Self::CkbTestNet,
            _ => panic!("Invalid value for Currency"),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Clone, Copy, Hash, Ord, PartialOrd)]
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
            1 => Self::Milli,
            2 => Self::Micro,
            3 => Self::Nano,
            4 => Self::Pico,
            _ => panic!("Invalid value for SiPrefix"),
        }
    }
}

pub struct InvoiceBuilder {
    currency: Currency,
    amount: Option<u64>,
    prefix: Option<SiPrefix>,
    description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RawCkbInvoice {
    pub currency: Currency,
    pub amount: Option<u64>,
    pub prefix: Option<SiPrefix>,
    //pub payment_hash: Option<Bytes>,
    pub description: Option<String>,
    pub expiry_time: Option<Duration>,
}

impl From<RawCkbInvoice> for molecule_pcn::RawCkbInvoice {
    fn from(invoice: RawCkbInvoice) -> Self {
        molecule_pcn::RawCkbInvoiceBuilder::default()
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
            //.payment_hash(invoice.payment_hash)
            .description(
                BytesOpt::new_builder()
                    .set(
                        invoice
                            .description
                            .map(|x| molecule::bytes::Bytes::from(x.into_bytes()).pack()),
                    )
                    .build(),
            )
            .expiry_time(
                ExpiryTimeOpt::new_builder()
                    .set(invoice.expiry_time.map(|x| {
                        let seconds = x.as_secs();
                        let nanos = x.subsec_nanos() as u64;
                        molecule_pcn::Duration::new_builder()
                            .seconds(seconds.pack())
                            .nanos(nanos.pack())
                            .build()
                    }))
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_pcn::RawCkbInvoice> for RawCkbInvoice {
    type Error = Error;

    fn try_from(invoice: molecule_pcn::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(RawCkbInvoice {
            currency: (u8::from(invoice.currency())).into(),
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            prefix: invoice
                .prefix()
                .to_opt()
                .map(|x| u8::from(x))
                .map(|x| x.into()),
            //payment_hash: invoice.payment_hash().to_opt().map(|x| x.unpack()),
            description: invoice.description().to_opt().map(|x| {
                let v: Vec<u8> = x.unpack();
                String::from_utf8(v.to_vec()).unwrap()
            }),
            expiry_time: invoice.expiry_time().to_opt().map(|x| {
                let seconds: u64 = x.seconds().unpack();
                let nanos: u64 = x.nanos().unpack();
                core::time::Duration::from_secs(seconds)
                    .saturating_add(core::time::Duration::from_nanos(nanos))
            }),
        })
    }
}

impl InvoiceBuilder {
    pub fn new() -> Self {
        Self {
            currency: Currency::Ckb,
            amount: None,
            prefix: None,
            description: None,
        }
    }

    pub fn currency(mut self, currency: Currency) -> Self {
        self.currency = currency;
        self
    }

    pub fn amount(mut self, amount: u64) -> Self {
        self.amount = Some(amount);
        self
    }

    pub fn prefix(mut self, prefix: SiPrefix) -> Self {
        self.prefix = Some(prefix);
        self
    }

    pub fn description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    // pub fn build(self) -> RawCkbInvoice {
    //     let desc = self.description.unwrap().as_bytes();
    //     let desc_data: ckb_types::packed::Bytes = ckb_types::packed::Bytes::new_builder()
    //         .set(desc.to_vec())
    //         .build();
    //     let mut data = RawDataPartBuilder::default()
    //         .description(BytesOptBuilder::default().set(Some(desc_data)).build())
    //         .build();
    //     // let mut invoice = RawCkbInvoiceBuilder::default();
    //     // let data = RawCkbInvoiceData::default();
    //     // let mut expiry_time = RawCkbInvoiceExpiryTime::default();
    //     // expiry_time.set_duration(Duration::from_secs(0).into());
    //     // data.set_expiry_time(expiry_time);
    //     // invoice.set_data(data);
    //     // invoice
    // }
}

// write test for invoices
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_invoice() {
        // let invoice = RawCkbInvoiceBuilder::default().build();
        // let data = invoice.data();
        // let duration = data.expiry_time().to_opt();
        // assert_eq!(duration.is_none(), true);
        // eprintln!("invoice: {}", invoice.data());
        // eprintln!("invoice: {}", data.expiry_time());
    }
}
