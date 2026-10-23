#[cfg(not(target_arch = "wasm32"))]
pub mod admin;
#[cfg(not(target_arch = "wasm32"))]
pub mod biscuit;
#[cfg(not(target_arch = "wasm32"))]
pub mod cch;
pub mod channel;
pub mod config;
#[cfg(debug_assertions)]
pub mod dev;
pub mod graph;
pub mod info;
pub mod invoice;
#[cfg(not(target_arch = "wasm32"))]
pub mod lsp;
#[cfg(not(target_arch = "wasm32"))]
mod middleware;
pub mod payment;
pub mod peer;
#[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
pub mod prof;
#[cfg(not(target_arch = "wasm32"))]
pub mod pubsub;
#[cfg(not(target_arch = "wasm32"))]
pub mod subscription;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod tenant;
pub mod utils;
pub mod watchtower;
#[cfg(not(target_arch = "wasm32"))]
pub mod server {
    use crate::ckb::CkbConfig;
    use crate::fiber::gossip::GossipMessageStore;
    #[cfg(feature = "watchtower")]
    use crate::invoice::PreimageStore;
    use crate::rpc::admin::{AdminRpcServer, AdminRpcServerImpl};
    use crate::rpc::cch::{CchRpcServer, CchRpcServerImpl};
    use crate::rpc::channel::{ChannelRpcServer, ChannelRpcServerImpl};
    pub use crate::rpc::config::RpcConfig;
    #[cfg(debug_assertions)]
    use crate::rpc::dev::{DevRpcServer, DevRpcServerImpl};
    use crate::rpc::graph::{GraphRpcServer, GraphRpcServerImpl};
    use crate::rpc::info::{InfoRpcServer, InfoRpcServerImpl};
    use crate::rpc::invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
    use crate::rpc::lsp::{LspRpcServer, LspRpcServerImpl};
    use crate::rpc::middleware::{
        prepare_browser_websocket, BiscuitAuthMiddleware, BROWSER_WS_PROTOCOL,
    };
    use crate::rpc::payment::PaymentRpcServer;
    use crate::rpc::payment::PaymentRpcServerImpl;
    use crate::rpc::peer::{PeerRpcServer, PeerRpcServerImpl};
    #[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
    use crate::rpc::prof::{ProfRpcServer, ProfRpcServerImpl};
    use crate::{
        cch::CchMessage,
        fiber::{
            channel::{ChannelActorStateStore, ChannelOpenRecordStore},
            graph::{NetworkGraph, NetworkGraphStateStore},
            NetworkActorMessage,
        },
        invoice::InvoiceStore,
        FiberConfig,
    };
    #[cfg(debug_assertions)]
    use crate::{ckb::CkbChainMessage, fiber::types::Hash256};
    #[cfg(feature = "watchtower")]
    use crate::{
        rpc::watchtower::{WatchtowerRpcServer, WatchtowerRpcServerImpl},
        watchtower::WatchtowerStore,
    };
    use anyhow::{bail, Result};
    #[cfg(debug_assertions)]
    use ckb_types::core::TransactionView;
    use jsonrpsee::server::{
        serve_with_graceful_shutdown, stop_channel, ServerHandle, StopHandle, TowerServiceBuilder,
    };
    use jsonrpsee::ws_client::RpcServiceBuilder;
    use jsonrpsee::{Methods, RpcModule};
    use ractor::ActorRef;
    #[cfg(debug_assertions)]
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::RwLock;
    use tower::{Service, ServiceExt};
    use tower_http::cors::{Any, CorsLayer};
    use tracing::debug;

    use super::biscuit::BiscuitAuth;
    use crate::store::actor::StoreActorMessage;
    use crate::store::store_impl::StoreChange;
    use ractor::{ActorCell, OutputPort};

    #[cfg(feature = "watchtower")]
    pub trait RpcServerStore:
        ChannelActorStateStore
        + ChannelOpenRecordStore
        + InvoiceStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + WatchtowerStore
        + PreimageStore
    {
    }
    #[cfg(feature = "watchtower")]
    impl<T> RpcServerStore for T where
        T: ChannelActorStateStore
            + ChannelOpenRecordStore
            + InvoiceStore
            + NetworkGraphStateStore
            + GossipMessageStore
            + WatchtowerStore
            + PreimageStore
    {
    }
    #[cfg(not(feature = "watchtower"))]
    pub trait RpcServerStore:
        ChannelActorStateStore
        + ChannelOpenRecordStore
        + InvoiceStore
        + NetworkGraphStateStore
        + GossipMessageStore
    {
    }
    #[cfg(not(feature = "watchtower"))]
    impl<T> RpcServerStore for T where
        T: ChannelActorStateStore
            + ChannelOpenRecordStore
            + InvoiceStore
            + NetworkGraphStateStore
            + GossipMessageStore
    {
    }

    /// Running RPC listeners, sharing one shutdown handle.
    pub struct RpcServer {
        /// Stops both listeners and their active connections.
        pub handle: ServerHandle,
        /// Bound address of the original HTTP/WS listener.
        pub http_address: SocketAddr,
        /// Bound address of the optional dedicated WebSocket listener.
        pub ws_address: Option<SocketAddr>,
    }

    struct ListenerConfig {
        ws_only: bool,
        cors_enabled: bool,
        cors_allowed_origins: Vec<String>,
    }

    #[cfg(test)]
    pub(super) async fn start_server(
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

    fn spawn_listener(
        listener: TcpListener,
        auth: Arc<BiscuitAuth>,
        enable_auth: bool,
        methods: Methods,
        stop_handle: StopHandle,
        config: ListenerConfig,
    ) {
        // From this example
        // https://github.com/paritytech/jsonrpsee/blob/d3d9fa8553756751ad913830e7d0d0faca614cb5/examples/examples/jsonrpsee_as_service.rs

        // This state is cloned for every connection
        // all these types based on Arcs and it should
        // be relatively cheap to clone them.
        //
        // Make sure that nothing expensive is cloned here
        // when doing this or use an `Arc`.
        #[derive(Clone)]
        struct PerConnection<RpcMiddlewave, HttpMiddlewave> {
            methods: Methods,
            stop_handle: StopHandle,
            svc_builder: TowerServiceBuilder<RpcMiddlewave, HttpMiddlewave>,
        }

        // Each RPC call/connection get its own `stop_handle`
        // to able to determine whether the server has been stopped or not.
        //
        // To keep the server running the `server_handle`
        // must be kept and it can also be used to stop the server.

        let mut server_config = jsonrpsee::server::ServerConfig::builder()
            .max_subscriptions_per_connection(32)
            .set_message_buffer_capacity(64)
            .enable_ws_ping(jsonrpsee::server::PingConfig::default());
        if config.ws_only {
            server_config = server_config.ws_only();
        }
        let cors_enabled = config.cors_enabled;
        let cors_allowed_origins = config.cors_allowed_origins;
        let browser_ws = config.ws_only;
        let per_conn = PerConnection {
            methods,
            stop_handle: stop_handle.clone(),
            svc_builder: jsonrpsee::server::Server::builder()
                .set_config(server_config.build())
                .to_service_builder(),
        };

        tokio::spawn(async move {
            loop {
                // accept connection or stop
                let sock = tokio::select! {
                    res = listener.accept() => {
                        match res {
                            Ok((stream, _remote_addr)) => stream,
                            Err(e) => {
                                tracing::error!("failed to accept connection: {e:?}");
                                continue;
                            }
                        }
                    }
                    _ = per_conn.stop_handle.clone().shutdown() => break,
                };

                let per_conn2 = per_conn.clone();
                let auth = auth.clone();

                let svc =
                    tower::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                        let PerConnection {
                            methods,
                            stop_handle,
                            svc_builder,
                        } = per_conn2.clone();

                        let browser_protocol =
                            if browser_ws && jsonrpsee::server::ws::is_upgrade_request(&req) {
                                prepare_browser_websocket(req.headers_mut())
                            } else {
                                Ok(false)
                            };
                        let headers = req.headers().clone();
                        let auth = auth.clone();
                        let rpc_middleware = RpcServiceBuilder::new().layer_fn(move |service| {
                            BiscuitAuthMiddleware {
                                headers: headers.clone(),
                                inner: service,
                                auth: auth.clone(),
                                enable_auth,
                            }
                        });
                        let mut svc = svc_builder
                            .set_rpc_middleware(rpc_middleware)
                            .build(methods, stop_handle);
                        async move {
                            let browser_protocol = match browser_protocol {
                                Ok(protocol) => protocol,
                                Err(_) => {
                                    let mut response =
                                        hyper::Response::new(jsonrpsee::server::HttpBody::from(
                                            "Invalid WebSocket authentication protocols".to_owned(),
                                        ));
                                    *response.status_mut() = hyper::StatusCode::BAD_REQUEST;
                                    return Ok(response);
                                }
                            };
                            svc.call(req).await.map(|mut response| {
                                if browser_protocol
                                    && response.status() == hyper::StatusCode::SWITCHING_PROTOCOLS
                                {
                                    response.headers_mut().insert(
                                        hyper::header::SEC_WEBSOCKET_PROTOCOL,
                                        hyper::header::HeaderValue::from_static(
                                            BROWSER_WS_PROTOCOL,
                                        ),
                                    );
                                }
                                response
                            })
                        }
                    });

                // Conditionally wrap the service with CORS layer if enabled
                let svc = if cors_enabled {
                    // Configure CORS to allow configured origins and handle preflight requests
                    // Note: CORS must be the outermost layer to handle OPTIONS preflight requests
                    // before authentication, as required by the CORS specification.
                    let cors_layer = if cors_allowed_origins.is_empty() {
                        // If no specific origins configured, allow all origins
                        CorsLayer::new()
                            .allow_origin(Any)
                            .allow_methods(Any)
                            .allow_headers(Any)
                    } else {
                        // Allow specific configured origins
                        use tower_http::cors::AllowOrigin;
                        let origins: Vec<_> = cors_allowed_origins
                            .iter()
                            .filter_map(|o| o.parse().ok())
                            .collect();
                        CorsLayer::new()
                            .allow_origin(AllowOrigin::list(origins))
                            .allow_methods(Any)
                            .allow_headers(Any)
                    };
                    tower::ServiceBuilder::new()
                        .layer(cors_layer)
                        .service(svc)
                        .boxed_clone()
                } else {
                    tower::ServiceBuilder::new().service(svc).boxed_clone()
                };

                tokio::spawn(serve_with_graceful_shutdown(
                    sock,
                    svc,
                    stop_handle.clone().shutdown(),
                ));
            }
        });
    }

    fn is_public_addr(addr: &str) -> Result<bool> {
        let addrs = addr.to_socket_addrs()?;
        Ok(addrs.into_iter().any(|addr| {
            let ip = addr.ip();
            if ip.is_unspecified() {
                return true;
            }
            match ip {
                IpAddr::V4(ip) => {
                    !(ip.is_private()
                        || ip.is_loopback()
                        || ip.is_link_local()
                        || ip.is_documentation())
                }
                IpAddr::V6(ip) => !(ip.is_loopback() || ip.is_unique_local()),
            }
        }))
    }

    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    pub async fn start_rpc<S: RpcServerStore + Clone + Send + Sync + 'static>(
        config: RpcConfig,
        ckb_config: Option<CkbConfig>,
        fiber_config: Option<FiberConfig>,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        cch_actor: Option<ActorRef<CchMessage>>,
        lsp_actor: Option<ActorRef<crate::lsp::LspServiceMessage>>,
        store: S,
        store_actor: Option<ActorRef<StoreActorMessage>>,
        network_graph: Option<Arc<RwLock<NetworkGraph<S>>>>,
        supervisor: ActorCell,
        store_change_port: Option<Arc<OutputPort<StoreChange>>>,
        signing_events: crate::signing::SigningEvents,
        #[cfg(debug_assertions)] ckb_chain_actor: Option<ActorRef<CkbChainMessage>>,
        #[cfg(debug_assertions)] rpc_dev_module_commitment_txs: Option<
            Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
        >,
    ) -> Result<RpcServer> {
        let listening_addr = config.listening_addr.as_deref().unwrap_or("[::1]:0");
        for address in std::iter::once(listening_addr).chain(config.ws_listening_addr.as_deref()) {
            if config.biscuit_public_key.is_none() && is_public_addr(address)? {
                bail!("Cannot listen on a public address without a biscuit public key set in the config. Please set rpc.biscuit_public_key or listen on a private interface.");
            }
        }

        if config.biscuit_private_key_path.is_some() {
            if lsp_actor.is_none() {
                bail!("rpc.biscuit_private_key_path requires the LSP service");
            }
            if config.biscuit_public_key.is_none() {
                bail!("rpc.biscuit_private_key_path requires rpc.biscuit_public_key");
            }
        }

        let auth = match config.biscuit_public_key.as_ref() {
            Some(key) => {
                let auth = BiscuitAuth::from_pubkey(key.to_string())?;
                tracing::info!("Enable RPC auth");
                Some(auth)
            }
            None => None,
        };

        let mut modules = RpcModule::new(());
        if config.is_module_enabled("invoice") {
            modules
                .merge(
                    InvoiceRpcServerImpl::new(
                        store.clone(),
                        network_actor.clone(),
                        fiber_config.clone(),
                    )
                    .with_lsp_actor(lsp_actor.clone())
                    .into_rpc(),
                )
                .unwrap();
        }
        if config.is_module_enabled("graph") {
            if let Some(ref network_graph) = network_graph {
                modules
                    .merge(GraphRpcServerImpl::new(network_graph.clone(), store.clone()).into_rpc())
                    .unwrap();
            }
        }
        if let Some(network_actor) = network_actor {
            if config.is_module_enabled("info") {
                modules
                    .merge(
                        InfoRpcServerImpl::new(
                            network_actor.clone(),
                            ckb_config.clone().expect("ckb config should be set"),
                        )
                        .into_rpc(),
                    )
                    .unwrap();
            }

            if config.is_module_enabled("peer") {
                modules
                    .merge(PeerRpcServerImpl::new(network_actor.clone()).into_rpc())
                    .unwrap();
            }

            if config.is_module_enabled("channel") {
                modules
                    .merge(
                        ChannelRpcServerImpl::new(network_actor.clone(), store.clone())
                            .with_lsp_actor(lsp_actor.clone())
                            .into_rpc(),
                    )
                    .unwrap();
            }

            if config.is_module_enabled("payment") {
                modules
                    .merge(
                        PaymentRpcServerImpl::new(network_actor.clone(), store.clone())
                            .with_lsp_actor(lsp_actor.clone())
                            .into_rpc(),
                    )
                    .unwrap();
            }

            #[cfg(feature = "watchtower")]
            if config.is_module_enabled("watchtower") {
                modules
                    .merge(
                        WatchtowerRpcServerImpl::new(store.clone())
                            .with_signing_events(signing_events.clone())
                            .into_rpc(),
                    )
                    .unwrap();
            }

            #[cfg(debug_assertions)]
            if config.is_module_enabled("dev") {
                modules
                    .merge(
                        DevRpcServerImpl::new(
                            ckb_config
                                .clone()
                                .expect("ckb config should be set")
                                .rpc_url,
                            ckb_chain_actor.expect("ckb_chain_actor should be set"),
                            network_actor.clone(),
                            rpc_dev_module_commitment_txs
                                .expect("rpc_dev_module_commitment_txs should be set"),
                        )
                        .with_lsp_actor(lsp_actor.clone())
                        .into_rpc(),
                    )
                    .unwrap();
            }

            if config.is_module_enabled("admin") {
                modules
                    .merge(AdminRpcServerImpl::new(store_actor).into_rpc())
                    .unwrap();
            }

            #[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
            if config.is_module_enabled("prof") {
                modules.merge(ProfRpcServerImpl::new().into_rpc()).unwrap();
            }
        }
        if let Some(cch_actor) = cch_actor {
            if config.is_module_enabled("cch") {
                modules
                    .merge(CchRpcServerImpl::new(cch_actor).into_rpc())
                    .unwrap();
            }
        }
        if let Some(lsp_actor) = lsp_actor.clone() {
            if config.is_module_enabled("lsp") {
                modules
                    .merge(LspRpcServerImpl::new(lsp_actor).into_rpc())
                    .unwrap();
            }
        }
        if config.is_module_enabled("pubsub") {
            if let Some(ref store_change_port) = store_change_port {
                crate::rpc::pubsub::register_pub_sub_rpc(
                    &mut modules,
                    store_change_port,
                    supervisor,
                )
                .await?;
            }
        }

        // Bind all configured addresses before spawning either listener. A failed
        // second bind drops the first listener instead of leaving a partial service.
        let http_listener = TcpListener::bind(listening_addr).await?;
        let http_address = http_listener.local_addr()?;
        let ws_listener = match config.ws_listening_addr.as_deref() {
            Some(address) => Some(TcpListener::bind(address).await?),
            None => None,
        };
        let ws_address = ws_listener
            .as_ref()
            .map(TcpListener::local_addr)
            .transpose()?;
        let ws_methods = if ws_listener.is_some() {
            use super::subscription::{SubscriptionRpcServer, SubscriptionRpcServerImpl};
            let mut ws_modules = modules.clone();
            ws_modules.merge(
                SubscriptionRpcServerImpl::new(
                    signing_events,
                    store,
                    lsp_actor,
                    config.is_module_enabled("channel"),
                    config.is_module_enabled("watchtower"),
                )
                .into_rpc(),
            )?;
            Some(ws_modules.into())
        } else {
            None
        };
        let enable_auth = auth.is_some();
        let auth = Arc::new(auth.unwrap_or_else(BiscuitAuth::without_pubkey));
        let (stop_handle, handle) = stop_channel();
        spawn_listener(
            http_listener,
            auth.clone(),
            enable_auth,
            modules.into(),
            stop_handle.clone(),
            ListenerConfig {
                ws_only: false,
                cors_enabled: config.cors_enabled,
                cors_allowed_origins: config.cors_allowed_origins.clone(),
            },
        );
        if let (Some(listener), Some(methods)) = (ws_listener, ws_methods) {
            spawn_listener(
                listener,
                auth,
                enable_auth,
                methods,
                stop_handle,
                ListenerConfig {
                    ws_only: true,
                    cors_enabled: config.cors_enabled,
                    cors_allowed_origins: config.cors_allowed_origins,
                },
            );
        }
        debug!(%http_address, ?ws_address, "started RPC listeners");
        Ok(RpcServer {
            handle,
            http_address,
            ws_address,
        })
    }

    #[test]
    fn test_is_public_addr() {
        assert!(is_public_addr("[::]:0").unwrap());
        assert!(!is_public_addr("[::1]:0").unwrap());
        assert!(is_public_addr("0.0.0.0:0").unwrap());
        assert!(!is_public_addr("127.0.0.1:0").unwrap());
    }

    #[cfg(test)]
    mod listener_tests {
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

        use super::{start_rpc, RpcConfig, RpcServer};

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
            // Well-formed browser credentials still use the ordinary RPC policies.
            let invalid_token = URL_SAFE_NO_PAD.encode(b"not a Biscuit");
            for (token, code) in [(token.as_str(), -32001), (invalid_token.as_str(), -32999)] {
                let mut request = url.clone().into_client_request().unwrap();
                request.headers_mut().insert(
                    SEC_WEBSOCKET_PROTOCOL,
                    format!(
                        "fiber-rpc, base64url.bearer.authorization.fiber.{}",
                        token.trim_end_matches('=')
                    )
                    .parse()
                    .unwrap(),
                );
                let (mut socket, _) = connect_async(request).await.unwrap();
                socket.send(Message::text(
                    r#"{"jsonrpc":"2.0","id":1,"method":"subscribe","params":["channel_signing"]}"#,
                )).await.unwrap();
                let reply = browser_message(&mut socket).await;
                assert_eq!(reply["error"]["code"], code);
                socket.close(None).await.unwrap();
            }
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
            assert!(
                matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32999)
            );
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
            assert!(
                matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32601)
            );
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
    }
}
