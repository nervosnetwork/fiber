mod config;
pub use config::RpcConfig;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use log::{debug, info};
use serde::Deserialize;
use std::{future::Future, sync::Arc};
use tokio::sync::mpsc;

use crate::ckb::Command;

#[derive(Debug, Deserialize)]
pub enum HttpRequest {
    Command(Command),
}

#[derive(Debug, Deserialize)]
pub struct HttpBody {
    pub id: Option<u64>,
    pub request: HttpRequest,
}

pub struct AppState {
    pub ckb_command_sender: mpsc::Sender<Command>,
}

async fn handle_request(
    State(state): State<Arc<AppState>>,
    Json(http_request): Json<HttpBody>,
) -> impl IntoResponse {
    debug!("Recevied http request: {:?}", http_request);
    match http_request {
        HttpBody { id: _, request } => match request {
            HttpRequest::Command(command) => {
                state
                    .ckb_command_sender
                    .send(command)
                    .await
                    .expect("send command");
            }
        },
    }
    StatusCode::OK
}

pub async fn start_rpc<F>(
    config: RpcConfig,
    ckb_command_sender: mpsc::Sender<Command>,
    shutdown_signal: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let app_state = Arc::new(AppState { ckb_command_sender });
    let app = Router::new()
        .route("/", post(handle_request))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(config.listening_addr)
        .await
        .expect("bind rpc addr");
    info!("Starting rpc server at {:?}", &listener.local_addr());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .expect("start rpc server");
    info!("Rpc service stopped");
}
