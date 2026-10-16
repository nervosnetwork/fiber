//! SSE transport with bounded buffering and reconnect/snapshot recovery.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use fiber_json_types::{LspEvent, LspEventEnvelope};
use tokio::sync::{mpsc, oneshot};

const MAX_EVENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const SESSION_HEADER: &str = "x-lsp-session-id";

enum StreamEnd {
    Reconnect,
    Replaced,
    TokenRejected,
}

/// One event is acknowledged after verification and signature submission.
pub struct EventDelivery {
    /// Typed request or snapshot boundary from the authenticated stream.
    pub event: LspEvent,
    /// Send false on failure to reconnect and recover outstanding requests.
    pub processed: oneshot::Sender<bool>,
}

/// A new stream takes over the tenant's signer session. Reconnects resume it;
/// replacement by another stream stops automatic retries.
/// Dropping this stream cancels the request and any pending reconnect.
pub struct EventStream {
    receiver: mpsc::Receiver<EventDelivery>,
    task: tokio::task::JoinHandle<()>,
}

impl EventStream {
    pub(crate) fn connect(rpc_url: &str, token: String) -> Result<Self> {
        let mut url = reqwest::Url::parse(rpc_url)?;
        url.set_path("/lsp/events");
        url.set_query(None);
        url.set_fragment(None);
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        let (sender, receiver) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            let mut session_id = None;
            loop {
                let started = Instant::now();
                let mut retry_after = Duration::ZERO;
                let result = receive(
                    &client,
                    &url,
                    &token,
                    &sender,
                    &mut retry_after,
                    &mut session_id,
                )
                .await;
                if sender.is_closed() {
                    break;
                }
                match result {
                    Ok(StreamEnd::Replaced) => {
                        tracing::warn!(
                            "LSP signer session replaced by another device; stopping event reader"
                        );
                        break;
                    }
                    Ok(StreamEnd::TokenRejected) => {
                        tracing::warn!("LSP tenant token rejected; stopping event reader");
                        break;
                    }
                    Err(error) => {
                        tracing::warn!("LSP event stream disconnected; reconnecting: {error:#}");
                    }
                    Ok(StreamEnd::Reconnect) => {}
                }
                // A briefly accepted connection must not reset a failing retry loop.
                if started.elapsed() >= Duration::from_secs(30) {
                    backoff = Duration::from_millis(500);
                }
                let delay = reconnect_delay(backoff, retry_after, rand::random());
                tokio::select! {
                    _ = sender.closed() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });
        Ok(Self { receiver, task })
    }

    /// Wait for the next tenant event, including pending work replayed after reconnect.
    pub async fn next(&mut self) -> Option<EventDelivery> {
        self.receiver.recv().await
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn reconnect_delay(backoff: Duration, retry_after: Duration, random: u64) -> Duration {
    let half = (backoff.as_millis() as u64 / 2).max(1);
    retry_after + Duration::from_millis(half + random % (half + 1))
}

async fn receive(
    client: &reqwest::Client,
    url: &reqwest::Url,
    token: &str,
    sender: &mpsc::Sender<EventDelivery>,
    retry_after: &mut Duration,
    session_id: &mut Option<String>,
) -> Result<StreamEnd> {
    let mut request = client
        .get(url.clone())
        .bearer_auth(token)
        .header("accept", "text/event-stream");
    if let Some(id) = session_id.as_ref() {
        request = request.header(SESSION_HEADER, id);
    }
    let mut response = request.send().await?;
    match response.status() {
        reqwest::StatusCode::CONFLICT => return Ok(StreamEnd::Replaced),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            return Ok(StreamEnd::TokenRejected);
        }
        _ => {}
    }
    if let Some(seconds) = response
        .headers()
        .get("retry-after")
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.parse::<u64>().ok())
    {
        *retry_after = Duration::from_secs(seconds.min(300));
    }
    if !response.status().is_success() {
        bail!("HTTP {} from tenant event endpoint", response.status());
    }
    anyhow::ensure!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(';').next() == Some("text/event-stream")),
        "expected text/event-stream"
    );
    let received_session = response
        .headers()
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("missing or invalid LSP session ID")?;
    anyhow::ensure!(
        session_id.as_ref().is_none_or(|id| id == received_session),
        "LSP session changed during resume"
    );
    *session_id = Some(received_session.to_owned());
    let mut decoder = Decoder::default();
    while let Some(chunk) = response.chunk().await? {
        decoder.push(&chunk)?;
        while let Some(data) = decoder.next_data()? {
            let value: serde_json::Value =
                serde_json::from_str(&data).context("invalid SSE JSON")?;
            // Future tenant event types can share the stream without breaking this signer.
            if !matches!(
                value["type"].as_str(),
                Some(
                    "lsp.channel.signature_required"
                        | "lsp.watchtower.signature_required"
                        | "lsp.snapshot_complete"
                )
            ) {
                continue;
            }
            let envelope: LspEventEnvelope = serde_json::from_value(value)?;
            anyhow::ensure!(envelope.version == 1, "unsupported LSP event version");
            let (processed, reply) = oneshot::channel();
            sender
                .send(EventDelivery {
                    event: envelope.event,
                    processed,
                })
                .await?;
            if !reply.await.unwrap_or(false) {
                bail!("signer could not process event; replay pending work on reconnect");
            }
        }
    }
    Ok(StreamEnd::Reconnect)
}

#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<()> {
        anyhow::ensure!(
            self.buffer.len() + chunk.len() <= MAX_EVENT_BYTES,
            "SSE frame too large"
        );
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }

    fn next_data(&mut self) -> Result<Option<String>> {
        loop {
            let Some((end, separator)) = self
                .buffer
                .windows(2)
                .position(|bytes| bytes == b"\n\n")
                .map(|end| (end, 2))
                .into_iter()
                .chain(
                    self.buffer
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .map(|end| (end, 4)),
                )
                .min_by_key(|(end, _)| *end)
            else {
                return Ok(None);
            };
            let frame: Vec<_> = self.buffer.drain(..end + separator).collect();
            let text = std::str::from_utf8(&frame)?;
            let data: Vec<_> = text
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .map(|data| data.strip_prefix(' ').unwrap_or(data))
                })
                .collect();
            if !data.is_empty() {
                return Ok(Some(data.join("\n")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_sse_heartbeats_crlf_and_multiline_data() {
        let mut decoder = Decoder::default();
        decoder
            .push(b": heartbeat\r\n\r\nevent: lsp.test\r\ndata: {\r\nda")
            .unwrap();
        assert!(decoder.next_data().unwrap().is_none());
        decoder
            .push("ta: \"name\":\"签名\"}\r\n\r\ndata: second\n\n".as_bytes())
            .unwrap();
        assert_eq!(
            decoder.next_data().unwrap().unwrap(),
            "{\n\"name\":\"签名\"}"
        );
        assert_eq!(decoder.next_data().unwrap().unwrap(), "second");
        assert!(decoder.next_data().unwrap().is_none());
    }

    #[test]
    fn oversized_sse_frames_are_rejected() {
        let mut decoder = Decoder::default();
        assert!(decoder.push(&vec![b'x'; MAX_EVENT_BYTES + 1]).is_err());
    }

    #[tokio::test]
    async fn reconnects_after_limits_processing_failure_and_eof_with_same_tenant_token() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("GET /lsp/events HTTP/1.1\r\n"));
                assert!(request
                    .to_lowercase()
                    .contains("authorization: bearer tenant-token\r\n"));
                assert_eq!(
                    request
                        .to_lowercase()
                        .contains("x-lsp-session-id: 0123456789abcdef0123456789abcdef\r\n"),
                    attempt >= 2,
                    "only the first accepted connection may take over the session"
                );
                let response = if attempt == 0 {
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                } else {
                    let data = "event: lsp.future\ndata: {\"version\":1,\"type\":\"lsp.future\"}\n\n\
                                event: lsp.snapshot_complete\ndata: {\"version\":1,\"type\":\"lsp.snapshot_complete\"}\n\n";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Lsp-Session-Id: 0123456789abcdef0123456789abcdef\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len())
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut events =
            EventStream::connect(&format!("http://{address}"), "tenant-token".into()).unwrap();
        for processed in [false, true, true] {
            let delivery = tokio::time::timeout(Duration::from_secs(5), events.next())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(delivery.event, LspEvent::SnapshotComplete));
            delivery.processed.send(processed).unwrap();
        }
        drop(events);
        server.await.unwrap();
    }
}
