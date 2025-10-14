use serde::{Deserialize, Serialize};

use crate::fiber::{
    amp::{AmpChildDesc, AmpSecret},
    network::USER_CUSTOM_RECORDS_MAX_INDEX,
    types::Hash256,
    PaymentCustomRecords,
};

#[derive(Eq, PartialEq, Debug)]
/// Bolt04 basic MPP payment data record
pub struct BasicMppPaymentData {
    pub payment_secret: Hash256,
    pub total_amount: u128,
}

impl BasicMppPaymentData {
    // record type for payment data record in bolt04
    // custom records key from 65536 is reserved for internal usage
    pub const CUSTOM_RECORD_KEY: u32 = USER_CUSTOM_RECORDS_MAX_INDEX + 1;

    pub fn new(payment_secret: Hash256, total_amount: u128) -> Self {
        Self {
            payment_secret,
            total_amount,
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.extend_from_slice(self.payment_secret.as_ref());
        vec.extend_from_slice(&self.total_amount.to_le_bytes());
        vec
    }

    pub fn write(&self, custom_records: &mut PaymentCustomRecords) {
        custom_records
            .data
            .insert(Self::CUSTOM_RECORD_KEY, self.to_vec());
    }

    pub fn read(custom_records: &PaymentCustomRecords) -> Option<Self> {
        custom_records
            .data
            .get(&Self::CUSTOM_RECORD_KEY)
            .and_then(|data| {
                if data.len() != 32 + 16 {
                    return None;
                }
                let secret: [u8; 32] = data[..32].try_into().unwrap();
                let payment_secret = Hash256::from(secret);
                let total_amount = u128::from_le_bytes(data[32..].try_into().unwrap());
                Some(Self::new(payment_secret, total_amount))
            })
    }
}

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct AmpPaymentData {
    pub total_amp_count: u16,
    pub payment_hash: Hash256,
    pub child_desc: AmpChildDesc,
    pub total_amount: u128,
}

impl AmpPaymentData {
    pub const CUSTOM_RECORD_KEY: u32 = USER_CUSTOM_RECORDS_MAX_INDEX + 2;

    pub fn new(
        payment_hash: Hash256,
        total_amp_count: u16,
        child_desc: AmpChildDesc,
        total_amount: u128,
    ) -> Self {
        Self {
            payment_hash,
            total_amp_count,
            child_desc,
            total_amount,
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.extend_from_slice(self.payment_hash.as_ref());
        vec.extend_from_slice(&self.total_amp_count.to_le_bytes());
        vec.extend_from_slice(&self.child_desc.index.to_le_bytes());
        vec.extend_from_slice(self.child_desc.secret.as_bytes());
        vec.extend_from_slice(&self.total_amount.to_le_bytes());
        vec
    }

    pub fn index(&self) -> u16 {
        self.child_desc.index
    }

    pub fn write(&self, custom_records: &mut PaymentCustomRecords) {
        custom_records
            .data
            .insert(Self::CUSTOM_RECORD_KEY, self.to_vec());
    }

    pub fn read(custom_records: &PaymentCustomRecords) -> Option<Self> {
        custom_records
            .data
            .get(&Self::CUSTOM_RECORD_KEY)
            .and_then(|data| {
                if data.len() != 32 + 4 + 32 + 16 {
                    return None;
                }
                let parent_hash: [u8; 32] = data[..32].try_into().unwrap();
                let total_amp_count = u16::from_le_bytes(data[32..34].try_into().unwrap());
                let index = u16::from_le_bytes(data[34..36].try_into().unwrap());
                let secret = AmpSecret::new(data[36..68].try_into().unwrap());
                let total_amount = u128::from_le_bytes(data[68..].try_into().unwrap());
                Some(Self::new(
                    Hash256::from(parent_hash),
                    total_amp_count,
                    AmpChildDesc::new(index, secret),
                    total_amount,
                ))
            })
    }
}
