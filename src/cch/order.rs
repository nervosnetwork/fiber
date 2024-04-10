use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and has not send out payments yet.
    Pending = 0,
    /// There's an outgoing payment in flight.
    InFlight = 1,
    /// Order is settled.
    Succeeded = 2,
    /// Order is failed.
    Failed = 3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub status: CchOrderStatus,
}
