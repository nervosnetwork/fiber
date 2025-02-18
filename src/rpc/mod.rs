mod cch;
mod channel;
mod config;
#[cfg(debug_assertions)]
mod dev;
mod graph;
mod info;
mod invoice;
mod payment;
mod peer;
mod utils;

use crate::ckb::CkbConfig;
use crate::fiber::gossip::GossipMessageStore;
use crate::rpc::info::InfoRpcServer;
use crate::rpc::payment::PaymentRpcServer;
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
use cch::{CchRpcServer, CchRpcServerImpl};
use channel::{ChannelRpcServer, ChannelRpcServerImpl};
#[cfg(debug_assertions)]
use ckb_types::core::TransactionView;
pub use config::RpcConfig;
#[cfg(debug_assertions)]
use dev::{DevRpcServer, DevRpcServerImpl};
use graph::{GraphRpcServer, GraphRpcServerImpl};
use info::InfoRpcServerImpl;
use invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
use jsonrpsee::server::{Server, ServerHandle};
use jsonrpsee::RpcModule;
use payment::PaymentRpcServerImpl;
use peer::{PeerRpcServer, PeerRpcServerImpl};
use ractor::ActorRef;
#[cfg(debug_assertions)]
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

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
pub async fn start_rpc<
    S: ChannelActorStateStore
        + InvoiceStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + Clone
        + Send
        + Sync
        + 'static,
>(
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
) -> ServerHandle {
    let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
    let server = build_server(listening_addr).await;
    let mut modules = RpcModule::new(());
    if config.is_module_enabled("invoice") {
        if network_actor.is_none() {
            warn!("network_actor should be set when invoice module is enabled");
        }
        modules
            .merge(
                InvoiceRpcServerImpl::new(store.clone(), network_actor.clone(), fiber_config)
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
                .merge(ChannelRpcServerImpl::new(network_actor.clone(), store.clone()).into_rpc())
                .unwrap();
        }

        if config.is_module_enabled("payment") {
            modules
                .merge(PaymentRpcServerImpl::new(network_actor.clone(), store.clone()).into_rpc())
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
    server.start(modules)
}
