#[cfg(not(target_arch = "wasm32"))]
use crate::cch::{CchMessage, CchOrderStatus, ReceiveBTCOrder};
use crate::{
    fiber::serde_utils::{U128Hex, U64Hex},
    invoice::Currency,
};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::{
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
    /// Generated invoice for CKB
    pub fiber_pay_invoice: String,
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
    /// Fiber payment request string
    pub fiber_pay_req: String,
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
// #[rpc(server)]
#[cfg(not(target_arch = "wasm32"))]
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

pub struct CchRpcServerImpl {
    cch_actor: ActorRef<CchMessage>,
}

impl CchRpcServerImpl {
    pub fn new(cch_actor: ActorRef<CchMessage>) -> Self {
        CchRpcServerImpl { cch_actor }
    }
}

const TIMEOUT: u64 = 1000;
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl CchRpcServer for CchRpcServerImpl {
    /// Send BTC to a address.
    async fn send_btc(&self, params: SendBtcParams) -> Result<SendBTCResponse, ErrorObjectOwned> {
        // <Self as CchRpcServerImpl>::send_btc(self, params).await
        self.send_btc(params).await
    }

    /// Receive BTC from a payment hash.
    async fn receive_btc(
        &self,
        params: ReceiveBtcParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned> {
        self.receive_btc(params).await
    }

    /// Get receive BTC order by payment hash.
    async fn get_receive_btc_order(
        &self,
        params: GetReceiveBtcOrderParams,
    ) -> Result<ReceiveBTCResponse, ErrorObjectOwned> {
        self.get_receive_btc_order(params).await
    }
}

// #[async_trait::async_trait(?Send)]
impl CchRpcServerImpl {
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
                ckb_final_tlc_expiry_delta: order.ckb_final_tlc_expiry_delta,
                currency: order.currency,
                wrapped_btc_type_script: order.wrapped_btc_type_script,
                btc_pay_req: order.btc_pay_req,
                fiber_pay_invoice: order
                    .fiber_pay_invoice
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
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
                fiber_pay_req: params.fiber_pay_req,
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
            ckb_final_tlc_expiry_delta: value.ckb_final_tlc_expiry_delta,
            wrapped_btc_type_script: value.wrapped_btc_type_script,
            btc_pay_req: value.btc_pay_req,
            payment_hash: value.payment_hash,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            status: value.status,
        }
    }
}
