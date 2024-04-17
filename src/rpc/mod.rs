mod config;
pub use config::RpcConfig;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use log::{debug, error, info};
use serde::Deserialize;
use std::{future::Future, sync::Arc};
use tokio::sync::mpsc;

pub type NetworkActorCommandWithReply = (
    NetworkActorCommand,
    Option<oneshot::Sender<crate::Result<()>>>,
);

use crate::{cch::CchCommand, ckb::NetworkActorCommand};

#[derive(Debug, Deserialize)]
pub enum HttpRequest {
    Command(NetworkActorCommand),
    CchCommand(CchCommand),
}

#[derive(Debug, Deserialize)]
pub struct HttpBody {
    pub id: Option<u64>,
    pub request: HttpRequest,
}

pub struct AppState {
    pub ckb_command_sender: Option<mpsc::Sender<NetworkActorCommandWithReply>>,
    pub cch_command_sender: Option<mpsc::Sender<CchCommand>>,
}

async fn handle_request(
    State(state): State<Arc<AppState>>,
    Json(http_request): Json<HttpBody>,
) -> impl IntoResponse {
    debug!("Received http request: {:?}", http_request);
    let payload = http_request.request;
    match (
        payload,
        &state.ckb_command_sender,
        &state.cch_command_sender,
    ) {
        (HttpRequest::Command(command), Some(ckb_command_sender), _) => {
            let (sender, receiver) = oneshot::channel::<crate::Result<()>>();
            let command = (command, Some(sender));
            ckb_command_sender
                .send(command)
                .await
                .expect("send command");
            debug!("Waiting for command to be processed");
            match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(_) => StatusCode::OK,
                Err(err) => {
                    error!("Error processing command: {:?}", err);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
        }
        (HttpRequest::CchCommand(command), _, Some(cch_command_sender)) => {
            cch_command_sender
                .send(command)
                .await
                .expect("send command");
            StatusCode::OK
        }
        _ => StatusCode::NOT_FOUND,
    }
}

pub async fn start_rpc<F>(
    config: RpcConfig,
    ckb_command_sender: Option<mpsc::Sender<NetworkActorCommandWithReply>>,
    cch_command_sender: Option<mpsc::Sender<CchCommand>>,
    shutdown_signal: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let app_state = Arc::new(AppState {
        ckb_command_sender,
        cch_command_sender,
    });
    let app = Router::new()
        .route("/", post(handle_request))
        .with_state(app_state);

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
