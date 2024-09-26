mod cch;
mod channel;
mod config;
mod graph;
mod invoice;
mod peer;
mod utils;

use std::sync::Arc;

use crate::{
    cch::CchMessage,
    fiber::{
        channel::ChannelActorStateStore,
        graph::{NetworkGraph, NetworkGraphStateStore},
        NetworkActorMessage,
    },
    invoice::{InvoiceCommand, InvoiceStore},
};
use cch::{CchRpcServer, CchRpcServerImpl};
use channel::{ChannelRpcServer, ChannelRpcServerImpl};
pub use config::RpcConfig;
use graph::{GraphRpcServer, GraphRpcServerImpl};
use invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
use jsonrpsee::server::{Server, ServerHandle};
use peer::{PeerRpcServer, PeerRpcServerImpl};
use ractor::ActorRef;
use tentacle::secio::PublicKey;
use tokio::sync::{mpsc::Sender, RwLock};

pub type InvoiceCommandWithReply = (InvoiceCommand, Sender<crate::Result<String>>);

fn build_server(addr: &str) -> Server {
    #[cfg(not(release))]
    {
        // Use socket2 to set reuse address and reuse port,
        // so that we can restart the server without waiting for the port to be released.
        // it will avoid the error: "Address already in use" in CI.
        use socket2::{Domain, Socket, Type};
        let addr = addr.parse().unwrap();
        let domain = Domain::for_address(addr);
        let socket = Socket::new(domain, Type::STREAM, None).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket.set_reuse_address(true).expect("set reuse address");
        socket.set_reuse_port(true).expect("set reuse port");

        socket.bind(&addr.into()).unwrap();
        socket.listen(4096).unwrap();

        jsonrpsee::server::Server::builder()
            .build_from_tcp(socket)
            .unwrap()
    }
    #[cfg(release)]
    {
        Server::builder().build(addr).await.unwrap()
    }
}

pub async fn start_rpc<
    S: ChannelActorStateStore + InvoiceStore + NetworkGraphStateStore + Clone + Send + Sync + 'static,
>(
    config: RpcConfig,
    network_actor: Option<ActorRef<NetworkActorMessage>>,
    cch_actor: Option<ActorRef<CchMessage>>,
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    node_publick_key: Option<PublicKey>,
) -> ServerHandle {
    let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
    let server = build_server(listening_addr);
    let mut methods = InvoiceRpcServerImpl::new(store.clone(), node_publick_key).into_rpc();
    if let Some(network_actor) = network_actor {
        let peer = PeerRpcServerImpl::new(network_actor.clone());
        let channel = ChannelRpcServerImpl::new(network_actor, store.clone());
        let network_graph = GraphRpcServerImpl::new(network_graph, store);
        methods.merge(peer.into_rpc()).unwrap();
        methods.merge(channel.into_rpc()).unwrap();
        methods.merge(network_graph.into_rpc()).unwrap();
    }
    if let Some(cch_actor) = cch_actor {
        let cch = CchRpcServerImpl::new(cch_actor);
        methods.merge(cch.into_rpc()).unwrap();
    }
    server.start(methods)
}
