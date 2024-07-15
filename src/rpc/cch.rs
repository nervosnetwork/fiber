use crate::{
    cch::{CchMessage, CchOrderStatus, ReceiveBTCOrder},
    ckb::{
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
    pub btc_pay_req: String,
    pub currency: Currency,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendBTCResponse {
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    // The minimal expiry in seconds of the final TLC in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry: u64,

    pub currency: Currency,
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub btc_pay_req: String,
    pub ckb_pay_req: String,
    pub payment_hash: String,

    #[serde_as(as = "U128Hex")]
    // Amount required to pay in Satoshis, including fee
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ReceiveBtcParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: String,
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
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    // The minimal expiry in seconds of the final TLC in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry: u64,

    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub btc_pay_req: String,
    pub payment_hash: String,
    pub channel_id: Hash256,
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_id: Option<u64>,

    // Amount will be received by the payee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,
}

#[rpc(server)]
pub trait CchRpc {
    #[method(name = "send_btc")]
    async fn send_btc(&self, params: SendBtcParams) -> Result<SendBTCResponse, ErrorObjectOwned>;

    #[method(name = "receive_btc")]
    async fn receive_btc(
        &self,
        params: ReceiveBtcParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned>;

    #[method(name = "get_receive_btc_order")]
    async fn get_receive_btc_order(
        &self,
        params: GetReceiveBtcOrderParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned>;
}

pub struct CchRpcServerImpl {
    pub cch_actor: ActorRef<CchMessage>,
}

impl CchRpcServerImpl {
    pub fn new(cch_actor: ActorRef<CchMessage>) -> Self {
        CchRpcServerImpl { cch_actor }
    }
}

pub const TIMEOUT: u64 = 1000;

#[async_trait]
impl CchRpcServer for CchRpcServerImpl {
    async fn send_btc(&self, params: SendBtcParams) -> Result<SendBTCResponse, ErrorObjectOwned> {
        let result = call_t!(
            self.cch_actor,
            CchMessage::SendBTC,
            TIMEOUT,
            crate::cch::SendBTC {
                btc_pay_req: params.btc_pay_req,
                currency: params.currency,
            }
        )
        .map_err(|ractor_error| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                ractor_error.to_string(),
                Option::<()>::None,
            )
        })?;

        result
            .map(|order| SendBTCResponse {
                timestamp: order.created_at,
                expiry: order.expires_after,
                ckb_final_tlc_expiry: order.ckb_final_tlc_expiry,
                currency: order.currency,
                wrapped_btc_type_script: order.wrapped_btc_type_script,
                btc_pay_req: order.btc_pay_req,
                ckb_pay_req: order.ckb_pay_req,
                payment_hash: order.payment_hash,
                amount_sats: order.amount_sats,
                fee_sats: order.fee_sats,
                status: order.status,
            })
            .map_err(Into::into)
    }

    async fn receive_btc(
        &self,
        params: ReceiveBtcParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned> {
        let result = call_t!(
            self.cch_actor,
            CchMessage::ReceiveBTC,
            TIMEOUT,
            crate::cch::ReceiveBTC {
                payment_hash: params.payment_hash,
                channel_id: params.channel_id,
                amount_sats: params.amount_sats,
                final_tlc_expiry: params.final_tlc_expiry,
            }
        )
        .map_err(|ractor_error| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                ractor_error.to_string(),
                Option::<()>::None,
            )
        })?;

        result.map(Into::into).map_err(Into::into)
    }

    async fn get_receive_btc_order(
        &self,
        params: GetReceiveBtcOrderParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned> {
        let result = call_t!(
            self.cch_actor,
            CchMessage::GetReceiveBTCOrder,
            TIMEOUT,
            params.payment_hash
        )
        .map_err(|ractor_error| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                ractor_error.to_string(),
                Option::<()>::None,
            )
        })?;

        result.map(Into::into).map_err(Into::into)
    }
}

impl From<ReceiveBTCOrder> for ReceiveBTCResponse {
    fn from(value: ReceiveBTCOrder) -> Self {
        Self {
            timestamp: value.created_at,
            expiry: value.expires_after,
            ckb_final_tlc_expiry: value.ckb_final_tlc_expiry,
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
