use crate::{
    cch::{CchInvoice, CchInvoiceState, CchMessage, CchOrder, CchPaymentState},
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
pub(crate) struct SendBtcParams {
    /// Bitcoin payment request string
    btc_pay_req: String,
    /// Request currency
    currency: Currency,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub(crate) struct ReceiveBtcParams {
    /// Fiber payment request string
    pub fiber_pay_req: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    payment_hash: Hash256,
}

/// RPC module for cross chain hub demonstration.
#[rpc(server)]
trait CchRpc {
    /// Send BTC to a address.
    #[method(name = "send_btc")]
    async fn send_btc(&self, params: SendBtcParams) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Receive BTC from a payment hash.
    #[method(name = "receive_btc")]
    async fn receive_btc(
        &self,
        params: ReceiveBtcParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Get receive BTC order by payment hash.
    #[method(name = "get_receive_btc_order")]
    async fn get_receive_btc_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;
}

pub(crate) struct CchRpcServerImpl {
    cch_actor: ActorRef<CchMessage>,
}

impl CchRpcServerImpl {
    pub(crate) fn new(cch_actor: ActorRef<CchMessage>) -> Self {
        CchRpcServerImpl { cch_actor }
    }
}

const TIMEOUT: u64 = 1000;

#[async_trait]
impl CchRpcServer for CchRpcServerImpl {
    async fn send_btc(&self, params: SendBtcParams) -> Result<CchOrderResponse, ErrorObjectOwned> {
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
        params: ReceiveBtcParams,
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

    async fn get_receive_btc_order(
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

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrderResponse {
    pub payment_hash: String,
    pub payment_preimage: Option<String>,
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    pub in_invoice: CchInvoice,
    pub out_invoice: CchInvoice,
    pub in_state: CchInvoiceState,
    pub out_state: CchPaymentState,
}

impl From<CchOrder> for CchOrderResponse {
    fn from(value: CchOrder) -> Self {
        Self {
            payment_hash: value.payment_hash.to_string(),
            payment_preimage: value.payment_preimage.map(|hash| hash.to_string()),
            created_at: value.created_at,
            expires_after: value.expires_after,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            in_invoice: value.in_invoice,
            out_invoice: value.out_invoice,
            in_state: value.in_state,
            out_state: value.out_state,
        }
    }
}
