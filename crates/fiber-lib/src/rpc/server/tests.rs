use std::net::SocketAddr;

use anyhow::Result;
use jsonrpsee::server::{stop_channel, ServerHandle};
use jsonrpsee::Methods;
use tokio::net::TcpListener;

use super::{spawn_listener, ListenerConfig};
use crate::rpc::biscuit::BiscuitAuth;
use std::sync::Arc;

use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use biscuit_auth::KeyPair;
use fiber_json_types::SubscriptionEvent;
use futures::{SinkExt, StreamExt};
use hyper::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use jsonrpsee::{
    core::client::{ClientT, SubscriptionClientT},
    http_client::HttpClientBuilder,
    rpc_params,
    ws_client::WsClientBuilder,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

use crate::{
    fiber::channel::{ChannelActorState, ChannelActorStateStore},
    store::{open_store, sample::StoreSample, Store},
    tests::test_utils::{gen_rpc_config, get_test_root_actor},
};

use super::{is_public_addr, start_rpc, RpcConfig, RpcServer};

async fn start(
    events: crate::signing::SigningEvents,
    config: RpcConfig,
    store: Store,
) -> anyhow::Result<RpcServer> {
    start_rpc(
        config,
        None,
        None,
        None,
        None,
        None,
        store,
        None,
        None,
        get_test_root_actor().await.get_cell(),
        None,
        events,
        #[cfg(debug_assertions)]
        None,
        #[cfg(debug_assertions)]
        None,
    )
    .await
}

async fn browser_message(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // Browsers handle ping/pong internally and expose only data messages.
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn browser_websocket_authenticates_without_echoing_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let events = crate::signing::SigningEvents::default();
    let root = KeyPair::new();
    let biscuit = biscuit_auth::Biscuit::builder()
        .code(r#"write("subscriptions"); read("channels");"#)
        .unwrap()
        .build(&root)
        .unwrap();
    let token = URL_SAFE_NO_PAD.encode(biscuit.to_vec().unwrap());
    let mut config = gen_rpc_config();
    config.ws_listening_addr = Some("127.0.0.1:0".into());
    config.biscuit_public_key = Some(root.public().to_string());
    let server = start(events.clone(), config, store.clone()).await.unwrap();
    let mut request = format!("ws://{}", server.ws_address.unwrap())
        .into_client_request()
        .unwrap();
    let protocols = format!("fiber-rpc, base64url.bearer.authorization.fiber.{}", token);
    request
        .headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, protocols.parse().unwrap());
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()[SEC_WEBSOCKET_PROTOCOL], "fiber-rpc");
    assert!(!format!("{:?}", response.headers()).contains(&token));
    socket
        .send(Message::text(
            r#"{"jsonrpc":"2.0","id":1,"method":"subscribe","params":["channel_signing"]}"#,
        ))
        .await
        .unwrap();
    let reply = browser_message(&mut socket).await;
    assert!(reply.get("result").is_some(), "{reply}");
    let state = ChannelActorState::samples(42).remove(0);
    let id = state.get_id();
    let publisher = events.channels(None);
    let update = publisher.prepare(&state);
    store.insert_channel_actor_state(state);
    update.publish();
    let event = browser_message(&mut socket).await;
    assert_eq!(event["params"]["subscription"], reply["result"]);
    assert_eq!(
        event["params"]["result"]["data"]["channel_id"],
        format!("{id:#x}")
    );
    socket.close(None).await.unwrap();
    server.handle.stop().unwrap();
}

#[tokio::test]
async fn browser_websocket_rejects_ambiguous_credentials_and_enforces_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let root = KeyPair::new();
    let token = biscuit_auth::Biscuit::builder()
        .code(r#"write("subscriptions"); read("invoices");"#)
        .unwrap()
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let credential = format!(
        "base64url.bearer.authorization.fiber.{}",
        token.trim_end_matches('=')
    );
    let mut config = gen_rpc_config();
    config.ws_listening_addr = Some("127.0.0.1:0".into());
    config.biscuit_public_key = Some(root.public().to_string());
    let server = start(Default::default(), config, store).await.unwrap();
    let url = format!("ws://{}", server.ws_address.unwrap());
    for (protocols, bearer) in [
        (format!("fiber-rpc, {credential}, {credential}"), None),
        (credential.clone(), None),
        (
            "fiber-rpc, base64url.bearer.authorization.fiber.".into(),
            None,
        ),
        (
            "fiber-rpc, base64url.bearer.authorization.fiber.!".into(),
            None,
        ),
        (format!("fiber-rpc, {credential}"), Some("Bearer invalid")),
    ] {
        let mut request = url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, protocols.parse().unwrap());
        if let Some(bearer) = bearer {
            request
                .headers_mut()
                .insert(AUTHORIZATION, bearer.parse().unwrap());
        }
        let error = connect_async(request).await.unwrap_err();
        assert!(
            matches!(error, tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status() == hyper::StatusCode::BAD_REQUEST)
        );
    }
    // Well-formed browser credentials still use the ordinary RPC policies,
    // while invalid Biscuit tokens are rejected during the HTTP upgrade handshake.
    let mut request = url.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        format!("fiber-rpc, {credential}").parse().unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::text(
            r#"{"jsonrpc":"2.0","id":1,"method":"subscribe","params":["channel_signing"]}"#,
        ))
        .await
        .unwrap();
    let reply = browser_message(&mut socket).await;
    assert_eq!(reply["error"]["code"], -32001);
    socket.close(None).await.unwrap();

    let invalid_token = URL_SAFE_NO_PAD.encode(b"not a Biscuit");
    let mut request = url.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        format!(
            "fiber-rpc, base64url.bearer.authorization.fiber.{}",
            invalid_token.trim_end_matches('=')
        )
        .parse()
        .unwrap(),
    );
    let error = connect_async(request).await.unwrap_err();
    assert!(
        matches!(error, tokio_tungstenite::tungstenite::Error::Http(response)
        if response.status() == hyper::StatusCode::UNAUTHORIZED)
    );
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        SEC_WEBSOCKET_PROTOCOL,
        format!("fiber-rpc, {credential}").parse().unwrap(),
    );
    let http = HttpClientBuilder::default()
        .set_headers(headers)
        .build(format!("http://{}", server.http_address))
        .unwrap();
    let error = http
        .request::<serde_json::Value, _>("parse_invoice", rpc_params![{}])
        .await
        .unwrap_err();
    assert!(matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32999));
    server.handle.stop().unwrap();
}

#[tokio::test]
async fn dedicated_ws_isolates_subscriptions_and_shares_rpc_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let mut config = gen_rpc_config();
    config.listening_addr = Some("127.0.0.1:0".into());
    config.ws_listening_addr = Some("127.0.0.1:0".into());
    let events = crate::signing::SigningEvents::default();
    let server = start(events.clone(), config, store.clone()).await.unwrap();
    let ws_address = server.ws_address.unwrap();
    let original = WsClientBuilder::default()
        .build(format!("ws://{}", server.http_address))
        .await
        .unwrap();
    let dedicated = WsClientBuilder::default()
        .build(format!("ws://{ws_address}"))
        .await
        .unwrap();

    let http = HttpClientBuilder::default()
        .build(format!("http://{}", server.http_address))
        .unwrap();
    let error = http
        .request::<serde_json::Value, _>(
            "parse_invoice",
            rpc_params![serde_json::json!({"invoice": "invalid"})],
        )
        .await
        .unwrap_err();
    let jsonrpsee::core::ClientError::Call(expected) = error else {
        panic!("expected RPC error: {error}");
    };
    assert_ne!(expected.code(), -32601);
    for client in [&original, &dedicated] {
        let error = client
            .request::<serde_json::Value, _>(
                "parse_invoice",
                rpc_params![serde_json::json!({"invoice": "invalid"})],
            )
            .await
            .unwrap_err();
        let jsonrpsee::core::ClientError::Call(error) = error else {
            panic!("expected RPC error: {error}");
        };
        assert_eq!(error, expected);
    }
    let error = original
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["channel_signing"],
            "unsubscribe",
        )
        .await
        .unwrap_err();
    assert!(matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32601));
    let mut subscription = dedicated
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["channel_signing"],
            "unsubscribe",
        )
        .await
        .unwrap();
    let state = ChannelActorState::samples(42).remove(0);
    let channel_id = state.get_id();
    {
        let publisher = events.channels(None);
        let update = publisher.prepare(&state);
        store.insert_channel_actor_state(state);
        update.publish();
    };
    let event = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(event, SubscriptionEvent::ChannelSigning(result) if result.channel_id == channel_id.into())
    );
    let http_on_ws = HttpClientBuilder::default()
        .build(format!("http://{ws_address}"))
        .unwrap();
    let error = http_on_ws
        .request::<serde_json::Value, _>(
            "parse_invoice",
            rpc_params![serde_json::json!({"invoice": "invalid"})],
        )
        .await
        .unwrap_err();
    assert!(matches!(error, jsonrpsee::core::ClientError::Transport(_)));

    server.handle.stop().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(original.on_disconnect(), dedicated.on_disconnect());
        server.handle.stopped().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dedicated_ws_is_optional_and_requires_auth_on_public_interfaces() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let config = gen_rpc_config();
    let server = start(Default::default(), config.clone(), store.clone())
        .await
        .unwrap();
    assert!(server.ws_address.is_none());
    server.handle.stop().unwrap();
    server.handle.stopped().await;

    let mut config = config;
    config.ws_listening_addr = Some("0.0.0.0:0".into());
    let error = start(Default::default(), config, store)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("biscuit public key"));
}

#[tokio::test]
async fn failed_ws_bind_does_not_leave_primary_listener_running() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_address = primary.local_addr().unwrap();
    let mut config = gen_rpc_config();
    config.listening_addr = Some(primary_address.to_string());
    config.ws_listening_addr = Some(occupied.local_addr().unwrap().to_string());
    drop(primary);
    assert!(start(Default::default(), config, store).await.is_err());
    let _available = tokio::net::TcpListener::bind(primary_address)
        .await
        .unwrap();
}

#[test]
fn test_is_public_addr() {
    assert!(is_public_addr("[::]:0").unwrap());
    assert!(!is_public_addr("[::1]:0").unwrap());
    assert!(is_public_addr("0.0.0.0:0").unwrap());
    assert!(!is_public_addr("127.0.0.1:0").unwrap());
}

pub(crate) async fn start_server(
    addr: &str,
    auth: Option<BiscuitAuth>,
    methods: impl Into<Methods>,
    cors_enabled: bool,
    cors_allowed_origins: Vec<String>,
) -> Result<(ServerHandle, SocketAddr)> {
    let listener = TcpListener::bind(addr).await?;
    let listen_addr = listener.local_addr()?;
    let (stop_handle, handle) = stop_channel();
    let enable_auth = auth.is_some();
    spawn_listener(
        listener,
        Arc::new(auth.unwrap_or_else(BiscuitAuth::without_pubkey)),
        enable_auth,
        methods.into(),
        stop_handle,
        ListenerConfig {
            ws_only: false,
            cors_enabled,
            cors_allowed_origins,
        },
    );
    Ok((handle, listen_addr))
}
