use crate::cch::CchMessage;
use crate::rpc::utils::RpcResultExt;
use jsonrpsee::{proc_macros::rpc, types::ErrorObjectOwned};
use ractor::{call_t, ActorRef};

pub use fiber_json_types::{CchOrderResponse, GetCchOrderParams, ReceiveBTCParams, SendBTCParams};

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
        let currency = params.currency.into();
        let result = call_t!(
            self.cch_actor,
            CchMessage::SendBTC,
            TIMEOUT,
            crate::cch::SendBTC {
                btc_pay_req: params.btc_pay_req,
                currency,
            }
        )
        .rpc_err_no_data()?;

        result.map(CchOrderResponse::from).map_err(Into::into)
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
        .rpc_err_no_data()?;

        result.map(CchOrderResponse::from).map_err(Into::into)
    }

    async fn get_cch_order(
        &self,
        params: GetCchOrderParams,
    ) -> Result<CchOrderResponse, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.into();
        let result = call_t!(
            self.cch_actor,
            CchMessage::GetCchOrder,
            TIMEOUT,
            payment_hash
        )
        .rpc_err_no_data()?;

        result.map(CchOrderResponse::from).map_err(Into::into)
    }
}
