//! Tenant SSE transport. Wakeups are coalesced; reconnects replay persisted pending work.

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use fiber_json_types::{GetChannelSigningStatusResult, LspEvent, LspEventEnvelope};
use futures::stream;
use http_body_util::StreamBody;
use hyper::{body::Frame, header::AUTHORIZATION, Method, Request, Response, StatusCode};
use jsonrpsee::server::{HttpBody, StopHandle};
use parking_lot::Mutex;
use ractor::ActorRef;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::fiber::channel::ChannelActorStateStore;
use crate::lsp::{LspServiceMessage, TenantEventContext, TenantId};
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;

use super::biscuit::{extract_optional_node_id, extract_tenant_id, BiscuitAuth};

const SESSION_HEADER: &str = "x-lsp-session-id";

#[derive(Clone, Copy)]
pub(super) struct EventLimits {
    pub lifetime: Duration,
    pub heartbeat: Duration,
    pub write_timeout: Duration,
    pub total: usize,
}

impl Default for EventLimits {
    fn default() -> Self {
        Self {
            lifetime: Duration::from_secs(300),
            heartbeat: Duration::from_secs(25),
            write_timeout: Duration::from_secs(5),
            total: 16_384,
        }
    }
}

#[derive(Default)]
struct Connections(Mutex<ConnectionState>);

#[derive(Default)]
struct ConnectionState {
    // Keep the current session after disconnect so a replaced device cannot
    // reclaim it through an ordinary reconnect while the new device is offline.
    sessions: HashMap<TenantId, TenantSession>,
    total: usize,
}

struct TenantSession {
    id: String,
    active: Option<Arc<CancellationToken>>,
}

#[derive(Debug)]
enum AdmissionError {
    Replaced,
    Full,
}

impl Connections {
    fn acquire(
        self: &Arc<Self>,
        tenant: TenantId,
        requested_session: Option<&str>,
        limits: EventLimits,
    ) -> Result<Connection, AdmissionError> {
        let mut state = self.0.lock();
        let current = state.sessions.get(&tenant);
        if let (Some(requested), Some(current)) = (requested_session, current) {
            if requested != current.id {
                return Err(AdmissionError::Replaced);
            }
        }
        let replacing = current.is_some_and(|session| session.active.is_some());
        if !replacing && state.total >= limits.total {
            return Err(AdmissionError::Full);
        }
        // Without a session header this is an explicit takeover. After a server
        // restart, the first authenticated resume also restores its session.
        let session_id = requested_session
            .map(str::to_owned)
            .unwrap_or_else(|| hex::encode(rand::random::<[u8; 16]>()));
        let cancellation = Arc::new(CancellationToken::new());
        if let Some(previous) = state.sessions.insert(
            tenant.clone(),
            TenantSession {
                id: session_id.clone(),
                active: Some(cancellation.clone()),
            },
        ) {
            if let Some(active) = previous.active {
                active.cancel();
            }
        }
        if !replacing {
            state.total += 1;
        }
        Ok(Connection {
            connections: self.clone(),
            tenant,
            session_id,
            cancellation,
        })
    }
}

struct Connection {
    connections: Arc<Connections>,
    tenant: TenantId,
    session_id: String,
    cancellation: Arc<CancellationToken>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let mut state = self.connections.0.lock();
        if let Some(session) = state.sessions.get_mut(&self.tenant) {
            if session
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &self.cancellation))
            {
                session.active = None;
                state.total -= 1;
            }
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Permissions {
    channels: bool,
    watchtower: bool,
}

/// Shared by every HTTP connection on this RPC listener.
pub(super) struct LspEvents {
    actor: ActorRef<LspServiceMessage>,
    connections: Arc<Connections>,
    limits: EventLimits,
    channels: bool,
    watchtower: bool,
}

impl LspEvents {
    pub(super) fn new(
        actor: ActorRef<LspServiceMessage>,
        channels: bool,
        watchtower: bool,
    ) -> Self {
        Self {
            actor,
            connections: Arc::default(),
            limits: EventLimits::default(),
            channels,
            watchtower: cfg!(feature = "watchtower") && watchtower,
        }
    }

    fn authorize(
        &self,
        auth: &BiscuitAuth,
        token: &str,
    ) -> Result<(TenantId, Permissions, Option<fiber_types::NodeId>)> {
        let channel = auth.check_permission("get_channel_signing_status", token);
        let watchtower = auth.check_permission("get_watchtower_signing_status", token);
        let mut permissions = Permissions {
            channels: self.channels && channel.is_ok(),
            watchtower: self.watchtower && watchtower.is_ok(),
        };
        let biscuit = match (&channel, &watchtower) {
            (Ok((biscuit, _)), _) if permissions.channels => biscuit,
            (_, Ok((biscuit, _))) if permissions.watchtower => biscuit,
            _ => return Err(anyhow!("signing read permission required")),
        };
        let tenant = extract_tenant_id(biscuit)?.ok_or_else(|| anyhow!("tenant token required"))?;
        // Enforce the same watchtower identity boundary as the existing RPCs.
        let node_id = if permissions.watchtower {
            extract_optional_node_id(biscuit)?.filter(|node| node != &fiber_types::NodeId::local())
        } else {
            None
        };
        // The watchtower rule also accepts read("channels"), but its RPC still
        // requires a node fact. Preserve channel access when that fact is absent.
        permissions.watchtower &= node_id.is_some();
        anyhow::ensure!(
            permissions.channels || permissions.watchtower,
            "signing read permission required"
        );
        Ok((tenant, permissions, node_id))
    }

    pub(super) async fn serve<B>(
        &self,
        req: Request<B>,
        auth: Arc<BiscuitAuth>,
        stop: StopHandle,
    ) -> Response<HttpBody> {
        if req.method() != Method::GET {
            let mut response = response(StatusCode::METHOD_NOT_ALLOWED, "GET required");
            response
                .headers_mut()
                .insert("allow", "GET".parse().unwrap());
            return response;
        }
        if req.uri().query().is_some() {
            return response(
                StatusCode::BAD_REQUEST,
                "query parameters are not supported",
            );
        }
        let Some(token) = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_owned)
        else {
            return response(StatusCode::UNAUTHORIZED, "tenant bearer token required");
        };
        let Ok((tenant, permissions, node_id)) = self.authorize(&auth, &token) else {
            return response(
                StatusCode::FORBIDDEN,
                "invalid token or signing read permission",
            );
        };
        let requested_session = match req.headers().get(SESSION_HEADER) {
            None => None,
            Some(value) => match value.to_str() {
                Ok(id) if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
                    Some(id)
                }
                _ => return response(StatusCode::BAD_REQUEST, "invalid LSP session ID"),
            },
        };
        let context = match ractor::call_t!(
            self.actor,
            LspServiceMessage::GetTenantEventContext,
            5_000,
            tenant.clone()
        ) {
            Ok(Ok(context)) => context,
            Ok(Err(_)) => return response(StatusCode::FORBIDDEN, "tenant is not registered"),
            Err(_) => return response(StatusCode::SERVICE_UNAVAILABLE, "LSP service unavailable"),
        };
        if node_id
            .as_ref()
            .is_some_and(|node| node != &context.node_id)
        {
            return response(
                StatusCode::FORBIDDEN,
                "token node does not match registered tenant",
            );
        }
        // Resolve and verify the tenant before allowing this request to evict
        // its current stream. A failed request must leave that stream intact.
        let connection = match self
            .connections
            .acquire(tenant, requested_session, self.limits)
        {
            Ok(connection) => connection,
            Err(AdmissionError::Replaced) => {
                return response(StatusCode::CONFLICT, "session_replaced");
            }
            Err(AdmissionError::Full) => {
                let mut response = response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "event connection limit reached",
                );
                response
                    .headers_mut()
                    .insert("retry-after", "5".parse().unwrap());
                return response;
            }
        };
        let session_id = connection.session_id.clone();
        let producer_cancellation = connection.cancellation.clone();
        // Register before reading the initial snapshot, closing the snapshot/listen race.
        let mut changes = context.events.subscribe(&context.node_id);
        let (sender, receiver) = mpsc::channel::<String>(4);
        let limits = self.limits;
        tokio::spawn(async move {
            let run = async {
                let mut heartbeat = tokio::time::interval(limits.heartbeat);
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                heartbeat.tick().await;
                send(&sender, "retry: 1000\n: connected\n\n".to_owned(), limits).await?;
                let mut previous = HashMap::new();
                validate_permissions(&auth, &token, permissions)?;
                send_pending(&sender, &context, permissions, &mut previous, limits).await?;
                send_event(&sender, LspEvent::SnapshotComplete, limits).await?;
                while !sender.is_closed() {
                    tokio::select! {
                        _ = heartbeat.tick() => {
                            validate_permissions(&auth, &token, permissions)?;
                            send(&sender, ": heartbeat\n\n".to_owned(), limits).await?;
                            continue;
                        }
                        _ = changes.changed() => {}
                    }
                    validate_permissions(&auth, &token, permissions)?;
                    send_pending(&sender, &context, permissions, &mut previous, limits).await?;
                }
                Ok::<(), anyhow::Error>(())
            };
            // The lifetime and disconnect paths also cancel a blocked writer.
            tokio::select! {
                _ = producer_cancellation.cancelled() => {}
                _ = tokio::time::sleep(limits.lifetime) => {}
                _ = sender.closed() => {}
                _ = stop.shutdown() => {}
                result = run => {
                    if let Err(error) = result {
                        tracing::debug!("tenant event stream closed: {error}");
                    }
                }
            }
        });
        // On replacement, discard queued frames as well as cancelling the
        // producer. Dropping an old body must not release the new stream's slot.
        let frames = stream::unfold((receiver, connection), |(mut receiver, connection)| async {
            let data = tokio::select! {
                biased;
                _ = connection.cancellation.cancelled() => None,
                data = receiver.recv() => data,
            };
            data.map(|data| {
                (
                    Ok::<_, Infallible>(Frame::data(data.into())),
                    (receiver, connection),
                )
            })
        });
        let mut result = Response::new(HttpBody::new(StreamBody::new(frames)));
        let headers = result.headers_mut();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("cache-control", "no-cache, no-transform".parse().unwrap());
        headers.insert("x-accel-buffering", "no".parse().unwrap());
        headers.insert(SESSION_HEADER, session_id.parse().unwrap());
        result
    }
}

fn validate_permissions(auth: &BiscuitAuth, token: &str, permissions: Permissions) -> Result<()> {
    // Rechecking time caveats on heartbeats bounds the lifetime of an expired token,
    // and checking before a snapshot prevents new events being sent after expiration.
    if permissions.channels {
        auth.check_permission("get_channel_signing_status", token)?;
    }
    if permissions.watchtower {
        auth.check_permission("get_watchtower_signing_status", token)?;
    }
    Ok(())
}

async fn send(sender: &mpsc::Sender<String>, data: String, limits: EventLimits) -> Result<()> {
    tokio::time::timeout(limits.write_timeout, sender.send(data)).await??;
    Ok(())
}

async fn send_event(
    sender: &mpsc::Sender<String>,
    event: LspEvent,
    limits: EventLimits,
) -> Result<()> {
    let name = event.event_name();
    let data = serde_json::to_string(&LspEventEnvelope { version: 1, event })?;
    send(sender, format!("event: {name}\ndata: {data}\n\n"), limits).await
}

async fn send_pending(
    sender: &mpsc::Sender<String>,
    context: &TenantEventContext,
    permissions: Permissions,
    previous: &mut HashMap<String, String>,
    limits: EventLimits,
) -> Result<()> {
    let mut current = HashMap::new();
    for state in context.store.get_all_channel_states() {
        let channel_id = state.get_id();
        let mut events = Vec::new();
        if permissions.channels {
            let status =
                super::channel::to_rpc_channel_signing_status(&state).map_err(|e| anyhow!(e))?;
            if matches!(
                status,
                fiber_json_types::ChannelSigningStatus::SignatureRequired { .. }
            ) {
                events.push(LspEvent::ChannelSignatureRequired(Box::new(
                    GetChannelSigningStatusResult {
                        channel_id: channel_id.into(),
                        status,
                    },
                )));
            }
        }
        #[cfg(feature = "watchtower")]
        if permissions.watchtower {
            let status = super::watchtower::to_rpc_watchtower_signing_status(
                context
                    .watchtower_store
                    .get_watchtower_signer(&context.node_id, &channel_id),
            );
            if matches!(
                status,
                fiber_json_types::WatchtowerSigningStatus::SignatureRequired { .. }
            ) {
                events.push(LspEvent::WatchtowerSignatureRequired(Box::new(
                    fiber_json_types::GetWatchtowerSigningStatusResult {
                        channel_id: channel_id.into(),
                        status,
                    },
                )));
            }
        }
        for event in events {
            let key = format!("{}:{channel_id:?}", event.event_name());
            let data = serde_json::to_string(&event)?;
            if previous.get(&key) != Some(&data) {
                send_event(sender, event, limits).await?;
            }
            current.insert(key, data);
        }
    }
    *previous = current;
    Ok(())
}

pub(super) fn response(status: StatusCode, message: &'static str) -> Response<HttpBody> {
    let mut response = Response::new(HttpBody::from(message));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use biscuit_auth::{macros::biscuit, Biscuit, KeyPair};
    use http_body_util::BodyExt;
    use ractor::{Actor, ActorProcessingErr};

    use crate::store::{open_store, NodeNamespace, Store};

    struct ContextActor;

    #[async_trait::async_trait]
    impl Actor for ContextActor {
        type Msg = LspServiceMessage;
        type State = Store;
        type Arguments = Store;

        async fn pre_start(
            &self,
            _: ActorRef<Self::Msg>,
            store: Store,
        ) -> Result<Store, ActorProcessingErr> {
            Ok(store)
        }

        async fn handle(
            &self,
            _: ActorRef<Self::Msg>,
            message: Self::Msg,
            store: &mut Store,
        ) -> Result<(), ActorProcessingErr> {
            if let LspServiceMessage::GetTenantEventContext(tenant, reply) = message {
                let _ = reply.send(Ok(TenantEventContext {
                    events: Arc::default(),
                    store: store.namespaced(NodeNamespace::hosted_tenant(tenant.as_str())),
                    watchtower_store: store.clone(),
                    node_id: fiber_types::NodeId::from_bytes(vec![2; 33]),
                }));
            }
            Ok(())
        }
    }

    struct Fixture {
        events: LspEvents,
        auth: Arc<BiscuitAuth>,
        root: KeyPair,
        stop: StopHandle,
        _server: jsonrpsee::server::ServerHandle,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        async fn new(limits: EventLimits) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = open_store(dir.path()).unwrap();
            let actor = Actor::spawn(None, ContextActor, store).await.unwrap().0;
            let root = KeyPair::new();
            let auth = Arc::new(BiscuitAuth::from_pubkey(root.public().to_string()).unwrap());
            let mut events = LspEvents::new(actor, true, true);
            events.limits = limits;
            let (stop, server) = jsonrpsee::server::stop_channel();
            Self {
                events,
                auth,
                root,
                stop,
                _server: server,
                _dir: dir,
            }
        }

        fn token(&self, facts: &str) -> String {
            Biscuit::builder()
                .code(facts)
                .unwrap()
                .build(&self.root)
                .unwrap()
                .to_base64()
                .unwrap()
        }

        async fn connect(&self, token: &str, session: Option<&str>) -> Response<HttpBody> {
            let mut request = Request::builder()
                .uri("/lsp/events")
                .header(AUTHORIZATION, format!("Bearer {token}"));
            if let Some(id) = session {
                request = request.header(SESSION_HEADER, id);
            }
            self.events
                .serve(
                    request.body(()).unwrap(),
                    self.auth.clone(),
                    self.stop.clone(),
                )
                .await
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.events.actor.stop(None);
        }
    }

    #[tokio::test]
    async fn authentication_and_invalid_requests_do_not_evict_the_current_stream() {
        let f = Fixture::new(EventLimits::default()).await;
        assert_eq!(
            f.events
                .serve(Request::new(()), f.auth.clone(), f.stop.clone())
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let credential = f.token(r#"tenant("alice"); read("channels");"#);
        let mut current = f.connect(&credential, None).await;
        assert_eq!(current.status(), StatusCode::OK);
        assert_eq!(
            f.connect("invalid", None).await.status(),
            StatusCode::FORBIDDEN
        );
        for facts in [
            r#"read("channels");"#,
            r#"tenant("alice"); write("channels");"#,
            r#"tenant("alice"); read("watchtower");"#,
        ] {
            assert_eq!(
                f.connect(&f.token(facts), None).await.status(),
                StatusCode::FORBIDDEN,
                "{facts}"
            );
        }
        for (session, status) in [
            ("invalid", StatusCode::BAD_REQUEST),
            ("ffffffffffffffffffffffffffffffff", StatusCode::CONFLICT),
        ] {
            assert_eq!(f.connect(&credential, Some(session)).await.status(), status);
        }
        #[cfg(feature = "watchtower")]
        {
            let node = fiber_types::NodeId::from_bytes(vec![3; 33]);
            let wrong_node = f.token(&format!(
                r#"tenant("alice"); node("{node}"); read("channels");"#
            ));
            assert_eq!(
                f.connect(&wrong_node, None).await.status(),
                StatusCode::FORBIDDEN
            );
        }
        let query = Request::builder()
            .uri("/lsp/events?tenant=bob")
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .body(())
            .unwrap();
        assert_eq!(
            f.events
                .serve(query, f.auth.clone(), f.stop.clone())
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        tokio::time::timeout(Duration::from_secs(1), current.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn limits_heartbeat_and_expiry_preserve_the_current_session() {
        let f = Fixture::new(EventLimits {
            heartbeat: Duration::from_millis(10),
            lifetime: Duration::from_millis(150),
            total: 2,
            ..Default::default()
        })
        .await;
        let alice = f.token(r#"tenant("alice"); read("channels");"#);
        let first = f.connect(&alice, None).await;
        let bob = f
            .connect(&f.token(r#"tenant("bob"); read("channels");"#), None)
            .await;
        assert_eq!(bob.status(), StatusCode::OK);
        let rejected = f
            .connect(&f.token(r#"tenant("charlie"); read("channels");"#), None)
            .await;
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejected.headers()["retry-after"], "5");
        // The tenant can replace its own stream even at the global limit.
        let replacement = f.connect(&alice, None).await;
        assert_eq!(replacement.status(), StatusCode::OK);
        let session = replacement.headers()[SESSION_HEADER]
            .to_str()
            .unwrap()
            .to_owned();
        let discarded = tokio::time::timeout(Duration::from_secs(1), first.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(discarded.is_empty());
        assert_eq!(f.events.connections.0.lock().total, 2);
        drop(bob);
        assert_eq!(f.events.connections.0.lock().total, 1);
        let data = tokio::time::timeout(Duration::from_secs(1), replacement.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        let text = std::str::from_utf8(&data).unwrap();
        assert!(text.contains(": heartbeat\n\n"));
        assert!(text.contains("lsp.snapshot_complete"));
        assert_eq!(f.events.connections.0.lock().total, 0);
        let resumed = f.connect(&alice, Some(&session)).await;
        assert_eq!(resumed.status(), StatusCode::OK);
        assert_eq!(resumed.headers()[SESSION_HEADER], session);
    }

    #[tokio::test]
    async fn a_slow_consumer_cannot_block_the_stream_forever() {
        let (sender, _receiver) = mpsc::channel(1);
        let limits = EventLimits {
            write_timeout: Duration::from_millis(10),
            ..Default::default()
        };
        send(&sender, "first".into(), limits).await.unwrap();
        assert!(send(&sender, "second".into(), limits).await.is_err());
    }

    #[tokio::test]
    async fn token_expiration_closes_an_existing_stream() {
        let f = Fixture::new(EventLimits {
            heartbeat: Duration::from_millis(20),
            ..Default::default()
        })
        .await;
        let expires = std::time::SystemTime::now() + Duration::from_secs(2);
        let credential = biscuit!(
            r#"tenant("alice"); read("channels"); check if time($time), $time < {expires};"#,
            expires = expires
        )
        .build(&f.root)
        .unwrap()
        .to_base64()
        .unwrap();
        let response = f.connect(&credential, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(std::str::from_utf8(&data)
            .unwrap()
            .contains("lsp.snapshot_complete"));
        assert_eq!(f.events.connections.0.lock().total, 0);
        assert_eq!(
            f.connect(&credential, None).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[cfg(feature = "watchtower")]
    #[tokio::test]
    async fn watchtower_snapshot_is_scoped_to_the_authenticated_node() {
        use crate::{fiber::channel::ChannelActorState, store::sample::StoreSample};
        use fiber_types::{
            Hash256, NodeId, OnchainKeyPurpose, OnchainSigningContent,
            WatchtowerExternalSignerState, WatchtowerSignerState,
        };

        let f = Fixture::new(EventLimits {
            lifetime: Duration::from_millis(50),
            ..Default::default()
        })
        .await;
        let context = ractor::call!(
            f.events.actor,
            LspServiceMessage::GetTenantEventContext,
            TenantId::new("alice").unwrap()
        )
        .unwrap()
        .unwrap();
        let channel = ChannelActorState::samples(1).remove(0);
        let id = channel.get_id();
        context.store.insert_channel_actor_state(channel);
        for (node, request) in [
            (context.node_id.clone(), [4; 32]),
            (NodeId::from_bytes(vec![3; 33]), [5; 32]),
        ] {
            let mut state = WatchtowerExternalSignerState::default();
            state.pending_requests.insert(
                Hash256::from(request),
                OnchainSigningContent {
                    key_purpose: OnchainKeyPurpose::Settlement,
                    transaction: Default::default(),
                },
            );
            context.watchtower_store.put_watchtower_signer(
                &node,
                &id,
                WatchtowerSignerState::External(state),
            );
        }
        let node = context.node_id;
        let credential = f.token(&format!(
            r#"tenant("alice"); node("{node}"); read("watchtower");"#
        ));
        let response = f.connect(&credential, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = tokio::time::timeout(Duration::from_secs(1), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        let text = std::str::from_utf8(&data).unwrap();
        assert!(text.contains("lsp.watchtower.signature_required"));
        assert!(!text.contains("lsp.channel.signature_required"));
        assert!(text.contains(&hex::encode([4; 32])));
        assert!(!text.contains(&hex::encode([5; 32])));
    }
}
