use crate::{
    cch::{CchMessage, CchOrderStatus, ReceiveBTCOrder},
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::Currency,
};
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call_t, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[derive(Serialize, Deserialize)]
pub struct SendBtcParams {
    /// Bitcoin payment request string
    pub btc_pay_req: String,
    /// Request currency
    pub currency: Currency,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendBTCResponse {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    /// Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// The minimal expiry in seconds of the final TLC in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry_delta: u64,

    /// Request currency
    pub currency: Currency,
    /// Wrapped BTC type script
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    /// Payment request for BTC
    pub btc_pay_req: String,
    /// Payment request for CKB
    pub ckb_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: String,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// Order status
    pub status: CchOrderStatus,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ReceiveBtcParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: String,
    /// Channel ID for the CKB payment.
    pub channel_id: Hash256,
    /// How many satoshis to receive, excluding cross-chain hub fee.
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Expiry set for the HTLC for the CKB payment to the payee.
    #[serde_as(as = "U64Hex")]
    pub final_tlc_expiry: u64,
}

#[derive(Serialize, Deserialize)]
pub struct GetReceiveBtcOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: String,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiveBTCResponse {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    /// Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// The minimal expiry in seconds of the final TLC in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry_delta: u64,

    /// Wrapped BTC type script
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    /// Payment request for BTC
    pub btc_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: String,
    /// Channel ID for the CKB payment.
    pub channel_id: Hash256,
    /// TLC ID for the CKB payment.
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_id: Option<u64>,

    /// Amount will be received by the payee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    /// Order status
    pub status: CchOrderStatus,
}

/// RPC module for cross chain hub demonstration.
#[rpc(server)]
trait CchRpc {
    /// Send BTC to a address.
    #[method(name = "send_btc")]
    async fn send_btc(&self, params: SendBtcParams) -> Result<SendBTCResponse, ErrorObjectOwned>;

    /// Receive BTC from a payment hash.
    #[method(name = "receive_btc")]
    async fn receive_btc(
        &self,
        params: ReceiveBtcParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned>;

    /// Get receive BTC order by payment hash.
    #[method(name = "get_receive_btc_order")]
    async fn get_receive_btc_order(
        &self,
        params: GetReceiveBtcOrderParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned>;
}

const TIMEOUT: u64 = 1000;

impl From<ReceiveBTCOrder> for ReceiveBTCResponse {
    fn from(value: ReceiveBTCOrder) -> Self {
        Self {
            timestamp: value.created_at,
            expiry: value.expires_after,
            ckb_final_tlc_expiry_delta: value.ckb_final_tlc_expiry_delta,
            wrapped_btc_type_script: value.wrapped_btc_type_script,
            btc_pay_req: value.btc_pay_req,
            payment_hash: value.payment_hash,
            channel_id: value.channel_id,
            tlc_id: value.tlc_id,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            status: value.status,
        }
    }
}
