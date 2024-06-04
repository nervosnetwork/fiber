mod cch;
mod channel;
mod invoice;
mod peer;
mod config;

use cch::{CchRpcServer, CchRpcServerImpl};
use channel::{ChannelRpcServer, ChannelRpcServerImpl};
use invoice::{InvoiceRpcServer, InvoiceRpcServerImpl};
use jsonrpsee::{server::{Server, ServerHandle}, Methods};
use peer::{PeerRpcServer, PeerRpcServerImpl};
use ractor::ActorRef;
use tokio::sync::mpsc::Sender;
use crate::{
    cch::CchCommand,
    ckb::NetworkActorMessage, invoice::InvoiceCommand,
};
pub use config::RpcConfig;

pub type InvoiceCommandWithReply = (InvoiceCommand, Sender<crate::Result<String>>);

pub async fn start_rpc(
    config: RpcConfig,
    ckb_network_actor: Option<ActorRef<NetworkActorMessage>>,
    cch_command_sender: Option<Sender<CchCommand>>,
    invoice_command_sender: Option<Sender<InvoiceCommandWithReply>>,
) -> ServerHandle {
    let listening_addr = config.listening_addr.as_deref().unwrap_or("[::]:0");
    let server = Server::builder().build(listening_addr).await.unwrap();
    let mut methods = Methods::new();
    if let Some(ckb_network_actor) = ckb_network_actor {
        let peer = PeerRpcServerImpl::new(ckb_network_actor.clone());
        let channel = ChannelRpcServerImpl::new(ckb_network_actor.clone());
        methods.merge(peer.into_rpc()).unwrap();
        methods.merge(channel.into_rpc()).unwrap();
    }
    if let Some(cch_command_sender) = cch_command_sender {
        let cch = CchRpcServerImpl::new(cch_command_sender);
        methods.merge(cch.into_rpc()).unwrap();
    }
    if let Some(invoice_command_sender) = invoice_command_sender {
        let invoice = InvoiceRpcServerImpl::new(invoice_command_sender);
        methods.merge(invoice.into_rpc()).unwrap();
    }
    server.start(methods)
}
