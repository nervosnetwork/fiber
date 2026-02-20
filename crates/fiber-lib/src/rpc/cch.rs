use crate::{
    cch::{CchInvoice, CchMessage, CchOrder, CchOrderStatus},
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::Currency,
};
use jsonrpsee::{
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call_t, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[derive(Serialize, Deserialize)]
pub struct SendBTCParams {
    /// Payment request string for the BTC Lightning payee.
    ///
    /// The invoice should not be expired soon. The remaining expiry time should be greater than the CCH config
    /// `min_incoming_invoice_expiry_delta_seconds`.
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
    /// Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,

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
    /// Payment request string for the CKB Fiber payee.
    ///
    /// The invoice should not be expired soon. The remaining expiry time should be greater than the CCH config
    /// `min_incoming_invoice_expiry_delta_seconds`.
    pub fiber_pay_req: String,
}

#[derive(Serialize, Deserialize)]
pub struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
}

/// RPC module for cross chain hub demonstration.
#[rpc(server)]
trait CchRpc {
    /// Creates a CCH order for a BTC Lightning payee.
    #[method(name = "send_btc")]
    async fn send_btc(&self, params: SendBTCParams) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Creates a CCH order for a CKB Fiber payee.
    #[method(name = "receive_btc")]
    async fn receive_btc(
        &self,
        params: ReceiveBTCParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;

    /// Get a CCH order by payment hash.
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
            expiry_delta_seconds: value.expiry_delta_seconds,
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
