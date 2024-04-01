use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    Pending = 0,
    Completed = 1,
    Expired = 2,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendBTCOrder {
    // Seconds since epoch when the order is created
    pub timestamp: u64,
    // Seconds after timestamp that the order expires
    pub expiry: u64,
    // The minimal expiry in seconds of the final TLC in the CKB network
    pub ckb_final_tlc_expiry: u64,

    pub btc_pay_req: String,
    pub payment_hash: String,

    // Amount required to pay in Shannons
    pub amount_shannons: u64,
    pub fulfilled_amount_shannons: u64,

    pub status: CchOrderStatus,
}
