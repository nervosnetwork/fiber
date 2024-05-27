mod config;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
pub use config::RpcConfig;
use log::{debug, error, info};
use ractor::{rpc::CallResult, ActorRef, RpcReplyPort};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{future::Future, sync::Arc};
use tokio::sync::mpsc;

pub type NetworkActorCommandWithReply =
    (NetworkActorCommand, Option<mpsc::Sender<crate::Result<()>>>);

pub type InvoiceCommandWithReply = (InvoiceCommand, Option<mpsc::Sender<crate::Result<String>>>);

use crate::{
    cch::CchCommand,
    ckb::{
        channel::{ChannelCommand, ChannelCommandWithId, ProcessingChannelError},
        NetworkActorCommand, NetworkActorMessage,
    },
    invoice::InvoiceCommand,
};

#[derive(Debug, Deserialize)]
pub struct HttpBody<T> {
    pub id: Option<u64>,
    pub request: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    code: u16,
    message: String,
}

impl From<&ProcessingChannelError> for RpcError {
    fn from(err: &ProcessingChannelError) -> Self {
        RpcError {
            code: StatusCode::BAD_REQUEST.as_u16(),
            message: format!("{:?}", err),
        }
    }
}

impl From<ProcessingChannelError> for RpcError {
    fn from(err: ProcessingChannelError) -> Self {
        Self::from(&err)
    }
}

impl IntoResponse for RpcError {
    fn into_response(self) -> Response {
        // Also set the http status code
        (
            StatusCode::from_u16(self.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(self),
        )
            .into_response()
    }
}

type CkbRpcRequest = HttpBody<NetworkActorCommand>;
type CchRpcRequest = HttpBody<CchCommand>;
type InvoiceRpcRequest = HttpBody<InvoiceCommand>;

pub struct CkbRpcState {
    pub ckb_network_actor: ActorRef<NetworkActorMessage>,
}

pub const TIMEOUT_MS: u64 = 60000;

async fn call_network_actor<Error, TReply, TMsgBuilder>(
    actor: &ActorRef<NetworkActorMessage>,
    msg_builder: TMsgBuilder,
) -> Response
where
    Error: Into<RpcError> + core::fmt::Debug,
    TReply: Serialize,
    TMsgBuilder: FnOnce(RpcReplyPort<Result<TReply, Error>>) -> NetworkActorCommand,
{
    let builder = |x| NetworkActorMessage::new_command(msg_builder(x));
    match ractor::rpc::call(
        &actor.get_cell(),
        builder,
        Some(Duration::from_millis(TIMEOUT_MS)),
    )
    .await
    {
        Ok(CallResult::Success(Ok(r))) => Json::<TReply>(r).into_response(),
        Ok(CallResult::Success(Err(error))) => {
            error!(
                "Call network actor failed because of processing error: {:?}",
                error
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(CallResult::Timeout) => {
            error!("Call network actor timeout");
            StatusCode::REQUEST_TIMEOUT.into_response()
        }
        Ok(CallResult::SenderError) => {
            error!("Call network actor sender error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(err) => {
            error!(
                "Call network actor failed because of message error: {:?}",
                err
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

impl CkbRpcState {
    pub async fn process_command(&self, command: NetworkActorCommand) -> Response {
        match command {
            NetworkActorCommand::OpenChannel(open_channel, _) => {
                call_network_actor(&self.ckb_network_actor, |reply_port| {
                    NetworkActorCommand::OpenChannel(open_channel, Some(reply_port))
                })
                .await
            }
            NetworkActorCommand::AcceptChannel(accept_channel, _) => {
                call_network_actor(&self.ckb_network_actor, |reply_port| {
                    NetworkActorCommand::AcceptChannel(accept_channel, Some(reply_port))
                })
                .await
            }
            NetworkActorCommand::ControlPcnChannel(ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::AddTlc(cmd, _),
            }) => {
                call_network_actor(&self.ckb_network_actor, |reply_port| {
                    let new_cmd = ChannelCommandWithId {
                        channel_id,
                        command: ChannelCommand::AddTlc(cmd, Some(reply_port)),
                    };
                    NetworkActorCommand::ControlPcnChannel(new_cmd)
                })
                .await
            }
            _ => {
                self.ckb_network_actor
                    .send_message(NetworkActorMessage::new_command(command))
                    .expect("ckb network actor alive");
                StatusCode::OK.into_response()
            }
        }
    }
}

pub struct CchRpcState {
    pub cch_command_sender: mpsc::Sender<CchCommand>,
}

pub struct InvoiceRpcState {
    pub invoice_command_sender: mpsc::Sender<InvoiceCommandWithReply>,
}

async fn serve_ckb_rpc(
    State(state): State<Arc<CkbRpcState>>,
    Json(http_request): Json<CkbRpcRequest>,
) -> Response {
    debug!("Received http request: {:?}", http_request);
    let command = http_request.request;
    state.process_command(command).await
}

async fn serve_cch_rpc(
    State(state): State<Arc<CchRpcState>>,
    Json(http_request): Json<CchRpcRequest>,
) -> impl IntoResponse {
    debug!("Received http request: {:?}", http_request);
    state
        .cch_command_sender
        .send(http_request.request)
        .await
        .expect("send command");
    StatusCode::OK
}

async fn serve_invoice_rpc(
    State(state): State<Arc<InvoiceRpcState>>,
    Json(http_request): Json<InvoiceRpcRequest>,
) -> impl IntoResponse {
    debug!("Received http request: {:?}", http_request);
    let (sender, mut receiver) = mpsc::channel(1);
    let command = (http_request.request, Some(sender));

    let _ = state.invoice_command_sender.send(command).await;
    let res = receiver.recv().await;
    let result = match res {
        Some(Ok(data)) => (StatusCode::OK, data),
        Some(Err(err)) => {
            // status code 400 with err message
            (StatusCode::BAD_REQUEST, err.to_string())
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "No response".to_string()),
    };
    result
}

pub async fn start_rpc<F>(
    config: RpcConfig,
    ckb_network_actor: Option<ActorRef<NetworkActorMessage>>,
    cch_command_sender: Option<mpsc::Sender<CchCommand>>,
    invoice_command_sender: Option<mpsc::Sender<InvoiceCommandWithReply>>,
    shutdown_signal: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let mut app = Router::new();
    if let Some(ckb_command_sender) = ckb_network_actor {
        let ckb_router = Router::new()
            .route("/", post(serve_ckb_rpc))
            .with_state(Arc::new(CkbRpcState {
                ckb_network_actor: ckb_command_sender,
            }));
        app = app.nest("/ckb", ckb_router);
    }
    if let Some(cch_command_sender) = cch_command_sender {
        let cch_router = Router::new()
            .route("/", post(serve_cch_rpc))
            .with_state(Arc::new(CchRpcState { cch_command_sender }));
        app = app.nest("/cch", cch_router);
    }
    if let Some(invoice_command_sender) = invoice_command_sender {
        let invoice_router = Router::new()
            .route("/", post(serve_invoice_rpc))
            .with_state(Arc::new(InvoiceRpcState {
                invoice_command_sender,
            }));
        app = app.nest("/invoice", invoice_router);
    }

    let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
    let listener = tokio::net::TcpListener::bind(listening_addr)
        .await
        .expect("bind rpc addr");
    info!("Starting rpc server at {:?}", &listener.local_addr());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .expect("start rpc server");
    info!("Rpc service stopped");
}
