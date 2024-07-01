mod cch;
mod channel;
mod config;
mod invoice;
mod peer;
mod utils;

use crate::{
    cch::CchCommand,
    ckb::{channel::ChannelActorStateStore, NetworkActorMessage},
    invoice::{InvoiceCommand, InvoiceStore},
};
use cch::{CchRpcServer, CchRpcServerImpl};
use channel::{ChannelRpcServer, ChannelRpcServerImpl};
pub use config::RpcConfig;
use invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
use jsonrpsee::server::{Server, ServerHandle};
use peer::{PeerRpcServer, PeerRpcServerImpl};
use ractor::ActorRef;
use tokio::sync::mpsc::Sender;

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

pub async fn start_rpc<S: ChannelActorStateStore + InvoiceStore + Clone + Send + Sync + 'static>(
    config: RpcConfig,
    ckb_network_actor: Option<ActorRef<NetworkActorMessage>>,
    cch_command_sender: Option<Sender<CchCommand>>,
    store: S,
) -> ServerHandle {
    let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
    let server = build_server(listening_addr);
    let mut methods = InvoiceRpcServerImpl::new(store.clone()).into_rpc();
    if let Some(ckb_network_actor) = ckb_network_actor {
        let peer = PeerRpcServerImpl::new(ckb_network_actor.clone());
        let channel = ChannelRpcServerImpl::new(ckb_network_actor, store);
        methods.merge(peer.into_rpc()).unwrap();
        methods.merge(channel.into_rpc()).unwrap();
    }
    if let Some(cch_command_sender) = cch_command_sender {
        let cch = CchRpcServerImpl::new(cch_command_sender);
        methods.merge(cch.into_rpc()).unwrap();
    }
    server.start(methods)
}
