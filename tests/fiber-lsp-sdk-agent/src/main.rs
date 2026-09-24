use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use ckb_types::prelude::*;
use clap::Parser;
use fiber_lsp_sdk_agent::{Agent, AgentConfig, HttpFiberRpc};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "fiber-lsp-sdk-agent",
    about = "Test-only hosted LSP client using the real fiber-lsp-sdk signer"
)]
struct Args {
    /// Hosted LSP JSON-RPC URL.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_RPC")]
    rpc: String,

    /// Directory for the RootKey, SDK store, tenant token, and channel bindings.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_STORE")]
    store: PathBuf,

    /// Operator token used only for one-time tenant registration.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_OPERATOR_TOKEN")]
    operator_token: String,

    /// Write tenant identity, token, and open material for the E2E driver.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_STATUS_FILE")]
    status_file: Option<PathBuf>,

    /// Test-only HTTP address exposing GET /status, POST /open-channel, and POST /shutdown.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_CONTROL_ADDR")]
    control_addr: Option<String>,

    /// Trusted local dev-chain contract binaries, independent of LSP responses.
    #[arg(long, default_value = "../deploy/contracts")]
    contracts_dir: PathBuf,

    /// Independent CKB endpoint used to approve live funding inputs.
    #[arg(long, default_value = "http://127.0.0.1:8114")]
    ckb_rpc: String,

    /// Polling interval in milliseconds.
    #[arg(long, default_value_t = 200, env = "FIBER_LSP_SDK_AGENT_INTERVAL_MS")]
    interval_ms: u64,

    /// Run one poll cycle and exit.
    #[arg(long)]
    once: bool,
}

#[derive(Debug, Deserialize)]
struct OpenChannelRequest {
    params: fiber_json_types::OpenChannelWithExternalFundingParams,
}

type OpenCommand = (
    OpenChannelRequest,
    oneshot::Sender<Result<fiber_json_types::OpenTenantChannelResult>>,
);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let rpc = HttpFiberRpc::new(&args.rpc, args.operator_token)?;
    let status_file = args.status_file.clone();
    let mut agent = Agent::open(
        rpc,
        AgentConfig {
            store_dir: args.store,
            status_file: args.status_file,
        },
    )
    .await?;

    if args.once {
        initialize_with_retry(&mut agent, &mut None).await?;
        info!(tenant_id = %agent.tenant_id(), "hosted LSP SDK agent ready");
        agent.poll_once().await?;
        return Ok(());
    }

    // Bind the test control server before initialize so wait.sh / Bruno can
    // connect while the agent retries tenant registration against node RPC.
    let (open_tx, mut open_rx) = mpsc::channel::<OpenCommand>(4);
    let (control, mut shutdown_rx) = match (args.control_addr, status_file) {
        (Some(address), Some(status_file)) => {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            (
                Some(tokio::spawn(run_control_server(
                    address,
                    status_file,
                    open_tx,
                    shutdown_tx,
                ))),
                Some(shutdown_rx),
            )
        }
        (Some(_), None) => anyhow::bail!("--control-addr requires --status-file"),
        (None, _) => (None, None),
    };
    if !initialize_with_retry(&mut agent, &mut shutdown_rx).await? {
        if let Some(control) = control {
            control.abort();
        }
        return Ok(());
    }

    info!(tenant_id = %agent.tenant_id(), "hosted LSP SDK agent ready");
    let interval = Duration::from_millis(args.interval_ms);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = wait_for_shutdown(&mut shutdown_rx) => break,
            Some((request, reply)) = open_rx.recv() => {
                let result = open_request(&mut agent, request, &args.contracts_dir, &args.ckb_rpc).await;
                let _ = reply.send(result);
            }
            _ = tokio::time::sleep(interval) => {
                if let Err(error) = agent.poll_once().await {
                    tracing::warn!("SDK agent poll failed: {error}");
                }
            }
        }
    }
    if let Some(control) = control {
        control.abort();
    }
    Ok(())
}

async fn initialize_with_retry<R, S>(
    agent: &mut Agent<R, S>,
    shutdown_rx: &mut Option<oneshot::Receiver<()>>,
) -> Result<bool>
where
    R: fiber_lsp_sdk_agent::FiberRpc,
    S: fiber_lsp_sdk::SignerStore,
{
    loop {
        match agent.initialize().await {
            Ok(()) => return Ok(true),
            Err(error) => {
                tracing::warn!("SDK agent initialize failed; retrying: {error:#}");
                tokio::select! {
                    _ = wait_for_shutdown(shutdown_rx) => return Ok(false),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }
}

async fn open_request<R, S>(
    agent: &mut Agent<R, S>,
    request: OpenChannelRequest,
    contracts: &std::path::Path,
    ckb_rpc: &str,
) -> Result<fiber_json_types::OpenTenantChannelResult>
where
    R: fiber_lsp_sdk_agent::FiberRpc,
    S: fiber_lsp_sdk::SignerStore,
{
    let script = |name: &str| -> Result<ckb_types::packed::Script> {
        let bytes = std::fs::read(contracts.join(name))?;
        Ok(ckb_types::packed::Script::new_builder()
            .code_hash(fiber_types::blake2b_hash_with_salt(&bytes, &[]).pack())
            .hash_type(ckb_types::core::ScriptHashType::Data2)
            .build())
    };
    let network = fiber_lsp_sdk::OpeningNetwork {
        funding_lock: script("funding-lock")?,
        commitment_lock: script("commitment-lock")?,
        commitment_cell_deps: if request.params.funding_udt_type_script.is_some() {
            3
        } else {
            2
        },
        epoch_duration_ms: if cfg!(debug_assertions) {
            2000
        } else {
            14_400_000
        },
        minimum_shutdown_fee: 100_000_000,
    };
    agent
        .open_tenant_channel(request.params, &network, ckb_rpc)
        .await
}

async fn wait_for_shutdown(receiver: &mut Option<oneshot::Receiver<()>>) {
    match receiver {
        Some(receiver) => {
            let _ = receiver.await;
        }
        None => std::future::pending().await,
    }
}

async fn run_control_server(
    address: String,
    status_file: PathBuf,
    open_tx: mpsc::Sender<OpenCommand>,
    shutdown: oneshot::Sender<()>,
) -> Result<()> {
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind test control server at {address}"))?;
    info!(%address, "test control server ready");
    let mut shutdown = Some(shutdown);
    loop {
        let (stream, _) = listener.accept().await?;
        if handle_control_connection(stream, &status_file, &open_tx, &mut shutdown).await? {
            return Ok(());
        }
    }
}

async fn handle_control_connection(
    mut stream: TcpStream,
    status_file: &PathBuf,
    open_tx: &mpsc::Sender<OpenCommand>,
    shutdown: &mut Option<oneshot::Sender<()>>,
) -> Result<bool> {
    let request = read_http_request(&mut stream).await?;
    let (status, body, should_shutdown) = if request.starts_with("GET /status ") {
        match tokio::fs::read(status_file).await {
            Ok(body) => ("200 OK", body, false),
            Err(error) => (
                "503 Service Unavailable",
                format!("{{\"error\":\"status unavailable: {error}\"}}").into_bytes(),
                false,
            ),
        }
    } else if request.starts_with("POST /open-channel ")
        || request.starts_with("POST /open-channel\r\n")
    {
        match open_body(&request) {
            Ok(payload) => {
                let (sender, receiver) = oneshot::channel();
                if open_tx.send((payload, sender)).await.is_err() {
                    ("503 Service Unavailable", b"{}".to_vec(), false)
                } else {
                    match receiver.await {
                        Ok(Ok(result)) => (
                            "200 OK",
                            serde_json::to_vec(&serde_json::json!({"result":result}))?,
                            false,
                        ),
                        Ok(Err(error)) => (
                            "400 Bad Request",
                            serde_json::to_vec(&serde_json::json!({"error":error.to_string()}))?,
                            false,
                        ),
                        Err(error) => (
                            "503 Service Unavailable",
                            serde_json::to_vec(&serde_json::json!({"error":error.to_string()}))?,
                            false,
                        ),
                    }
                }
            }
            Err(error) => (
                "400 Bad Request",
                format!("{{\"error\":\"{error}\"}}").into_bytes(),
                false,
            ),
        }
    } else if request.starts_with("POST /shutdown ") {
        ("200 OK", b"{\"status\":\"restarting\"}".to_vec(), true)
    } else {
        (
            "404 Not Found",
            b"{\"error\":\"not found\"}".to_vec(),
            false,
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    if should_shutdown {
        if let Some(shutdown) = shutdown.take() {
            let _ = shutdown.send(());
        }
    }
    Ok(should_shutdown)
}

async fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let size = stream.read(&mut tmp).await?;
        if size == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..size]);
        if buf.len() > 1024 * 1024 {
            anyhow::bail!("control request too large");
        }
        if let Some(header_end) = find_header_end(&buf) {
            if let Some(content_length) = content_length(&buf[..header_end]) {
                if buf.len() >= header_end + 4 + content_length {
                    break;
                }
            } else {
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let headers = std::str::from_utf8(headers).ok()?;
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.eq_ignore_ascii_case("content-length")).then(|| value.trim().parse().ok())?
    })
}

fn open_body(request: &str) -> Result<OpenChannelRequest> {
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default()
        .trim_end_matches('\0');
    serde_json::from_str(body).context("decode POST /open-channel body")
}
