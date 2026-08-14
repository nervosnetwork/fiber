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
pub mod liquidity;
#[cfg(not(target_arch = "wasm32"))]
mod middleware;
pub mod payment;
pub mod peer;
#[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
pub mod prof;
#[cfg(not(target_arch = "wasm32"))]
pub mod pubsub;
pub mod utils;
pub mod watchtower;
#[cfg(not(target_arch = "wasm32"))]
pub mod server {
    use crate::ckb::contracts::{get_cell_deps_by_contracts, try_get_script_by_contract, Contract};
    use crate::ckb::CkbChainMessage;
    use crate::ckb::CkbConfig;
    use crate::fiber::gossip::GossipMessageStore;
    #[cfg(debug_assertions)]
    use crate::fiber::types::Hash256;
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
    use crate::rpc::liquidity::{LiquidityRpcServer, LiquidityRpcServerImpl};
    use crate::rpc::middleware::BiscuitAuthMiddleware;
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
        liquidity::{
            actor::{LiquidityActor, LiquidityActorArguments, LiquidityActorMessage},
            chain::CkbLiquidityChainWatcher,
            payment::NetworkLoopOutPaymentAdapter,
            store::LiquidityStore,
        },
        FiberConfig,
    };
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
    use ractor::{Actor, ActorRef};
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
        + LiquidityStore
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
            + LiquidityStore
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
        + LiquidityStore
    {
    }
    #[cfg(not(feature = "watchtower"))]
    impl<T> RpcServerStore for T where
        T: ChannelActorStateStore
            + ChannelOpenRecordStore
            + InvoiceStore
            + NetworkGraphStateStore
            + GossipMessageStore
            + LiquidityStore
    {
    }

    async fn start_server(
        addr: &str,
        auth: Option<BiscuitAuth>,
        methods: impl Into<Methods>,
        cors_enabled: bool,
        cors_allowed_origins: Vec<String>,
    ) -> Result<(ServerHandle, SocketAddr)> {
        let listener = TcpListener::bind(addr).await?;
        let listen_addr = listener.local_addr().expect("get local address");

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
        let (stop_handle, server_handle) = stop_channel();

        let per_conn = PerConnection {
            methods: methods.into(),
            stop_handle: stop_handle.clone(),
            svc_builder: jsonrpsee::server::Server::builder().to_service_builder(),
        };
        let enable_auth = auth.is_some();
        let auth = Arc::new(auth.unwrap_or_else(BiscuitAuth::without_pubkey));

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

                let svc = tower::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let PerConnection {
                        methods,
                        stop_handle,
                        svc_builder,
                    } = per_conn2.clone();

                    let headers = req.headers().clone();
                    let auth = auth.clone();
                    let rpc_middleware =
                        RpcServiceBuilder::new().layer_fn(move |service| BiscuitAuthMiddleware {
                            headers: headers.clone(),
                            inner: service,
                            auth: auth.clone(),
                            enable_auth,
                        });
                    let mut svc = svc_builder
                        .set_rpc_middleware(rpc_middleware)
                        .build(methods, stop_handle);
                    async move { svc.call(req).await }
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

        Ok((server_handle, listen_addr))
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

    fn liquidity_provider_pubkey(
        fiber_config: Option<&FiberConfig>,
    ) -> Result<fiber_types::Pubkey> {
        let fiber_config = fiber_config
            .ok_or_else(|| anyhow::anyhow!("liquidity RPC requires Fiber configuration"))?;
        Ok(crate::fiber::types::pubkey_from_tentacle(
            fiber_config.public_key(),
        ))
    }

    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    pub async fn start_rpc<S: RpcServerStore + Clone + Send + Sync + 'static>(
        config: RpcConfig,
        ckb_config: Option<CkbConfig>,
        fiber_config: Option<FiberConfig>,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        cch_actor: Option<ActorRef<CchMessage>>,
        store: S,
        store_actor: Option<ActorRef<StoreActorMessage>>,
        network_graph: Option<Arc<RwLock<NetworkGraph<S>>>>,
        supervisor: ActorCell,
        store_change_port: Option<Arc<OutputPort<StoreChange>>>,
        ckb_chain_actor: Option<ActorRef<CkbChainMessage>>,
        #[cfg(debug_assertions)] rpc_dev_module_commitment_txs: Option<
            Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
        >,
    ) -> Result<(ServerHandle, SocketAddr)> {
        let listening_addr = config.listening_addr.as_deref().unwrap_or("[::1]:0");
        if config.biscuit_public_key.is_none() && is_public_addr(listening_addr)? {
            bail!("Cannot listen on a public address without a biscuit public key set in the config. Please set rpc.biscuit_public_key or listen on a private interface.");
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
        let liquidity_actor = if config.is_module_enabled("liquidity") {
            {
                let provider_pubkey = liquidity_provider_pubkey(fiber_config.as_ref())?;
                match (network_actor.clone(), ckb_chain_actor.clone()) {
                    (Some(network_actor), Some(ckb_chain_actor)) => {
                        if let Some(liquidity_lock_script) =
                            try_get_script_by_contract(Contract::LiquidityLock, &[])
                        {
                            match get_cell_deps_by_contracts(vec![Contract::LiquidityLock]).await {
                                Ok(cell_deps) => {
                                    let liquidity_lock_cell_deps: Vec<_> =
                                        cell_deps.into_iter().collect();
                                    if liquidity_lock_cell_deps.is_empty() {
                                        tracing::warn!(
                                            "liquidity-lock cell deps are not configured; mutation RPCs will be unavailable"
                                        );
                                        None
                                    } else {
                                        let (actor, _handle) = Actor::spawn_linked(
                                            None,
                                            LiquidityActor::<_, _, _>(std::marker::PhantomData),
                                            LiquidityActorArguments {
                                                store: store.clone(),
                                                payment: NetworkLoopOutPaymentAdapter::new(
                                                    network_actor,
                                                ),
                                                chain: CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
                                                    ckb_chain_actor,
                                                    store.clone(),
                                                    liquidity_lock_script,
                                                    liquidity_lock_cell_deps,
                                                ),
                                                provider_pubkey,
                                            },
                                            supervisor.clone(),
                                        )
                                        .await?;
                                        match ractor::call!(
                                            actor,
                                            LiquidityActorMessage::ResumeNonTerminal
                                        ) {
                                            Ok(Ok(resumed)) => {
                                                tracing::info!(
                                                    resumed,
                                                    "resumed non-terminal liquidity swaps"
                                                );
                                            }
                                            Ok(Err(error)) => {
                                                tracing::warn!(%error, "failed to resume non-terminal liquidity swaps");
                                            }
                                            Err(error) => {
                                                tracing::warn!(%error, "failed to call liquidity actor recovery");
                                            }
                                        }
                                        Some(actor)
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "failed to load liquidity-lock cell deps; mutation RPCs will be unavailable");
                                    None
                                }
                            }
                        } else {
                            tracing::warn!(
                                "liquidity module enabled but liquidity-lock script is not configured; mutation RPCs will be unavailable"
                            );
                            None
                        }
                    }
                    _ => None,
                }
            }
        } else {
            None
        };

        if config.is_module_enabled("liquidity") {
            modules
                .merge(LiquidityRpcServerImpl::new(store.clone(), liquidity_actor).into_rpc())
                .unwrap();
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
                        ChannelRpcServerImpl::new(network_actor.clone(), store.clone()).into_rpc(),
                    )
                    .unwrap();
            }

            if config.is_module_enabled("payment") {
                modules
                    .merge(
                        PaymentRpcServerImpl::new(network_actor.clone(), store.clone()).into_rpc(),
                    )
                    .unwrap();
            }

            #[cfg(feature = "watchtower")]
            if config.is_module_enabled("watchtower") {
                modules
                    .merge(WatchtowerRpcServerImpl::new(store.clone()).into_rpc())
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

        let (handle, addr) = start_server(
            listening_addr,
            auth,
            modules,
            config.cors_enabled,
            config.cors_allowed_origins.clone(),
        )
        .await?;
        debug!("started listen to RPC addr {:?}", &listening_addr);
        Ok((handle, addr))
    }

    #[test]
    fn test_is_public_addr() {
        assert!(is_public_addr("[::]:0").unwrap());
        assert!(!is_public_addr("[::1]:0").unwrap());
        assert!(is_public_addr("0.0.0.0:0").unwrap());
        assert!(!is_public_addr("127.0.0.1:0").unwrap());
    }

    #[test]
    fn liquidity_startup_provider_identity_matches_fiber_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fiber_config = crate::tests::get_fiber_config(temp_dir.path(), None);
        let expected = crate::fiber::types::pubkey_from_tentacle(fiber_config.public_key());

        let actual = liquidity_provider_pubkey(Some(&fiber_config)).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn liquidity_startup_requires_fiber_config() {
        let error = liquidity_provider_pubkey(None).unwrap_err();

        assert!(error
            .to_string()
            .contains("liquidity RPC requires Fiber configuration"));
    }
}
