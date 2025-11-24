#[cfg(not(target_arch = "wasm32"))]
use crate::cch::{CchMessage, CchOrder, CchOrderStatus};
use crate::{
    cch::CchInvoice,
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
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
pub struct SendBTCParams {
    /// Bitcoin payment request string
    pub btc_pay_req: String,
    /// Request currency
    pub currency: Currency,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrderResponse {
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

    /// Generated invoice for the incoming payment
    pub incoming_invoice: CchInvoice,
    /// The final payee to accept the payment. It has the different network with incoming invoice.
    pub outgoing_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
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
pub struct ReceiveBTCParams {
    /// Fiber payment request string
    pub fiber_pay_req: String,
}

#[derive(Serialize, Deserialize)]
pub struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
}

/// RPC module for cross chain hub demonstration.
// #[rpc(server)]
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait CchRpc {
    /// Send BTC to a address.
    #[method(name = "send_btc")]
    async fn send_btc(&self, params: SendBTCParams) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Receive BTC from a payment hash.
    #[method(name = "receive_btc")]
    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Get receive BTC order by payment hash.
    #[method(name = "get_cch_order")]
    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;
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
    async fn send_btc(&self, params: SendBTCParams) -> Result<CchOrderResponse, ErrorObjectOwned> {
        // <Self as CchRpcServerImpl>::send_btc(self, params).await
        self.send_btc(params).await
    }

    /// Receive BTC from a payment hash.
    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        self.receive_btc(params).await
    }

    /// Get receive BTC order by payment hash.
    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        self.get_cch_order(params).await
    }
}

// #[async_trait::async_trait(?Send)]
impl CchRpcServerImpl {
    async fn send_btc(&self, params: SendBTCParams) -> Result<CchOrderResponse, ErrorObjectOwned> {
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

        result.map(Into::into).map_err(Into::into)
    }

    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
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

    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        let result = call_t!(
            self.cch_actor,
            CchMessage::GetCchOrder,
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

impl From<CchOrder> for CchOrderResponse {
    fn from(value: CchOrder) -> Self {
        Self {
            timestamp: value.created_at,
            expiry: value.expires_after,
            ckb_final_tlc_expiry_delta: value.ckb_final_tlc_expiry_delta,
            wrapped_btc_type_script: value.wrapped_btc_type_script,
            outgoing_pay_req: value.outgoing_pay_req,
            incoming_invoice: value.incoming_invoice,
            payment_hash: value.payment_hash,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            status: value.status,
        }
    }
}
