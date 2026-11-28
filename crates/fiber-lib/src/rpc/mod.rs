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
#[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
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
pub mod tenant;
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
    #[cfg(feature = "lsp")]
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
    use crate::store::store_impl::StoreChangePort;
    use ractor::ActorCell;

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

        const MAX_CONCURRENT_CONNECTIONS: usize = 1024;
        const MAX_BATCH_REQUESTS: u32 = 32;

        let mut server_config = jsonrpsee::server::ServerConfig::builder()
            .max_subscriptions_per_connection(32)
            .set_message_buffer_capacity(64)
            .set_batch_request_config(jsonrpsee::server::BatchRequestConfig::Limit(
                MAX_BATCH_REQUESTS,
            ))
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

        let conn_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
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
                let Ok(conn_permit) = conn_slots.clone().try_acquire_owned() else {
                    tracing::warn!("RPC connection limit ({MAX_CONCURRENT_CONNECTIONS}) reached; dropping incoming connection");
                    drop(sock);
                    continue;
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

                        let is_ws_upgrade = jsonrpsee::server::ws::is_upgrade_request(&req);
                        let browser_protocol = if browser_ws && is_ws_upgrade {
                            prepare_browser_websocket(req.headers_mut())
                        } else {
                            Ok(false)
                        };
                        let ws_auth_result =
                            if enable_auth && is_ws_upgrade && browser_protocol.is_ok() {
                                crate::rpc::middleware::extract_bearer_token(req.headers())
                                    .and_then(|token| auth.validate_token_for_ws_upgrade(&token))
                            } else {
                                Ok(())
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
                            if let Err(err) = ws_auth_result {
                                tracing::debug!(
                                    "Rejecting unauthenticated WebSocket upgrade: {err}"
                                );
                                let mut response = hyper::Response::new(
                                    jsonrpsee::server::HttpBody::from("Unauthorized".to_owned()),
                                );
                                *response.status_mut() = hyper::StatusCode::UNAUTHORIZED;
                                return Ok(response);
                            }
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

                use futures::FutureExt;
                tokio::spawn(
                    serve_with_graceful_shutdown(sock, svc, stop_handle.clone().shutdown()).map(
                        move |_| {
                            drop(conn_permit);
                        },
                    ),
                );
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
    pub async fn start_rpc<
        S: RpcServerStore + crate::rpc_scope::RpcBackendStore + Clone + Send + Sync + 'static,
    >(
        config: RpcConfig,
        ckb_config: Option<CkbConfig>,
        fiber_config: Option<FiberConfig>,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        cch_actor: Option<ActorRef<CchMessage>>,
        lsp_actor: crate::rpc::tenant::LspActorHandle,
        store: S,
        store_actor: Option<ActorRef<StoreActorMessage>>,
        network_graph: Option<Arc<RwLock<NetworkGraph<S>>>>,
        supervisor: ActorCell,
        store_change_port: Option<Arc<StoreChangePort>>,
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

        let lsp_enabled = lsp_actor.is_some();
        #[cfg(feature = "lsp")]
        let tenant_token_epochs = match lsp_actor.as_ref() {
            Some(actor) => Some(
                ractor::call!(actor, crate::lsp::LspServiceMessage::GetTenantTokenEpochs).map_err(
                    |err| anyhow::anyhow!("Failed to query LSP tenant token epochs: {err}"),
                )?,
            ),
            None => None,
        };
        let auth = match config.biscuit_public_key.as_ref() {
            Some(key) => {
                let auth = BiscuitAuth::from_pubkey(key.to_string())?.with_lsp_enabled(lsp_enabled);
                #[cfg(feature = "lsp")]
                let auth = if let Some(epochs) = tenant_token_epochs {
                    auth.with_tenant_token_epochs(epochs)
                } else {
                    auth
                };
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
                #[cfg(feature = "lsp")]
                let watchtower_quotas = match lsp_actor.as_ref() {
                    Some(actor) => {
                        ractor::call!(actor, crate::lsp::LspServiceMessage::GetWatchtowerQuotas)
                            .map_err(|err| {
                                anyhow::anyhow!("Failed to query LSP watchtower quotas: {err}")
                            })?
                    }
                    None => Default::default(),
                };
                #[cfg(not(feature = "lsp"))]
                let watchtower_quotas = crate::lsp_config::WatchtowerQuotas::default();
                let watchtower_server = WatchtowerRpcServerImpl::new(store.clone())
                    .with_signing_events(signing_events.clone())
                    .with_quotas(watchtower_quotas)
                    .with_lsp_enabled(lsp_enabled);
                #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
                let watchtower_server = watchtower_server.with_lsp_actor(lsp_actor.clone());
                modules.merge(watchtower_server.into_rpc()).unwrap();
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
        #[cfg(feature = "lsp")]
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

    #[cfg(test)]
    #[path = "tests.rs"]
    pub(crate) mod tests;
}
