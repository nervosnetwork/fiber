#[cfg(not(target_arch = "wasm32"))]
pub mod cch;
pub mod channel;
pub mod config;
#[cfg(debug_assertions)]
pub mod dev;
pub mod graph;
pub mod info;
pub mod invoice;
pub mod payment;
pub mod peer;
pub mod utils;
#[cfg(feature = "watchtower")]
pub mod watchtower;
#[cfg(not(target_arch = "wasm32"))]
pub mod server {

    use crate::ckb::CkbConfig;
    use crate::fiber::gossip::GossipMessageStore;
    use crate::rpc::cch::{CchRpcServer, CchRpcServerImpl};
    use crate::rpc::channel::{ChannelRpcServer, ChannelRpcServerImpl};
    pub use crate::rpc::config::RpcConfig;
    #[cfg(debug_assertions)]
    use crate::rpc::dev::{DevRpcServer, DevRpcServerImpl};
    use crate::rpc::graph::{GraphRpcServer, GraphRpcServerImpl};
    use crate::rpc::info::InfoRpcServer;
    use crate::rpc::info::InfoRpcServerImpl;
    use crate::rpc::invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
    use crate::rpc::payment::PaymentRpcServer;
    use crate::rpc::payment::PaymentRpcServerImpl;
    use crate::rpc::peer::{PeerRpcServer, PeerRpcServerImpl};
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
    #[cfg(debug_assertions)]
    use ckb_types::core::TransactionView;
    use jsonrpsee::server::{Server, ServerHandle};
    use jsonrpsee::RpcModule;
    use ractor::ActorRef;
    #[cfg(debug_assertions)]
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[cfg(feature = "watchtower")]
    pub trait RpcServerStore:
        ChannelActorStateStore
        + InvoiceStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + WatchtowerStore
    {
    }
    #[cfg(feature = "watchtower")]
    impl<T> RpcServerStore for T where
        T: ChannelActorStateStore
            + InvoiceStore
            + NetworkGraphStateStore
            + GossipMessageStore
            + WatchtowerStore
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

    async fn build_server(addr: &str) -> Server {
        #[cfg(debug_assertions)]
        {
            // Use socket2 to set reuse address and reuse port,
            // so that we can restart the server without waiting for the port to be released.
            // it will avoid the error: "Address already in use" in CI.
            use socket2::{Domain, Socket, Type};
            let addr = addr.parse().expect("valid address");
            let domain = Domain::for_address(addr);
            let socket = Socket::new(domain, Type::STREAM, None).expect("new socket");
            socket
                .set_nonblocking(true)
                .expect("set socket nonblocking");
            socket.set_reuse_address(true).expect("set reuse address");
            #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
            socket.set_reuse_port(true).expect("set reuse port");

            socket.bind(&addr.into()).expect("bind socket to address");
            socket.listen(4096).expect("listen socket at the port");

            jsonrpsee::server::Server::builder()
                .build_from_tcp(socket)
                .expect("JsonRPC server built from TCP")
        }
        #[cfg(not(debug_assertions))]
        {
            Server::builder()
                .build(addr)
                .await
                .expect("JsonRPC server built")
        }
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
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
        #[cfg(debug_assertions)] ckb_chain_actor: Option<ActorRef<CkbChainMessage>>,
        #[cfg(debug_assertions)] rpc_dev_module_commitment_txs: Option<
            Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
        >,
    ) -> (ServerHandle, SocketAddr) {
        let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
        let server = build_server(listening_addr).await;
        let sockaddr = server.local_addr().expect("local addr");

        let mut modules = RpcModule::new(());
        if config.is_module_enabled("invoice") {
            modules
                .merge(InvoiceRpcServerImpl::new(store.clone(), fiber_config).into_rpc())
                .unwrap();
        }
        if config.is_module_enabled("graph") {
            modules
                .merge(GraphRpcServerImpl::new(network_graph, store.clone()).into_rpc())
                .unwrap();
        }
        if let Some(network_actor) = network_actor {
            if config.is_module_enabled("info") {
                modules
                    .merge(
                        InfoRpcServerImpl::new(
                            network_actor.clone(),
                            ckb_config.expect("ckb config should be set"),
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
        }
        if let Some(cch_actor) = cch_actor {
            if config.is_module_enabled("cch") {
                modules
                    .merge(CchRpcServerImpl::new(cch_actor).into_rpc())
                    .unwrap();
            }
        }
        (server.start(modules), sockaddr)
    }
}
