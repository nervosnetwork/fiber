//! Payment-related types.

use crate::gen::fiber as molecule_fiber;
use ckb_types::prelude::{Pack, Unpack};
use molecule::prelude::{Builder, Entity};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================
// Payment status
// ============================================================

/// The status of a payment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    Created,
    Inflight,
    Success,
    Failed,
}

impl PaymentStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, PaymentStatus::Success | PaymentStatus::Failed)
    }
}

// ============================================================
// Payment custom records
// ============================================================

/// 0 ~ 65535 is reserved for endpoint usage, index above 65535 is reserved for internal usage
pub const USER_CUSTOM_RECORDS_MAX_INDEX: u32 = 65535;

/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    pub data: HashMap<u32, Vec<u8>>,
}

// ============================================================
// Conversions with molecule types
// ============================================================

impl From<PaymentCustomRecords> for molecule_fiber::CustomRecords {
    fn from(custom_records: PaymentCustomRecords) -> Self {
        molecule_fiber::CustomRecords::new_builder()
            .data(
                custom_records
                    .data
                    .into_iter()
                    .map(|(key, val)| {
                        molecule_fiber::CustomRecordDataPairBuilder::default()
                            .key(key.pack())
                            .value(val.pack())
                            .build()
                    })
                    .collect(),
            )
            .build()
    }
}

impl From<molecule_fiber::CustomRecords> for PaymentCustomRecords {
    fn from(custom_records: molecule_fiber::CustomRecords) -> Self {
        PaymentCustomRecords {
            data: custom_records
                .data()
                .into_iter()
                .map(|pair| (pair.key().unpack(), pair.value().unpack()))
                .collect(),
        }
    }
}
