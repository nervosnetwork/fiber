//! CLI entry points for the liquidity-lock mutator.
//!
//! - stdin mode (default): read one JSON request from stdin and print one
//!   JSON response to stdout.
//! - `--serve <port>`: run an HTTP sidecar on 127.0.0.1:<port>; the Bruno
//!   suites POST mutation requests to `/` and read the JSON response, then
//!   submit the returned signed transaction through the CKB JSON-RPC.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use ckb_types::packed;
use liquidity_lock_mutator::{
    handle_request, parse_request_payload, MutatorRequest, MutatorResponse,
};
use secp256k1::SecretKey;

struct CliOptions {
    serve_port: Option<u16>,
    rpc_url: Option<String>,
    privkey_hex: Option<String>,
    privkey_path: Option<String>,
}

fn parse_args() -> Result<CliOptions> {
    let mut options = CliOptions {
        serve_port: None,
        rpc_url: None,
        privkey_hex: None,
        privkey_path: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--serve" => {
                let port = args.next().context("--serve requires a port")?;
                options.serve_port = Some(port.parse().context("invalid port")?);
            }
            "--rpc-url" => {
                options.rpc_url = Some(args.next().context("--rpc-url requires a value")?);
            }
            "--privkey" => {
                options.privkey_hex = Some(args.next().context("--privkey requires a value")?);
            }
            "--privkey-path" => {
                options.privkey_path =
                    Some(args.next().context("--privkey-path requires a value")?);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(options)
}

fn resolve_privkey(options: &CliOptions, request: &MutatorRequest) -> Result<SecretKey> {
    if let Some(hex_value) = &options.privkey_hex {
        return liquidity_lock_mutator::parse_secret_key(hex_value);
    }
    if let Some(path) = &options.privkey_path {
        return liquidity_lock_mutator::parse_secret_key_file(path);
    }
    if let Some(hex_value) = &request.privkey {
        return liquidity_lock_mutator::parse_secret_key(hex_value);
    }
    bail!("no privkey: pass --privkey/--privkey-path or a privkey field in the request")
}

fn process(
    payload: &str,
    options: &CliOptions,
    locked: &mut HashSet<packed::OutPoint>,
) -> Result<MutatorResponse> {
    let request = parse_request_payload(payload)?;
    let rpc_url = options
        .rpc_url
        .clone()
        .or_else(|| request.rpc_url.clone())
        .context("no rpc url: pass --rpc-url or an rpc_url field in the request")?;
    let privkey = resolve_privkey(options, &request)?;
    let (response, used) = handle_request(&request, &rpc_url, &privkey, locked)?;
    locked.extend(used);
    Ok(response)
}

fn run_stdin(options: &CliOptions) -> Result<()> {
    let mut payload = String::new();
    std::io::stdin()
        .read_to_string(&mut payload)
        .context("read request from stdin")?;
    let mut locked = HashSet::new();
    let response = process(&payload, options, &mut locked)?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

fn respond(stream: &mut std::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Result<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            bail!("connection closed before the request headers completed");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            header_end = position;
            break;
        }
        if buffer.len() > 1 << 20 {
            bail!("request headers exceed 1 MiB");
        }
    }
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().context("invalid content-length")?;
        }
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            bail!("connection closed before the request body completed");
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() > (1 << 20) + content_length {
            bail!("request body exceeds content-length by more than 1 MiB");
        }
    }
    body.truncate(content_length);
    Ok((request_line, String::from_utf8_lossy(&body).to_string()))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn run_serve(port: u16, options: Arc<CliOptions>) -> Result<()> {
    let locked = Arc::new(Mutex::new(HashSet::new()));
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("bind 127.0.0.1:{port}"))?;
    eprintln!("liquidity-lock-mutator listening on 127.0.0.1:{port}");
    for incoming in listener.incoming() {
        let mut stream = incoming.context("accept connection")?;
        let locked = Arc::clone(&locked);
        let options = Arc::clone(&options);
        std::thread::spawn(move || {
            let (request_line, body) = match read_http_request(&mut stream) {
                Ok(value) => value,
                Err(error) => {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        &format!("{{\"error\":\"{error}\"}}"),
                    );
                    return;
                }
            };
            let method = request_line.split(' ').next().unwrap_or_default();
            if method == "GET" {
                // Readiness probe used by the suite runner.
                respond(&mut stream, "200 OK", "{\"ok\":true}");
                return;
            }
            if method != "POST" {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "{\"error\":\"POST only\"}",
                );
                return;
            }
            let mut locked = locked.lock().expect("lock outpoint set");
            match process(&body, &options, &mut locked) {
                Ok(response) => {
                    let body = serde_json::to_string(&response)
                        .unwrap_or_else(|_| "{\"error\":\"response serialization failed\"}".into());
                    respond(&mut stream, "200 OK", &body);
                }
                Err(error) => {
                    eprintln!("mutation request failed: {error:#}");
                    let message = format!("{error:#}")
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"");
                    respond(
                        &mut stream,
                        "200 OK",
                        &format!("{{\"error\":\"{message}\"}}"),
                    );
                }
            }
        });
    }
    Ok(())
}

fn main() -> Result<()> {
    let options = Arc::new(parse_args()?);
    match options.serve_port {
        Some(port) => run_serve(port, options),
        None => run_stdin(&options),
    }
}
