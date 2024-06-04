use crate::cch::CchCommand;
use tokio::sync::mpsc::Sender;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct SendBtcParams {
    pub btc_pay_req: String,
}

#[rpc(server)]
pub trait CchRpc {
    #[method(name = "send_btc")]
    async fn send_btc(&self, params: SendBtcParams) -> Result<(), ErrorObjectOwned>;
}

pub struct CchRpcServerImpl {
    pub cch_command_sender: Sender<CchCommand>,
}

impl CchRpcServerImpl {
    pub fn new(cch_command_sender: Sender<CchCommand>) -> Self {
        CchRpcServerImpl { cch_command_sender }
    }
}

#[async_trait]
impl CchRpcServer for CchRpcServerImpl {
    async fn send_btc(&self, params: SendBtcParams) -> Result<(), ErrorObjectOwned> {
        let command = CchCommand::SendBTC(crate::cch::SendBTC {
            btc_pay_req: params.btc_pay_req,
        });

        self.cch_command_sender
            .send(command)
            .await
            .expect("send command");
        Ok(())
    }
}
