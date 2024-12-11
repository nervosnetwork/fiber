mod cch;
mod channel;
mod config;
mod graph;
mod info;
mod invoice;
mod peer;
mod utils;

use crate::fiber::gossip::GossipMessageStore;
use crate::rpc::info::InfoRpcServer;
use crate::{
    cch::CchMessage,
    fiber::{
        channel::ChannelActorStateStore,
        graph::{NetworkGraph, NetworkGraphStateStore},
        NetworkActorMessage,
    },
    invoice::{InvoiceCommand, InvoiceStore},
    FiberConfig,
};
use cch::{CchRpcServer, CchRpcServerImpl};
use channel::{ChannelRpcServer, ChannelRpcServerImpl};
pub use config::RpcConfig;
use graph::{GraphRpcServer, GraphRpcServerImpl};
use info::InfoRpcServerImpl;
use invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
use jsonrpsee::server::{Server, ServerHandle};
use peer::{PeerRpcServer, PeerRpcServerImpl};
use ractor::ActorRef;
use std::sync::Arc;
use tokio::sync::{mpsc::Sender, RwLock};

pub type InvoiceCommandWithReply = (InvoiceCommand, Sender<crate::Result<String>>);

fn build_server(addr: &str) -> Server {
    #[cfg(not(release))]
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
    #[cfg(release)]
    {
        Server::builder()
            .build(addr)
            .await
            .expect("JsonRPC server built")
    }
}

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
    fiber_config: Option<FiberConfig>,
    network_actor: Option<ActorRef<NetworkActorMessage>>,
    cch_actor: Option<ActorRef<CchMessage>>,
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
) -> ServerHandle {
    let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
    let server = build_server(listening_addr);
    let mut methods = InvoiceRpcServerImpl::new(store.clone(), fiber_config).into_rpc();
    if let Some(network_actor) = network_actor {
        let info = InfoRpcServerImpl::new(network_actor.clone(), store.clone());
        let peer = PeerRpcServerImpl::new(network_actor.clone());
        let channel = ChannelRpcServerImpl::new(network_actor, store.clone());
        let network_graph = GraphRpcServerImpl::new(network_graph, store.clone());
        methods.merge(info.into_rpc()).expect("add info RPC");
        methods.merge(peer.into_rpc()).expect("add peer RPC");
        methods.merge(channel.into_rpc()).expect("add channel RPC");
        methods
            .merge(network_graph.into_rpc())
            .expect("add network graph RPC");
    }
    if let Some(cch_actor) = cch_actor {
        let cch = CchRpcServerImpl::new(cch_actor);
        methods.merge(cch.into_rpc()).expect("add cch RPC");
    }
    server.start(methods)
}
