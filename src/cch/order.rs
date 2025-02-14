use super::CchError;
use lnd_grpc_tonic_client::lnrpc;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::{str::FromStr as _, time::Duration};

use crate::{
    fiber::{
        hash_algorithm::HashAlgorithm,
        serde_utils::{U128Hex, U64Hex},
        types::{Hash256, Pubkey},
    },
    invoice::{Currency, InvoiceBuilder},
};

/// The status of a cross-chain hub order, will update as the order progresses.
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
            InvoiceState::Settled => CchOrderStatus::Succeeded,
            _ => CchOrderStatus::Pending,
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendBTCOrder {
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    // The minimal expiry delta in milliseconds of the final TLC hop in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry_delta: u64,

    pub currency: Currency,
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub btc_pay_req: String,
    pub fiber_payee_pubkey: Pubkey,
    pub fiber_pay_req: String,
    pub payment_hash: String,
    pub payment_preimage: Option<String>,

    #[serde_as(as = "U128Hex")]
    /// Amount required to pay in Satoshis via wrapped BTC, including the fee for the cross-chain hub
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,
}

impl SendBTCOrder {
    pub fn generate_ckb_invoice(&mut self) -> Result<(), CchError> {
        let invoice_builder = InvoiceBuilder::new(self.currency)
            .payee_pub_key(self.fiber_payee_pubkey.into())
            .amount(Some(self.amount_sats))
            .payment_hash(
                Hash256::from_str(&self.payment_hash)
                    .map_err(|_| CchError::HexDecodingError(self.payment_hash.clone()))?,
            )
            .hash_algorithm(HashAlgorithm::Sha256)
            .expiry_time(Duration::from_secs(self.expires_after))
            .final_expiry_delta(self.ckb_final_tlc_expiry_delta)
            .udt_type_script(self.wrapped_btc_type_script.clone().into());

        let invoice = invoice_builder.build()?;
        self.fiber_pay_req = invoice.to_string();

        Ok(())
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiveBTCOrder {
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    // The minimal expiry in seconds of the final TLC in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry_delta: u64,

    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub btc_pay_req: String,
    pub fiber_pay_req: String,
    pub payment_hash: String,
    pub payment_preimage: Option<String>,

    /// Amount required to pay in Satoshis via BTC, including the fee for the cross-chain hub
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,
}
