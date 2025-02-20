use crate::{
    cch::{
        CchInvoice, CchInvoiceState, CchMessage, CchOrder, CchOrderStatus, CchOrderStore,
        CchPaymentState,
    },
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::{CkbInvoice, Currency},
};
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{
        error::{CALL_EXECUTION_FAILED_CODE, INTERNAL_ERROR_CODE},
        ErrorObjectOwned,
    },
};
use lightning_invoice::Bolt11Invoice;
use ractor::{call_t, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use thiserror::Error;

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
    /// Payment hash for the HTLC for both CKB and BTC. If not provided, return all orders.
    /// TODO: Add support for pagination.
    payment_hash: Option<Hash256>,
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
    #[method(name = "get_cch_order")]
    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned>;
}

pub(crate) struct CchRpcServerImpl<S> {
    cch_actor: ActorRef<CchMessage>,
    cch_store: S,
}

impl<S> CchRpcServerImpl<S> {
    pub(crate) fn new(cch_actor: ActorRef<CchMessage>, cch_store: S) -> Self {
        CchRpcServerImpl {
            cch_actor,
            cch_store,
        }
    }
}

const TIMEOUT: u64 = 1000;

#[async_trait]
impl<S> CchRpcServer for CchRpcServerImpl<S>
where
    S: CchOrderStore + Send + Sync + 'static,
{
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
        })??;

        SendBTCResponse::try_from(result).map_err(|error| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                error.to_string(),
                Option::<()>::None,
            )
        })
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
        })??;

        ReceiveBTCResponse::try_from(result).map_err(|error| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                error.to_string(),
                Option::<()>::None,
            )
        })
    }

    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        let orders = match params.payment_hash {
            Some(payment_hash) => {
                let order = self
                    .cch_store
                    .get_cch_order(&payment_hash)
                    .map_err(|error| {
                        ErrorObjectOwned::owned(
                            CALL_EXECUTION_FAILED_CODE,
                            error.to_string(),
                            Option::<()>::None,
                        )
                    })?;
                vec![CchOrderItem::from(order)]
            }
            None => self
                .cch_store
                .get_cch_orders()
                .map_err(|error| {
                    ErrorObjectOwned::owned(
                        CALL_EXECUTION_FAILED_CODE,
                        error.to_string(),
                        Option::<()>::None,
                    )
                })?
                .into_iter()
                .map(Into::into)
                .collect(),
        };
        Ok(orders)
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize)]
pub struct SendBTCResponse {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
    /// Payment preimage for the HTLC for both CKB and BTC.
    pub payment_preimage: Option<Hash256>,
    /// Seconds since epoch when the order is created.
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// Payment request for BTC
    #[serde_as(as = "DisplayFromStr")]
    pub btc_pay_req: Bolt11Invoice,
    /// Payment request for CKB
    #[serde_as(as = "DisplayFromStr")]
    pub fiber_pay_req: CkbInvoice,
    /// The state of the payment that is sent to the cross-chain hub.
    pub in_state: CchInvoiceState,
    /// The state of the payment that is sent from the cross-chain hub.
    pub out_state: CchPaymentState,
    /// The status of the order.
    pub status: CchOrderStatus,
}

#[serde_as]
#[derive(Debug, Clone, Serialize)]
pub struct ReceiveBTCResponse {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
    /// Payment preimage for the HTLC for both CKB and BTC.
    pub payment_preimage: Option<Hash256>,
    /// Seconds since epoch when the order is created.
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// Payment request for BTC
    #[serde_as(as = "DisplayFromStr")]
    pub btc_pay_req: Bolt11Invoice,
    /// Payment request for CKB
    #[serde_as(as = "DisplayFromStr")]
    pub fiber_pay_req: CkbInvoice,
    /// The state of the payment that is sent to the cross-chain hub.
    pub in_state: CchInvoiceState,
    /// The state of the payment that is sent from the cross-chain hub.
    pub out_state: CchPaymentState,
    /// The status of the order.
    pub status: CchOrderStatus,
}

/// The response for get_cch_order RPC. It contains a list of cch orders.
pub type CchOrderResponse = Vec<CchOrderItem>;

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrderItem {
    /// Payment hash for the HTLC.
    pub payment_hash: Hash256,
    /// Payment preimage for the HTLC.
    pub payment_preimage: Option<Hash256>,

    /// Seconds since epoch when the order is created.
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// The invoice for the payment that is sent to the cross-chain hub.
    pub in_invoice: CchInvoice,
    /// The invoice for the payment that is sent from the cross-chain hub.
    pub out_invoice: CchInvoice,
    /// The state of the payment that is sent to the cross-chain hub.
    pub in_state: CchInvoiceState,
    /// The state of the payment that is sent from the cross-chain hub.
    pub out_state: CchPaymentState,
    /// The status of the order.
    pub status: CchOrderStatus,
}

#[derive(Error, Debug)]
pub enum ConversionError {
    #[error("Failed to convert CchOrder: {0}")]
    ConversionError(#[from] anyhow::Error),
}

impl From<ConversionError> for ErrorObjectOwned {
    fn from(val: ConversionError) -> Self {
        ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, val.to_string(), Option::<()>::None)
    }
}

impl From<CchOrder> for CchOrderItem {
    fn from(value: CchOrder) -> Self {
        Self {
            payment_hash: value.payment_hash,
            payment_preimage: value.payment_preimage,
            created_at: value.created_at,
            expires_after: value.expires_after,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            in_invoice: value.in_invoice,
            out_invoice: value.out_invoice,
            in_state: value.in_state,
            out_state: value.out_state,
            status: value.status,
        }
    }
}

impl TryFrom<CchOrder> for SendBTCResponse {
    type Error = ConversionError;

    fn try_from(value: CchOrder) -> Result<Self, Self::Error> {
        let status = value.get_status();
        let btc_pay_req = match value.out_invoice {
            CchInvoice::Lightning(ref btc_invoice) => btc_invoice.clone(),
            _ => Err(anyhow::anyhow!(
                "Expecting having lightning invoice in the out_invoice field of a SendBTC cch order"
            ))?,
        };
        let fiber_pay_req = match value.in_invoice {
            CchInvoice::Fiber(ref fiber_invoice) => fiber_invoice.clone(),
            _ => Err(anyhow::anyhow!(
                "Expecting having fiber invoice in the in_invoice field of a SendBTC cch order"
            ))?,
        };

        Ok(Self {
            payment_hash: value.payment_hash,
            payment_preimage: value.payment_preimage,
            created_at: value.created_at,
            expires_after: value.expires_after,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            btc_pay_req,
            fiber_pay_req,
            in_state: value.in_state,
            out_state: value.out_state,
            status,
        })
    }
}

impl TryFrom<CchOrder> for ReceiveBTCResponse {
    type Error = ConversionError;

    fn try_from(value: CchOrder) -> Result<Self, Self::Error> {
        let status = value.get_status();
        let btc_pay_req = match value.in_invoice {
            CchInvoice::Lightning(ref btc_invoice) => btc_invoice.clone(),
            _ => Err(anyhow::anyhow!(
                "Expecting having lightning invoice in the in_invoice field of a ReceiveBTC cch order"
            ))?,
        };
        let fiber_pay_req = match value.out_invoice {
            CchInvoice::Fiber(ref fiber_invoice) => fiber_invoice.clone(),
            _ => Err(anyhow::anyhow!(
                "Expecting having fiber invoice in the out_invoice field of a ReceiveBTC cch order"
            ))?,
        };

        Ok(Self {
            payment_hash: value.payment_hash,
            payment_preimage: value.payment_preimage,
            created_at: value.created_at,
            expires_after: value.expires_after,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            btc_pay_req,
            fiber_pay_req,
            in_state: value.in_state,
            out_state: value.out_state,
            status,
        })
    }
}
