use lnd_grpc_tonic_client::lnrpc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and has not send out payments yet.
    Pending = 0,
    /// HTLC in the first half is accepted.
    Accepted = 1,
    /// There's an outgoing payment in flight for the second half.
    InFlight = 2,
    /// Order is settled.
    Succeeded = 3,
    /// Order is failed.
    Failed = 4,
}

/// lnd payment is the second half of SendBTCOrder
impl From<lnrpc::payment::PaymentStatus> for CchOrderStatus {
    fn from(status: lnrpc::payment::PaymentStatus) -> Self {
        use lnrpc::payment::PaymentStatus;
        match status {
            PaymentStatus::Succeeded => CchOrderStatus::Succeeded,
            PaymentStatus::Failed => CchOrderStatus::Failed,
            _ => CchOrderStatus::InFlight,
        }
    }
}

/// lnd invoice is the first half of ReceiveBTCOrder
impl From<lnrpc::invoice::InvoiceState> for CchOrderStatus {
    fn from(state: lnrpc::invoice::InvoiceState) -> Self {
        use lnrpc::invoice::InvoiceState;
        // Set to InFlight only when a CKB HTLC is created
        match state {
            InvoiceState::Accepted => CchOrderStatus::Accepted,
            InvoiceState::Canceled => CchOrderStatus::Failed,
            _ => CchOrderStatus::Pending,
        }
    }
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
    pub payment_preimage: Option<String>,

    // Amount required to pay in Shannons
    pub amount_shannons: u64,

    pub status: CchOrderStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiveBTCOrder {
    // Seconds since epoch when the order is created
    pub timestamp: u64,
    // Seconds after timestamp that the order expires
    pub expiry: u64,
    // The minimal expiry in seconds of the final TLC in the CKB network
    pub ckb_final_tlc_expiry: u64,

    // Generated invoice
    pub btc_pay_req: String,
    pub payment_hash: String,
    pub payment_preimage: Option<String>,

    // Amount will be received by the payee
    pub amount_shannons: u64,
    // Payee in the CKB network
    pub payee_pubkey: String,

    pub status: CchOrderStatus,
}
