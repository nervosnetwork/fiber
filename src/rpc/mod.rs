mod config;
pub use config::RpcConfig;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use log::{debug, info};
use serde::Deserialize;
use std::{future::Future, sync::Arc};
use tokio::sync::mpsc;

use crate::{cch::CchCommand, ckb::Command};

#[derive(Debug, Deserialize)]
pub enum HttpRequest {
    Command(Command),
    CchCommand(CchCommand),
}

#[derive(Debug, Deserialize)]
pub struct HttpBody {
    pub id: Option<u64>,
    pub request: HttpRequest,
}

pub struct AppState {
    pub ckb_command_sender: Option<mpsc::Sender<Command>>,
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
            ckb_command_sender
                .send(command)
                .await
                .expect("send command");
            StatusCode::OK
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
    ckb_command_sender: Option<mpsc::Sender<Command>>,
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
