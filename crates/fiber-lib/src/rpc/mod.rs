#[cfg(not(target_arch = "wasm32"))]
pub mod biscuit;
#[cfg(not(target_arch = "wasm32"))]
pub mod cch;
pub mod channel;
pub mod config;
pub mod context;
#[cfg(debug_assertions)]
pub mod dev;
pub mod graph;
pub mod info;
pub mod invoice;
#[cfg(not(target_arch = "wasm32"))]
mod middleware;
pub mod payment;
pub mod peer;
#[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
pub mod prof;
pub mod utils;
pub mod watchtower;
#[cfg(not(target_arch = "wasm32"))]
pub mod server {
    use crate::ckb::CkbConfig;
    use crate::fiber::gossip::GossipMessageStore;
    #[cfg(feature = "watchtower")]
    use crate::invoice::PreimageStore;
    use crate::rpc::cch::{CchRpcServer, CchRpcServerImpl};
    use crate::rpc::channel::{ChannelRpcServer, ChannelRpcServerImpl};
    pub use crate::rpc::config::RpcConfig;
    #[cfg(debug_assertions)]
    use crate::rpc::dev::{DevRpcServer, DevRpcServerImpl};
    use crate::rpc::graph::{GraphRpcServer, GraphRpcServerImpl};
    use crate::rpc::info::InfoRpcServer;
    use crate::rpc::info::InfoRpcServerImpl;
    use crate::rpc::invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
    use crate::rpc::middleware::BiscuitAuthMiddleware;
    use crate::rpc::payment::PaymentRpcServer;
    use crate::rpc::payment::PaymentRpcServerImpl;
    use crate::rpc::peer::{PeerRpcServer, PeerRpcServerImpl};
    #[cfg(all(feature = "pprof", not(target_arch = "wasm32")))]
    use crate::rpc::prof::{ProfRpcServer, ProfRpcServerImpl};
    use crate::store::store_impl::KVStore;
    use crate::{
        cch::CchMessage,
        fiber::{
            channel::ChannelActorStateStore,
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
    use tower::Service;
    use tracing::debug;

    use super::biscuit::BiscuitAuth;

    #[cfg(feature = "watchtower")]
    pub trait RpcServerStore:
        ChannelActorStateStore
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
            + InvoiceStore
            + NetworkGraphStateStore
            + GossipMessageStore
            + WatchtowerStore
            + PreimageStore
    {
    }
    #[cfg(not(feature = "watchtower"))]
    pub trait RpcServerStore:
        ChannelActorStateStore + InvoiceStore + NetworkGraphStateStore + GossipMessageStore
    {
    }
    #[cfg(not(feature = "watchtower"))]
    impl<T> RpcServerStore for T where
        T: ChannelActorStateStore + InvoiceStore + NetworkGraphStateStore + GossipMessageStore
    {
    }

    async fn start_server(
        addr: &str,
        auth: Option<BiscuitAuth>,
        methods: impl Into<Methods>,
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

    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    pub async fn start_rpc<S: RpcServerStore + KVStore + Clone + Send + Sync + 'static>(
        config: RpcConfig,
        ckb_config: Option<CkbConfig>,
        fiber_config: Option<FiberConfig>,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        cch_actor: Option<ActorRef<CchMessage>>,
        store: S,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
        #[cfg(debug_assertions)] ckb_chain_actor: Option<ActorRef<CkbChainMessage>>,
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
            modules
                .merge(GraphRpcServerImpl::new(network_graph, store.clone()).into_rpc())
                .unwrap();
        }
        if let Some(network_actor) = network_actor {
            if config.is_module_enabled("info") {
                #[cfg(not(target_arch = "wasm32"))]
                modules
                    .merge(
                        InfoRpcServerImpl::new(
                            network_actor.clone(),
                            store.clone(),
                            ckb_config.clone().expect("ckb config should be set"),
                            fiber_config.clone(),
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
                            ckb_chain_actor.expect("ckb_chain_actor should be set"),
                            network_actor.clone(),
                            rpc_dev_module_commitment_txs
                                .expect("rpc_dev_module_commitment_txs should be set"),
                        )
                        .into_rpc(),
                    )
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

        let (handle, addr) = start_server(listening_addr, auth, modules).await?;
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
}
