mod config;
pub use config::RpcConfig;

use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use log::debug;
use serde::Deserialize;
use std::future::Future;

use crate::ckb::Command;

#[derive(Debug, Deserialize)]
pub struct HttpRequest {
    pub id: Option<u64>,
    pub command: Command,
}

async fn handle_request(Json(http_request): Json<HttpRequest>) -> impl IntoResponse {
    debug!("Recevied http request: {:?}", http_request);
    StatusCode::OK
}

pub async fn start_rpc<F>(config: RpcConfig, shutdown_signal: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let app = Router::new().route("/", post(handle_request));

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
