use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use fiber_lsp_sdk_agent::{Agent, AgentConfig, HttpFiberRpc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
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

    /// Test-only HTTP address exposing GET /status and POST /shutdown.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_CONTROL_ADDR")]
    control_addr: Option<String>,

    /// Polling interval in milliseconds.
    #[arg(long, default_value_t = 200, env = "FIBER_LSP_SDK_AGENT_INTERVAL_MS")]
    interval_ms: u64,

    /// Bind the current pending signer to this channel before polling.
    #[arg(long, env = "FIBER_LSP_SDK_AGENT_BIND_CHANNEL_ID")]
    bind_channel_id: Option<String>,

    /// Run one poll cycle and exit.
    #[arg(long)]
    once: bool,
}

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
    agent.initialize().await?;
    if let Some(channel_id) = args.bind_channel_id {
        agent.bind(channel_id.parse()?).await?;
    }

    info!(tenant_id = %agent.tenant_id(), "hosted LSP SDK agent ready");
    if args.once {
        agent.poll_once().await?;
        return Ok(());
    }

    let (control, mut shutdown_rx) = match (args.control_addr, status_file) {
        (Some(address), Some(status_file)) => {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            (
                Some(tokio::spawn(run_control_server(
                    address,
                    status_file,
                    shutdown_tx,
                ))),
                Some(shutdown_rx),
            )
        }
        (Some(_), None) => anyhow::bail!("--control-addr requires --status-file"),
        (None, _) => (None, None),
    };
    let interval = Duration::from_millis(args.interval_ms);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = wait_for_shutdown(&mut shutdown_rx) => break,
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
    shutdown: oneshot::Sender<()>,
) -> Result<()> {
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind test control server at {address}"))?;
    info!(%address, "test control server ready");
    let mut shutdown = Some(shutdown);
    loop {
        let (stream, _) = listener.accept().await?;
        if handle_control_connection(stream, &status_file, &mut shutdown).await? {
            return Ok(());
        }
    }
}

async fn handle_control_connection(
    mut stream: TcpStream,
    status_file: &PathBuf,
    shutdown: &mut Option<oneshot::Sender<()>>,
) -> Result<bool> {
    let mut request = [0u8; 4096];
    let size = stream.read(&mut request).await?;
    let request = String::from_utf8_lossy(&request[..size]);
    let (status, body, should_shutdown) = if request.starts_with("GET /status ") {
        match tokio::fs::read(status_file).await {
            Ok(body) => ("200 OK", body, false),
            Err(error) => (
                "503 Service Unavailable",
                format!("{{\"error\":\"status unavailable: {error}\"}}").into_bytes(),
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
